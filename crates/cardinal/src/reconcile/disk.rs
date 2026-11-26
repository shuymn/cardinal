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

/// Readings kept per volume in its disk state.
pub const KEPT_READINGS: usize = 12;

/// The bytes a volume's readings history file reaches before it is rotated (section 10.4):
/// the history keeps at least this much and at most twice it, about 13,000 readings of about
/// 80 bytes: about 45 days at the 5-minute timer and about 7 days at a reading every 45 s.
pub const HISTORY_FILE_BYTES: u64 = 1024 * 1024;

/// The interval of the shipped timers (`examples/launchd`, `examples/systemd`). The prediction
/// looks two of them ahead.
pub const TIMER_INTERVAL_MS: u64 = 5 * 60 * 1000;

/// How far back the readings that predict the critical line go: two timer intervals, so the
/// rate is that of the last interval or two and an earlier rise does not dilute it
/// (section 10.4).
pub const PREDICTION_WINDOW_MS: u64 = 2 * TIMER_INTERVAL_MS;

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
            predicted_only: false,
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
            let (to, predicted_only) = next(
                from,
                volume.state.predicted_only,
                free,
                &volume.state.lines,
                predicted,
            );
            volume.state.predicted_only = predicted_only;
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
                    json!({
                        "from": from,
                        "to": to,
                        "free_bytes": free,
                        "predicted": predicted,
                        "predicted_only": predicted_only,
                    }),
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
    let saved = write_json(state, &path, &volume.state, &scope, errors);
    let succeeded = volume
        .state
        .readings
        .last()
        .filter(|r| r.free_bytes.is_some());
    // The history takes a reading only once the state that saw it is saved, so a pass that
    // stops between the two loses at most a history line and the history never holds a
    // reading the next pass's state has not seen.
    if saved && let Some(reading) = succeeded {
        crate::testpoint::hit(crate::testpoint::RECONCILE_DISK_WRITTEN).ok();
        let files = state.disk_readings(&volume.state.volume);
        if let Err(err) = append_history(&files, reading, HISTORY_FILE_BYTES) {
            errors.push(scoped(&scope, format!("{}: {err}", files.0.display())));
        }
    }
    succeeded.is_some()
}

/// Appends `reading` as a line to the history file `files.0`, first renaming it over
/// `files.1` when the line would take it past `max_bytes`, so the history keeps between
/// `max_bytes` and twice that, the oldest readings dropped. A torn last line is skipped on
/// read, and the next line starts a line of its own. Each file is opened without following a
/// final symbolic link or blocking and must be a regular file, and so must `files.1` be when
/// it exists, so a FIFO or a link there is an error and never written through. Called under
/// `reconcile.lock`, the history's one writer.
///
/// # Errors
///
/// When the directory, a file, the rename, or the append fails, or a file is not a regular
/// one.
pub fn append_history(
    files: &(PathBuf, PathBuf),
    reading: &DiskReading,
    max_bytes: u64,
) -> std::io::Result<()> {
    let (current, rotated) = files;
    let line = serde_json::to_vec(reading).map_err(std::io::Error::other)?;
    if let Some(dir) = current.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = open_append(current)?;
    let size = file.metadata()?.len();
    if size > 0 && size.saturating_add(line.len() as u64 + 1) > max_bytes {
        match std::fs::symlink_metadata(rotated) {
            Ok(meta) if !meta.is_file() => {
                return Err(std::io::Error::other(format!(
                    "{}: not a regular file",
                    rotated.display()
                )));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        drop(file);
        std::fs::rename(current, rotated)?;
        file = open_append(current)?;
    }
    state::append_line(&file, &line)
}

/// `path` opened to append, created if missing, as a regular file only (`os::open_regular`).
fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    crate::os::open_regular(
        std::fs::OpenOptions::new()
            .append(true)
            .read(true)
            .create(true),
        path,
    )
}

/// The readings of the history `files`, oldest first; lines that do not parse, a torn last
/// one included, are skipped, and a missing file reads as empty.
///
/// # Errors
///
/// When a file exists but cannot be read or is not a regular file.
pub fn read_history(files: &(PathBuf, PathBuf)) -> std::io::Result<Vec<DiskReading>> {
    let mut readings = Vec::new();
    for path in [&files.1, &files.0] {
        let mut file = match crate::os::open_regular(std::fs::OpenOptions::new().read(true), path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)?;
        readings.extend(state::parse_jsonl::<DiskReading>(&bytes));
    }
    Ok(readings)
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

/// The state a successful reading of `free` bytes leads to from `current` (section 10.4), and
/// whether the episode is still `predicted_only`: below the critical line `critical`, a first
/// reading included; from `ok` below the warning line or when `predicted` is `Some(true)`,
/// `warning`, only by prediction when the reading is not below the warning line; out of
/// `critical` above the critical line, `warning`; out of pressure above the recovery line, or,
/// for an episode that entered `warning` only by prediction and has never been below the
/// warning line, at a reading above the warning line that no longer predicts (`Some(false)`).
/// A prediction without enough readings (`None`) enters nothing and clears nothing. A reading
/// below the warning line ends `predicted_only` for the rest of the episode. A reading on a
/// line crosses nothing.
#[must_use]
pub fn next(
    current: DiskLevel,
    predicted_only: bool,
    free: u64,
    lines: &DiskLines,
    predicted: Option<bool>,
) -> (DiskLevel, bool) {
    let below_critical = free < lines.critical_bytes;
    let below_warning = free < lines.warning_bytes;
    let to = match current {
        DiskLevel::Ok | DiskLevel::Unknown if below_critical => DiskLevel::Critical,
        DiskLevel::Ok | DiskLevel::Unknown if below_warning || predicted == Some(true) => {
            DiskLevel::Warning
        }
        DiskLevel::Ok | DiskLevel::Unknown => DiskLevel::Ok,
        DiskLevel::Warning | DiskLevel::Critical if free > lines.recovery_bytes => DiskLevel::Ok,
        DiskLevel::Warning if below_critical => DiskLevel::Critical,
        DiskLevel::Warning
            if predicted_only && free > lines.warning_bytes && predicted == Some(false) =>
        {
            DiskLevel::Ok
        }
        DiskLevel::Warning => DiskLevel::Warning,
        DiskLevel::Critical if free > lines.critical_bytes => DiskLevel::Warning,
        DiskLevel::Critical => DiskLevel::Critical,
    };
    let still = match current {
        DiskLevel::Ok | DiskLevel::Unknown => true,
        DiskLevel::Warning | DiskLevel::Critical => predicted_only,
    };
    (to, to == DiskLevel::Warning && still && !below_warning)
}

/// Whether the successful readings of the last two timer intervals, the newest at `now_ms`
/// from this pass, reach the critical line within two timer intervals at the rate free space
/// fell between the oldest and the newest of them; `None` when they cannot tell, the newest
/// not this pass's or none at least one interval before it (the state's readings filled by
/// one pass's, or a gap longer than the window).
#[must_use]
pub fn predicts_critical(readings: &[DiskReading], now_ms: u64, lines: &DiskLines) -> Option<bool> {
    let window: Vec<(u64, u64)> = readings
        .iter()
        .filter(|r| r.at_ms.saturating_add(PREDICTION_WINDOW_MS) >= now_ms)
        .filter_map(|r| r.free_bytes.map(|free| (r.at_ms, free)))
        .collect();
    let (&(first_ms, first), &(last_ms, last)) = (window.first()?, window.last()?);
    if last_ms != now_ms || last_ms.saturating_sub(first_ms) < TIMER_INTERVAL_MS {
        return None;
    }
    if last >= first {
        return Some(false);
    }
    // room / (fallen / elapsed) <= 2 intervals, in integers.
    let room = u128::from(last.saturating_sub(lines.critical_bytes));
    let fallen = u128::from(first - last);
    let elapsed = u128::from(last_ms - first_ms);
    Some(room * elapsed <= fallen * 2 * u128::from(TIMER_INTERVAL_MS))
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
    use super::{
        DiskLevel, DiskLines, DiskReading, HISTORY_FILE_BYTES, KEPT_READINGS, TIMER_INTERVAL_MS,
        append_history, next, predicts_critical, read_history,
    };

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
            assert_eq!(
                next(from, false, free, &LINES, Some(false)),
                (to, false),
                "{from:?} at {free}"
            );
        }
        assert_eq!(next(Ok, false, 250, &LINES, Some(true)), (Warning, true));
        assert_eq!(next(Ok, false, 200, &LINES, Some(true)), (Warning, true));
        assert_eq!(next(Ok, false, 150, &LINES, Some(true)), (Warning, false));
        assert_eq!(
            next(Warning, false, 250, &LINES, Some(true)),
            (Warning, false)
        );
        assert_eq!(
            next(Critical, false, 250, &LINES, Some(true)),
            (Warning, false)
        );
    }

    /// An episode that entered `warning` only by prediction ends at a reading above the
    /// warning line that no longer predicts; one that has been below the warning line keeps
    /// the recovery line, however its trend turns.
    #[test]
    fn prediction_only_warning_clears_above_the_warning_line() {
        use DiskLevel::{Critical, Ok, Warning};
        // Prediction only: clears above the warning line once the trend reverses.
        assert_eq!(next(Warning, true, 201, &LINES, Some(false)), (Ok, false));
        // Readings that cannot tell the rate enter nothing and clear nothing.
        assert_eq!(next(Warning, true, 201, &LINES, None), (Warning, true));
        assert_eq!(next(Ok, false, 201, &LINES, None), (Ok, false));
        assert_eq!(next(Ok, false, 199, &LINES, None), (Warning, false));
        // Still predicting, or on the line: stays, still by prediction only.
        assert_eq!(
            next(Warning, true, 250, &LINES, Some(true)),
            (Warning, true)
        );
        assert_eq!(
            next(Warning, true, 200, &LINES, Some(false)),
            (Warning, true)
        );
        // Below the warning line: the episode keeps the recovery line from then on.
        assert_eq!(
            next(Warning, true, 199, &LINES, Some(false)),
            (Warning, false)
        );
        assert_eq!(
            next(Warning, false, 250, &LINES, Some(false)),
            (Warning, false)
        );
        assert_eq!(
            next(Warning, true, 49, &LINES, Some(false)),
            (Critical, false)
        );
        assert_eq!(
            next(Critical, false, 250, &LINES, Some(false)),
            (Warning, false)
        );
        // Above the recovery line every episode ends.
        assert_eq!(next(Warning, true, 301, &LINES, Some(true)), (Ok, false));
    }

    fn reading(at_ms: u64, free: Option<u64>) -> DiskReading {
        DiskReading {
            at_ms,
            free_bytes: free,
            total_bytes: Some(1000),
        }
    }

    #[test]
    fn the_last_two_intervals_predict_the_critical_line() {
        let now = 10 * 60 * 60 * 1000;
        let step = TIMER_INTERVAL_MS;
        // 50 bytes per interval: 100 bytes above critical is two intervals away.
        let falling = [
            reading(now - 2 * step, Some(250)),
            reading(now - step, None),
            reading(now, Some(150)),
        ];
        assert_eq!(predicts_critical(&falling, now, &LINES), Some(true));
        let slower = [reading(now - 2 * step, Some(240)), reading(now, Some(150))];
        assert_eq!(predicts_critical(&slower, now, &LINES), Some(false));
        // A fall older than two intervals does not count: the readings cannot tell.
        let old = [
            reading(now - 2 * step - 1, Some(900)),
            reading(now, Some(150)),
        ];
        assert_eq!(predicts_critical(&old, now, &LINES), None);
        let rising = [reading(now - step, Some(150)), reading(now, Some(250))];
        assert_eq!(predicts_critical(&rising, now, &LINES), Some(false));
        assert_eq!(
            predicts_critical(&[reading(now, Some(60))], now, &LINES),
            None
        );
        // Readings less than one interval apart cannot tell, however fast they fall.
        let close = [reading(now - step + 1, Some(250)), reading(now, Some(60))];
        assert_eq!(predicts_critical(&close, now, &LINES), None);
        // The newest successful reading must be this pass's.
        let stale = [
            reading(now - 2 * step, Some(250)),
            reading(now - 1, Some(150)),
        ];
        assert_eq!(predicts_critical(&stale, now, &LINES), None);
    }

    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

    /// The readings of a transient writer, as seconds from the first and GiB free, at the
    /// shipped lines (warning 20, recovery 30, critical 5 GiB): a rise of 40 GiB at 601 s,
    /// then 13–14 GiB lost per interval. Taken from one host's free space.
    const WRITER: [(u64, f64); 7] = [
        (0, 35.02),
        (300, 21.63),
        (601, 61.07),
        (901, 56.74),
        (1202, 43.70),
        (1506, 29.68),
        (1870, 19.03),
    ];

    fn gib_lines() -> DiskLines {
        let gib = |n: u64| n * 1024 * 1024 * 1024;
        DiskLines {
            warning_bytes: gib(20),
            recovery_bytes: gib(30),
            critical_bytes: gib(5),
        }
    }

    /// The states the readings lead to, each pass predicting from the readings up to its own.
    fn levels(readings: &[(u64, f64)]) -> Vec<DiskLevel> {
        let lines = gib_lines();
        let mut kept = Vec::new();
        let (mut level, mut predicted_only) = (DiskLevel::Ok, false);
        let mut levels = Vec::new();
        for &(s, gib) in readings {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "fixture values are small and positive"
            )]
            let free = (gib * GIB) as u64;
            let at = 1_000_000 + s * 1000;
            kept.push(reading(at, Some(free)));
            let predicted = predicts_critical(&kept, at, &lines);
            (level, predicted_only) = next(level, predicted_only, free, &lines, predicted);
            levels.push(level);
        }
        levels
    }

    /// At 1506 s, 29.68 GiB free and 14.02 GiB lost in the last 304 s, the critical line is
    /// 535 s away: the rule enters `warning` there, a pass before the reading below the
    /// warning line. A rate over the hour, diluted by the rise at 601 s, did not. The
    /// prediction at 300 s clears at the rise, since that episode was never below the line.
    #[test]
    fn a_transient_writer_is_predicted_a_pass_early() {
        use DiskLevel::{Ok, Warning};
        assert_eq!(levels(&WRITER), [Ok, Warning, Ok, Ok, Ok, Warning, Warning]);
    }

    /// At the 5-minute timer, a week of readings of a large volume, with the clock's values
    /// given, stays whole within the cap; past it the oldest are dropped, and the files hold
    /// between the cap and twice it.
    #[test]
    fn the_history_keeps_a_week_within_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let files = (dir.path().join("v.jsonl"), dir.path().join("v.1.jsonl"));
        let start: u64 = 1_795_000_000_000;
        let week = 7 * 24 * 60 * 60 * 1000 / TIMER_INTERVAL_MS;
        let at = |n: u64| DiskReading {
            at_ms: start + n * TIMER_INTERVAL_MS,
            free_bytes: Some(494_384_795_648 - n * 1_000_003),
            total_bytes: Some(494_384_795_648),
        };
        let size = || {
            [&files.0, &files.1]
                .iter()
                .map(|p| std::fs::metadata(p).map_or(0, |m| m.len()))
                .sum::<u64>()
        };
        for n in 0..=week {
            append_history(&files, &at(n), HISTORY_FILE_BYTES).unwrap();
        }
        let kept = read_history(&files).unwrap();
        assert_eq!(kept.len() as u64, week + 1);
        assert_eq!(kept[0], at(0));
        assert_eq!(*kept.last().unwrap(), at(week));
        assert!(size() <= HISTORY_FILE_BYTES, "{}", size());

        // Far past a smaller cap: between it and twice it is kept, the newest, in order.
        let small = 16 * 1024;
        let files = (dir.path().join("s.jsonl"), dir.path().join("s.1.jsonl"));
        let size = || {
            [&files.0, &files.1]
                .iter()
                .map(|p| std::fs::metadata(p).map_or(0, |m| m.len()))
                .sum::<u64>()
        };
        let last = 2000;
        for n in 0..=last {
            append_history(&files, &at(n), small).unwrap();
            if n > 1000 {
                assert!((small..=2 * small).contains(&size()), "{n}: {}", size());
            }
        }
        let kept = read_history(&files).unwrap();
        assert!(kept[0].at_ms > at(0).at_ms);
        assert_eq!(*kept.last().unwrap(), at(last));
        assert!(
            kept.windows(2)
                .all(|w| w[1].at_ms == w[0].at_ms + TIMER_INTERVAL_MS)
        );
    }

    /// The levels and marks of byte readings `(at_ms, free)`, each read as `read` reads it:
    /// kept, the state's last [`KEPT_READINGS`] only, then predicted from and applied.
    fn walk(readings: &[(u64, u64)]) -> Vec<(DiskLevel, bool)> {
        let mut kept: Vec<DiskReading> = Vec::new();
        let (mut level, mut mark) = (DiskLevel::Unknown, false);
        readings
            .iter()
            .map(|&(at, free)| {
                kept.push(reading(at, Some(free)));
                let excess = kept.len().saturating_sub(KEPT_READINGS);
                kept.drain(..excess);
                let predicted = predicts_critical(&kept, at, &LINES);
                (level, mark) = next(level, mark, free, &LINES, predicted);
                (level, mark)
            })
            .collect()
    }

    /// A falling `warning` entered by prediction alone stays while a granted pass's readings
    /// after each `act`, seconds apart, push the previous pass's out of the state's 12, so the
    /// readings can no longer tell the rate.
    #[test]
    fn readings_within_a_pass_keep_a_predicted_warning() {
        use DiskLevel::{Ok, Warning};
        let s = 1000;
        // 600 at -400 s, 290 at 0: the critical line 310 s away; then 1 lower every 8 s.
        let mut readings = vec![(10_000 * s, 600), (10_400 * s, 290)];
        readings.extend((1..=12).map(|n| (10_400 * s + n * 8 * s, 290 - n)));
        let walked = walk(&readings);
        assert_eq!(walked[0], (Ok, false));
        assert!(
            walked[1..].iter().all(|&w| w == (Warning, true)),
            "{walked:?}"
        );
    }

    /// A reading above the warning line after a gap longer than the window, with nothing
    /// within it before, keeps a `warning` entered by prediction alone.
    #[test]
    fn a_gap_keeps_a_predicted_warning() {
        let s = 1000;
        let gap = walk(&[(10_000 * s, 600), (10_400 * s, 290), (14_000 * s, 250)]);
        assert_eq!(gap[2], (DiskLevel::Warning, true), "{gap:?}");
    }
}
