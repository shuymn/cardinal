//! Disk states (`docs/architecture.md` section 10.4): the watched volumes, their readings, and
//! the transitions between `ok`, `warning`, and `critical`, kept in `disk/<volume>.json` and
//! written only under `reconcile.lock`, which the pass holds.
//!
//! A reading that fails is kept as a reading without sizes and changes nothing else: the
//! state, its time, and the pass's last successful reading stay as they were, and the failure
//! goes into the pass's errors and a `disk_state` record. So does every transition.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::config::Config;
use crate::contract::{
    DiskLevel, DiskLines, DiskMeasure, DiskReading, DiskState, DiskStateSchema, Inventory,
    RecordKind, ReportReading, ScopedError,
};
use crate::record::Recorder;
use crate::sensor::{self, Measure};
use crate::state::{self, StateDir};

use super::raise::Passed;
use super::{read_json, scoped, write_json};

/// Readings kept per volume.
pub const KEPT_READINGS: usize = 12;

/// The interval of the shipped timers (`examples/launchd`, `examples/systemd`). The prediction
/// looks two of them ahead.
pub const TIMER_INTERVAL_MS: u64 = 5 * 60 * 1000;

/// How far back the readings that predict the critical line go.
pub const PREDICTION_WINDOW_MS: u64 = 60 * 60 * 1000;

/// The measure a pass reads (section 10.4).
const MEASURE: Measure = Measure::Usable;

#[cfg(target_os = "macos")]
const MEASURE_KIND: DiskMeasure = DiskMeasure::ImportantUsage;
#[cfg(not(target_os = "macos"))]
const MEASURE_KIND: DiskMeasure = DiskMeasure::Statvfs;

/// A watched volume during a pass.
#[derive(Clone, Debug)]
pub struct Watched {
    /// The path the volume was found from, which reads it.
    pub path: PathBuf,
    pub state: DiskState,
    /// The pass's last successful reading.
    pub reading: Option<ReportReading>,
    /// What the pass's readings found, for an episode that ends within it.
    pub passed: Passed,
}

impl Watched {
    /// `warning` or `critical`.
    #[must_use]
    pub fn under_pressure(&self) -> bool {
        under_pressure(self.state.state)
    }
}

/// `warning` or `critical`.
#[must_use]
pub fn under_pressure(level: DiskLevel) -> bool {
    matches!(level, DiskLevel::Warning | DiskLevel::Critical)
}

/// Step 4: the volumes of the state dir and of each worktree in the last full observation,
/// each read once and its state updated.
pub fn read_watched(
    config: &Config,
    state: &StateDir,
    errors: &mut Vec<ScopedError>,
) -> Vec<Watched> {
    let mut watched: Vec<Watched> = Vec::new();
    for path in watched_paths(state, errors) {
        let volume = match sensor::volume(&path) {
            Ok(volume) => volume,
            Err(message) => {
                errors.push(scoped("disk", message));
                continue;
            }
        };
        let key = state::key(&volume.id);
        if watched.iter().any(|w| w.state.volume == key) {
            continue;
        }
        let lines = config.host.disk.lines_for(&volume.mount).to_bytes();
        let mut disk_state = load(state, &key, errors).unwrap_or_else(|| DiskState {
            schema: DiskStateSchema,
            volume: key,
            mount: volume.mount.display().to_string(),
            measure: MEASURE_KIND,
            state: DiskLevel::Unknown,
            since_ms: state::now_ms(),
            lines,
            readings: Vec::new(),
        });
        disk_state.mount = volume.mount.display().to_string();
        disk_state.lines = lines;
        let mut volume = Watched {
            path,
            state: disk_state,
            reading: None,
            passed: Passed::default(),
        };
        read(config, state, &mut volume, errors);
        watched.push(volume);
    }
    watched
}

/// Reads `volume` once and keeps the reading; a successful one updates its state and becomes
/// the pass's last successful reading. Writes `disk/<volume>.json`. Whether the read
/// succeeded.
pub fn read(
    config: &Config,
    state: &StateDir,
    volume: &mut Watched,
    errors: &mut Vec<ScopedError>,
) -> bool {
    let at_ms = state::now_ms();
    let scope = format!("disk:{}", volume.state.mount);
    let read = sensor::free_space(&volume.path, MEASURE);
    let from = volume.state.state;
    volume.state.readings.push(DiskReading {
        at_ms,
        free_bytes: read.as_ref().ok().map(|s| s.free_bytes),
        total_bytes: read.as_ref().ok().map(|s| s.total_bytes),
    });
    let excess = volume.state.readings.len().saturating_sub(KEPT_READINGS);
    volume.state.readings.drain(..excess);
    let recorder = Recorder::new(
        state,
        config.host.records.max_bytes,
        config.host.records.keep,
    );
    match read {
        Ok(space) => {
            let free = space.free_bytes;
            let predicted = predicts_critical(&volume.state.readings, at_ms, &volume.state.lines);
            let to = next(from, free, &volume.state.lines, predicted);
            if to != from {
                volume.state.state = to;
                volume.state.since_ms = at_ms;
                if under_pressure(to) {
                    volume.passed.low_ms.get_or_insert(at_ms);
                }
                if to == DiskLevel::Critical {
                    volume.passed.critical_ms.get_or_insert(at_ms);
                }
                record(
                    &recorder,
                    &volume.state,
                    json!({ "from": from, "to": to, "free_bytes": free, "predicted": predicted }),
                );
            }
            volume.reading = Some(ReportReading {
                at_ms,
                free_bytes: free,
            });
        }
        Err(message) => {
            record(
                &recorder,
                &volume.state,
                json!({ "from": from, "to": from, "free_bytes": null, "error": message }),
            );
            errors.push(scoped(&scope, message));
        }
    }
    let path = state.disk_state(&volume.state.volume);
    write_json(state, &path, &volume.state, &scope, errors);
    volume
        .state
        .readings
        .last()
        .is_some_and(|r| r.free_bytes.is_some())
}

/// Step 4, last part (section 9.7, "Space"): writes `<state>/reserve` when it is missing and
/// the state dir's volume, `disks`'s volume read from the state dir, read above its recovery
/// line plus the reserve's size in this pass, so writing it never puts the volume under
/// pressure. The file is written block by block, never sparse, synced, and renamed into
/// place; a write that fails leaves no reserve and is reported in `errors`.
pub fn write_reserve(
    config: &Config,
    state: &StateDir,
    disks: &[Watched],
    errors: &mut Vec<ScopedError>,
) {
    let size = config.host.disk.reserve_bytes();
    let Some(volume) = disks.iter().find(|d| d.path == state.root()) else {
        return;
    };
    let Some(reading) = volume.reading else {
        return;
    };
    let line = volume.state.lines.recovery_bytes.saturating_add(size);
    if reading.free_bytes <= line {
        return;
    }
    let path = state.reserve_file();
    match std::fs::symlink_metadata(&path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => return,
        Err(err) => {
            errors.push(scoped("reserve", format!("{}: {err}", path.display())));
            return;
        }
    }
    if let Err(err) = state::write_reserve(&state.tmp_dir(), &path, size) {
        errors.push(scoped("reserve", format!("{}: {err}", path.display())));
    }
}

/// The state a successful reading of `free` bytes leads to from `current` (section 10.4):
/// below the critical line `critical`, a first reading included; from `ok` below the warning
/// line or when `predicted`, `warning`; out of `critical` above the critical line, `warning`;
/// out of pressure only above the recovery line. A reading on a line crosses nothing.
#[must_use]
pub fn next(current: DiskLevel, free: u64, lines: &DiskLines, predicted: bool) -> DiskLevel {
    let below_critical = free < lines.critical_bytes;
    match current {
        DiskLevel::Ok | DiskLevel::Unknown if below_critical => DiskLevel::Critical,
        DiskLevel::Ok | DiskLevel::Unknown if free < lines.warning_bytes || predicted => {
            DiskLevel::Warning
        }
        DiskLevel::Ok | DiskLevel::Unknown => DiskLevel::Ok,
        DiskLevel::Warning | DiskLevel::Critical if free > lines.recovery_bytes => DiskLevel::Ok,
        DiskLevel::Warning if below_critical => DiskLevel::Critical,
        DiskLevel::Warning => DiskLevel::Warning,
        DiskLevel::Critical if free > lines.critical_bytes => DiskLevel::Warning,
        DiskLevel::Critical => DiskLevel::Critical,
    }
}

/// Whether the successful readings of the last hour, the newest at `now_ms` included, reach
/// the critical line within two timer intervals at the rate free space fell between the first
/// and the last of them.
#[must_use]
pub fn predicts_critical(readings: &[DiskReading], now_ms: u64, lines: &DiskLines) -> bool {
    let hour: Vec<(u64, u64)> = readings
        .iter()
        .filter(|r| r.at_ms.saturating_add(PREDICTION_WINDOW_MS) >= now_ms)
        .filter_map(|r| r.free_bytes.map(|free| (r.at_ms, free)))
        .collect();
    let (Some(&(first_ms, first)), Some(&(last_ms, last))) = (hour.first(), hour.last()) else {
        return false;
    };
    if last_ms <= first_ms || last >= first {
        return false;
    }
    // room / (fallen / elapsed) <= 2 intervals, in integers.
    let room = u128::from(last.saturating_sub(lines.critical_bytes));
    let fallen = u128::from(first - last);
    let elapsed = u128::from(last_ms - first_ms);
    room * elapsed <= fallen * 2 * u128::from(TIMER_INTERVAL_MS)
}

/// The state dir, then each worktree of `reconcile/observation.json`.
fn watched_paths(state: &StateDir, errors: &mut Vec<ScopedError>) -> Vec<PathBuf> {
    let mut paths = vec![state.root().to_path_buf()];
    let inventory: Option<Inventory> = read_json(&state.reconcile_observation(), "disk", errors);
    for repo in inventory.iter().flat_map(|i| &i.repos) {
        paths.extend(repo.worktrees.iter().map(|w| PathBuf::from(&w.path)));
    }
    paths
}

/// The state `disk/<volume>.json` holds; `None` when there is none or it cannot be read,
/// which is reported in `errors` and starts the volume afresh.
fn load(state: &StateDir, key: &str, errors: &mut Vec<ScopedError>) -> Option<DiskState> {
    read_json(&state.disk_state(key), "disk", errors)
}

/// A `disk_state` record for `volume` with `data` and the volume's key, mount, and lines.
fn record(recorder: &Recorder, volume: &DiskState, data: serde_json::Value) {
    let serde_json::Value::Object(mut map) = data else {
        return;
    };
    let mut record = recorder.record(RecordKind::DiskState);
    map.insert("volume".into(), volume.volume.clone().into());
    map.insert("mount".into(), volume.mount.clone().into());
    map.insert(
        "lines".into(),
        serde_json::to_value(volume.lines).unwrap_or_default(),
    );
    record.data = map;
    let _ = recorder.append(&record);
}

/// The volume key of `path`, as [`read_watched`] keys it.
pub fn key_of(path: &Path) -> Option<String> {
    sensor::volume(path).ok().map(|v| state::key(&v.id))
}

/// The volume key of the entry at `path` itself, a final symbolic link not followed.
pub fn entry_key_of(path: &Path) -> Option<String> {
    sensor::entry_volume(path).ok().map(|v| state::key(&v.id))
}

#[cfg(test)]
mod tests {
    use super::{DiskLevel, DiskLines, DiskReading, TIMER_INTERVAL_MS, next, predicts_critical};

    const LINES: DiskLines = DiskLines {
        warning_bytes: 200,
        recovery_bytes: 300,
        critical_bytes: 50,
    };

    #[test]
    fn transitions_follow_the_lines() {
        use DiskLevel::{Critical, Ok, Unknown, Warning};
        for (from, free, to) in [
            (Unknown, 49, Critical),
            (Unknown, 100, Warning),
            (Unknown, 250, Ok),
            (Ok, 200, Ok),
            (Ok, 199, Warning),
            (Ok, 49, Critical),
            (Warning, 250, Warning),
            (Warning, 300, Warning),
            (Warning, 301, Ok),
            (Warning, 49, Critical),
            (Critical, 50, Critical),
            (Critical, 51, Warning),
            (Critical, 49, Critical),
            (Critical, 301, Ok),
        ] {
            assert_eq!(next(from, free, &LINES, false), to, "{from:?} at {free}");
        }
        assert_eq!(next(Ok, 250, &LINES, true), Warning);
        assert_eq!(next(Warning, 250, &LINES, true), Warning);
        assert_eq!(next(Critical, 250, &LINES, true), Warning);
    }

    fn reading(at_ms: u64, free: Option<u64>) -> DiskReading {
        DiskReading {
            at_ms,
            free_bytes: free,
            total_bytes: Some(1000),
        }
    }

    #[test]
    fn the_last_hour_predicts_the_critical_line() {
        let hour = 60 * 60 * 1000;
        let now = 10 * hour;
        let step = TIMER_INTERVAL_MS;
        // 50 bytes per interval: 100 bytes above critical is two intervals away.
        let falling = [
            reading(now - 2 * step, Some(250)),
            reading(now - step, None),
            reading(now, Some(150)),
        ];
        assert!(predicts_critical(&falling, now, &LINES));
        let slower = [reading(now - 2 * step, Some(240)), reading(now, Some(150))];
        assert!(!predicts_critical(&slower, now, &LINES));
        // A fall older than an hour does not count.
        let old = [reading(now - hour - 1, Some(900)), reading(now, Some(150))];
        assert!(!predicts_critical(&old, now, &LINES));
        let rising = [reading(now - step, Some(150)), reading(now, Some(250))];
        assert!(!predicts_critical(&rising, now, &LINES));
        assert!(!predicts_critical(&[reading(now, Some(60))], now, &LINES));
    }
}
