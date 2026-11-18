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
    Record, RecordKind,
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
const OTHER_UNITS: &[&str] = &["ui"];

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
        let (critical, warning, recovery) = lines;
        let main = s.main.display().to_string();
        // The arbiter's command does nothing should a pass start one.
        s.scene.observe_config(&format!(
            "repos = ['{main}']\n[disk]\ncritical_mib = {critical}\nwarning_mib = {warning}\n\
             recovery_mib = {recovery}\n[arbiter]\ncommand = ['true']\n{extra}"
        ));
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
        let mut command = self.s.cardinal();
        command.args(["reconcile", "--trigger", "manual"]);
        self.points.apply(&mut command, &[], None);
        command
    }

    /// One pass, which must succeed; when it started an arbiter pass, once that pass has
    /// ended, so neither the test's checks nor its next pass race the launcher's writes to
    /// the items it assigned.
    pub fn reconcile(&self) -> Reconcile {
        let since_ms = state::now_ms();
        let pass = pass(
            &self
                .reconcile_command()
                .output_within(RUN_LIMIT)
                .expect("run cardinal reconcile"),
        );
        if pass.arbiter == ArbiterStart::Started {
            self.arbiter_ended(since_ms);
        }
        pass
    }

    /// The arbiter pass a `reconcile` pass started at or after `since_ms` started, once it has
    /// ended and its launcher has let go of `arbiter/.lock`. The setup's `[arbiter] command`
    /// exits at once.
    pub fn arbiter_ended(&self, since_ms: u64) -> ArbiterPass {
        wait_for("the arbiter pass to end", RUN_LIMIT, || {
            let bytes = std::fs::read(self.s.state.arbiter_last()).ok()?;
            let last: ArbiterPass = serde_json::from_slice(&bytes).ok()?;
            let released = state::probe(&self.s.state.arbiter_lock()).ok()? != Probe::Held;
            (last.started_ms >= since_ms && last.ended_ms.is_some() && released).then_some(last)
        })
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

/// The pass `output` printed, which must have exited 0; the errors of the steps these tests
/// check must be `allowed`.
pub fn pass(output: &Output) -> Reconcile {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|err| panic!("{err}: {}", String::from_utf8_lossy(&output.stdout)))
}

/// The errors of `pass` from the steps of this unit (not [`OTHER_UNITS`]).
pub fn own_errors(pass: &Reconcile) -> Vec<String> {
    pass.errors
        .iter()
        .filter(|e| !OTHER_UNITS.contains(&e.scope.as_str()))
        .map(|e| format!("{}: {}", e.scope, e.message))
        .collect()
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

/// Writes `bytes` bytes to `root/rel`, creating its directories.
pub fn fill(root: &Path, rel: &str, bytes: u64) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![7u8; usize::try_from(bytes).unwrap()]).unwrap();
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
