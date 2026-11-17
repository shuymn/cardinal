//! The OS process helpers against real processes: the process table, start times, parent
//! PIDs, working directories, and boot time.

mod support;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cardinal::os::{self, Detail, ProcessTable};
use support::helper::{self, Mode, PidGuard, wait_for, wait_for_file};

#[test]
fn helper() {
    helper::run_if_requested();
}

fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

const WAIT: Duration = Duration::from_secs(10);

#[test]
fn own_process_is_listed_with_parent_start_time_and_cwd() {
    let table = ProcessTable::snapshot(Detail::all()).unwrap();
    let me = table
        .get(os::current_pid())
        .expect("this process is listed");
    #[cfg(unix)]
    assert_eq!(me.parent, Some(std::os::unix::process::parent_id()));
    assert!(me.parent.is_some());
    assert!(me.start_s <= now_s());
    assert!(me.start_s + 1 >= os::boot_time_s().unwrap());
    assert_ne!(me.name, "");
    assert_ne!(me.cmd, Vec::<String>::new());
    let cwd = dunce::canonicalize(std::env::current_dir().unwrap()).unwrap();
    assert_eq!(
        me.cwd.as_deref().map(|p| dunce::canonicalize(p).unwrap()),
        Some(cwd)
    );
    assert!(table.is_alive(me.pid, me.start_s));
    assert_eq!(os::start_time_s(me.pid), Some(me.start_s));
}

#[test]
fn child_identity_cwd_and_death() {
    let dir = tempfile::tempdir().unwrap();
    let before = now_s();
    let mut child = helper::spawn(&Mode::Sleep, Some(dir.path()));
    let after = now_s();
    let pid = child.id();

    let info = wait_for("the child in the process table", WAIT, || {
        os::process(pid, Detail::all())
    });
    assert_eq!(info.parent, Some(os::current_pid()));
    assert_eq!(os::parent_pid(pid), Some(os::current_pid()));
    assert!(
        info.start_s + 2 >= before && info.start_s <= after + 2,
        "start {} outside [{before}, {after}]",
        info.start_s
    );
    let expected_cwd = dunce::canonicalize(dir.path()).unwrap();
    let cwd = wait_for("the child's cwd", WAIT, || os::cwd(pid));
    assert_eq!(dunce::canonicalize(cwd).unwrap(), expected_cwd);

    assert_eq!(
        os::start_time_s(pid),
        Some(info.start_s),
        "start time is stable"
    );
    assert!(os::is_alive(pid, info.start_s));
    assert!(
        !os::is_alive(pid, info.start_s + 1),
        "another start time is another process"
    );

    // On Windows the guard still holds a handle to the killed child, which keeps it listed.
    child.kill();
    assert!(!os::is_alive(pid, info.start_s));
    assert_eq!(os::start_time_s(pid), None);
}

#[test]
fn descendants_and_ancestors_cover_grandchildren() {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    let pid_file = dir.path().join("pid");
    let child = helper::spawn(
        &Mode::Spawn {
            ready: ready.clone(),
            pid_file: pid_file.clone(),
            lock: None,
        },
        None,
    );
    wait_for_file(&ready, WAIT);
    let grandchild: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    let _grandchild = PidGuard(grandchild);

    let table = wait_for("both helpers in the table", WAIT, || {
        let table = ProcessTable::snapshot(Detail::default()).unwrap();
        (table.get(child.id()).is_some() && table.get(grandchild).is_some()).then_some(table)
    });
    assert_eq!(table.children(child.id()), [grandchild]);
    assert!(table.descendants(os::current_pid()).contains(&grandchild));
    assert_eq!(
        table.ancestors(grandchild)[..2],
        [child.id(), os::current_pid()]
    );
}

#[test]
fn boot_time_is_stable_and_in_the_past() {
    let first = os::boot_time_s().unwrap();
    let second = os::boot_time_s().unwrap();
    assert!(first.abs_diff(second) <= 1, "{first} vs {second}");
    assert!(first > 0 && first <= now_s());
}

/// A zombie (exited, not reaped) is not alive, although its PID still exists. Linux lists it
/// with the zombie flag; macOS does not list it at all.
#[cfg(unix)]
#[test]
fn zombie_is_not_alive() {
    let mut child = helper::spawn(&Mode::Exit, None);
    let pid = child.id();
    let start_s = wait_for("the child in the table", WAIT, || os::start_time_s(pid));
    wait_for("the child to exit", WAIT, || {
        match os::process(pid, Detail::default()) {
            None => Some(()),
            Some(info) => info.zombie.then_some(()),
        }
    });
    if cfg!(target_os = "linux") {
        let info = os::process(pid, Detail::default()).expect("Linux lists zombies");
        assert!(info.zombie);
    }
    assert!(!os::is_alive(pid, start_s));
    assert_eq!(os::start_time_s(pid), None);
    // Only this test reaps the child, so it was unreaped until now. macOS drops an exiting
    // process from the table a moment before it can be reaped.
    wait_for("the child to be reapable", WAIT, || {
        child.child().try_wait().unwrap()
    });
}
