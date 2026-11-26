//! The reconcile and disk tests' setup (`docs/architecture.md` sections 10.3, 10.4, and
//! 13.6): the plan tests' repository, configured as the one `reconcile` observes, disk lines
//! in MiB, and the readings fake in a test directory of the setup's own, which also holds the
//! pause points.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of the helpers"
)]

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, SystemTime};

use cardinal::contract::{
    ArbiterPass, ArbiterStart, DiskReport, DiskState, InboxItem, InboxKind, InboxStatus, Reconcile,
    Record, RecordKind, ScopedError,
};
use cardinal::inbox::StatusFilter;
use cardinal::state::{self, Probe};
use cardinal_test_support::Command;
use cardinal_test_support::process::wait_for;

use crate::plan_support::{RUN_LIMIT, Setup};
use crate::support::testpoints::TestPoints;

pub const MIB: u64 = 1024 * 1024;

/// The total size the readings fake reports.
const TOTAL: u64 = 64 * MIB;

/// Scopes of the pass's steps whose bodies other units write (`docs/architecture.md` section
/// 14.2): their errors say nothing about the steps these tests check.
const OTHER_UNITS: &[&str] = &[];

pub struct Disk {
    pub s: Setup,
    pub points: TestPoints,
}

impl Disk {
    /// Lines in MiB as `(critical, warning, recovery)`.
    pub fn new(lines: (u64, u64, u64)) -> Self {
        Self::with_host(lines, "")
    }

    /// [`Disk::new`] with more `host.toml` after the `[disk]` table: other tables.
    pub fn with_host(lines: (u64, u64, u64), extra: &str) -> Self {
        let mut s = Setup::new();
        let main = s.main.display().to_string();
        s.scene
            .observe_config(&format!("repos = ['{main}']\n{}", host(lines, extra)));
        let points = TestPoints::new(s.fx.scope());
        Self { s, points }
    }

    /// Makes every path under `prefix` a volume of its own whose reads take `free` in turn,
    /// the last one repeating; a `None` is a read that fails.
    pub fn readings(&self, volumes: &[(&Path, &[Option<u64>])]) {
        let volumes: Vec<serde_json::Value> = volumes
            .iter()
            .map(|(prefix, free)| {
                serde_json::json!({ "prefix": prefix, "free": free, "total": TOTAL })
            })
            .collect();
        std::fs::write(
            self.points.dir().join("disk-readings.json"),
            serde_json::to_vec(&serde_json::json!({ "volumes": volumes })).unwrap(),
        )
        .unwrap();
    }

    /// [`Disk::readings`] with the whole fixture, the state dir included, as one volume.
    pub fn fixture_reads(&self, free: &[Option<u64>]) {
        self.readings(&[(self.s.fx.root(), free)]);
    }

    /// `cardinal reconcile --trigger manual` with the test directory set.
    pub fn reconcile_command(&self) -> Command {
        reconcile_command(&self.s, &self.points)
    }

    /// One pass, which must succeed; when it started an arbiter pass, once that pass has
    /// ended, so neither the test's checks nor its next pass race the launcher's writes to
    /// the items it assigned. On Windows, where a test's every child runs in a job that does
    /// not let a process leave it, the trigger starts none and records none (see
    /// [`refused_start`]).
    pub fn reconcile(&self) -> Reconcile {
        reconcile(&self.s, &self.points)
    }

    /// The arbiter pass a `reconcile` pass started at or after `since_ms` started, once it has
    /// ended and its launcher has let go of `arbiter/.lock`. The setup's `[arbiter] command`
    /// exits at once.
    pub fn arbiter_ended(&self, since_ms: u64) -> ArbiterPass {
        arbiter_ended(&self.s, since_ms)
    }

    /// The disk state files, in file name order.
    pub fn states(&self) -> Vec<DiskState> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(self.s.state.disk_dir())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        paths.sort();
        paths
            .iter()
            .map(|p| serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap())
            .collect()
    }

    /// The one disk state file.
    pub fn state(&self) -> DiskState {
        let states = self.states();
        assert_eq!(states.len(), 1, "{states:#?}");
        states.into_iter().next().unwrap()
    }

    /// Moves the one disk state's readings and `since_ms` `by_ms` into the past, as if the
    /// passes so far had run that long ago: the timer's interval without waiting for it.
    pub fn age(&self, by_ms: u64) {
        let mut state = self.state();
        state.since_ms -= by_ms;
        for reading in &mut state.readings {
            reading.at_ms -= by_ms;
        }
        std::fs::write(
            self.s.state.disk_state(&state.volume),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
    }

    /// The one volume's readings history, oldest first.
    pub fn history(&self) -> Vec<cardinal::contract::DiskReading> {
        let files = self.s.state.disk_readings(&self.state().volume);
        cardinal::reconcile::read_history(&files).unwrap()
    }

    /// Every `disk_low` and `disk_critical` item, each occurrence its own, `disk_low` first.
    pub fn disk_items(&self) -> Vec<InboxItem> {
        let mut items: Vec<InboxItem> = cardinal::inbox::list(
            &self.s.config(),
            &self.s.state,
            Some(StatusFilter::All),
            false,
        )
        .unwrap()
        .items
        .into_iter()
        .filter(|i| matches!(i.kind, InboxKind::DiskLow | InboxKind::DiskCritical))
        .collect();
        items.sort_by_key(|i| (i.kind == InboxKind::DiskCritical, i.created_ms));
        items
    }

    /// The status of the latest `disk_low` and `disk_critical` occurrence, `None` for one
    /// never raised.
    pub fn disk_statuses(&self) -> (Option<InboxStatus>, Option<InboxStatus>) {
        let items = self.disk_items();
        let latest = |kind: InboxKind| items.iter().rfind(|i| i.kind == kind).map(|i| i.status);
        (latest(InboxKind::DiskLow), latest(InboxKind::DiskCritical))
    }

    pub fn records(&self, kind: RecordKind) -> Vec<Record> {
        cardinal::record::read_all(&self.s.state)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == kind)
            .collect()
    }
}

/// `host.toml` for the disk tests, after the `[observe]` table: lines in MiB as
/// `(critical, warning, recovery)`, a reserve of 1 MiB, an arbiter command that does nothing
/// should a pass start one, then `extra`, more tables.
pub fn host(lines: (u64, u64, u64), extra: &str) -> String {
    let (critical, warning, recovery) = lines;
    format!(
        "[disk]\ncritical_mib = {critical}\nwarning_mib = {warning}\nrecovery_mib = {recovery}\n\
         reserve_mib = 1\n[arbiter]\ncommand = ['true']\n{extra}"
    )
}

/// `cardinal reconcile --trigger manual` for `s`, with the test directory of `points` set.
pub fn reconcile_command(s: &Setup, points: &TestPoints) -> Command {
    let mut command = s.cardinal();
    command.args(["reconcile", "--trigger", "manual"]);
    points.apply(&mut command, &[], None);
    command
}

/// One pass for `s`, as [`Disk::reconcile`] runs it.
pub fn reconcile(s: &Setup, points: &TestPoints) -> Reconcile {
    let since_ms = state::now_ms();
    let pass = pass(
        &reconcile_command(s, points)
            .output_within(RUN_LIMIT)
            .expect("run cardinal reconcile"),
    );
    if cfg!(windows) {
        assert_ne!(pass.arbiter, ArbiterStart::Started, "{pass:#?}");
        assert!(!s.state.arbiter_last().exists(), "{pass:#?}");
    } else if pass.arbiter == ArbiterStart::Started {
        arbiter_ended(s, since_ms);
    }
    pass
}

/// The arbiter pass of `s` a `reconcile` pass started at or after `since_ms`, once it has
/// ended and its launcher has let go of `arbiter/.lock`.
pub fn arbiter_ended(s: &Setup, since_ms: u64) -> ArbiterPass {
    wait_for("the arbiter pass to end", RUN_LIMIT, || {
        let bytes = std::fs::read(s.state.arbiter_last()).ok()?;
        let last: ArbiterPass = serde_json::from_slice(&bytes).ok()?;
        let released = state::probe(&s.state.arbiter_lock()).ok()? != Probe::Held;
        (last.started_ms >= since_ms && last.ended_ms.is_some() && released).then_some(last)
    })
}

/// The pass `output` printed, which must have exited 0; the errors of the steps these tests
/// check must be `allowed`.
pub fn pass(output: &Output) -> Reconcile {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|err| panic!("{err}: {}", String::from_utf8_lossy(&output.stdout)))
}

/// The errors of `pass` from the steps of this unit (not [`OTHER_UNITS`]), but the
/// [`refused_start`] of a launcher on Windows.
pub fn own_errors(pass: &Reconcile) -> Vec<String> {
    pass.errors
        .iter()
        .filter(|e| !OTHER_UNITS.contains(&e.scope.as_str()) && !is_refused_start(e))
        .map(|e| format!("{}: {}", e.scope, e.message))
        .collect()
}

/// Whether the arbiter trigger of `pass` found an item due and could not start the launcher
/// because the job it runs in does not let a process leave it (`os::spawn_detached`), as
/// every job test-support starts a child in on Windows: a launcher left in that job would end
/// when the `reconcile` that started it does, leaving its pass running until `timeout_s`.
pub fn refused_start(pass: &Reconcile) -> bool {
    pass.errors.iter().any(is_refused_start)
}

fn is_refused_start(error: &ScopedError) -> bool {
    cfg!(windows)
        && error.scope == "arbiter"
        && error
            .message
            .contains("a job this process runs in does not let a process leave it")
}

/// The disk report `item` carries in its evidence.
pub fn carried_report(item: &InboxItem) -> Option<DiskReport> {
    serde_json::from_value(item.evidence["report"].clone()).unwrap()
}

/// The one report of `pass`.
pub fn report(pass: &Reconcile) -> &DiskReport {
    assert_eq!(pass.disk_reports.len(), 1, "{pass:#?}");
    &pass.disk_reports[0]
}

/// The targets of `report`, as paths relative to `root`.
pub fn targets(report: &DiskReport, root: &Path) -> Vec<String> {
    report
        .targets
        .iter()
        .map(|t| relative(&t.target, root))
        .collect()
}

/// The blocked entries of `report` as `(path relative to root, check)`.
pub fn blocked(report: &DiskReport, root: &Path) -> Vec<(String, String)> {
    report
        .blocked
        .iter()
        .map(|b| (relative(&b.target, root), b.check.clone()))
        .collect()
}

/// `path` relative to `root`, its components joined with `/` on every platform.
fn relative(path: &str, root: &Path) -> String {
    Path::new(path)
        .strip_prefix(root)
        .unwrap_or_else(|_| panic!("{path} is not under {}", root.display()))
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Writes `bytes` bytes to `root/rel`, creating its directories, and syncs them, so ext4's
/// extents count them (section 8.5).
pub fn fill(root: &Path, rel: &str, bytes: u64) {
    crate::plan_support::write_synced(root, rel, &vec![7u8; usize::try_from(bytes).unwrap()]);
}

/// Moves the last activity of the worktree at `wt` (its index's modification time)
/// `ago` into the past.
pub fn age(wt: &Path, ago: Duration) {
    let git = std::fs::read_to_string(wt.join(".git")).unwrap();
    let admin = PathBuf::from(git.trim().strip_prefix("gitdir: ").unwrap());
    let index = std::fs::File::options()
        .write(true)
        .open(admin.join("index"))
        .unwrap();
    index.set_modified(SystemTime::now() - ago).unwrap();
}
