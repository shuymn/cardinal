//! Races with an execution (A27, A28, A33–A36, A38, A39; `docs/architecture.md` sections 9.7
//! and 13.7), each at a pause point, after the second actor's `.reached` file (section 13.1).
//! The races of `act` itself run a granted owner's `act` on a linked worktree
//! (`tests/live_support`): at `act.lock_opened`, `act.claimed`, `act.moved`, `act.checked`,
//! and `act.committed`, and with jobs meanwhile. The deletion's races at `act.taken` and
//! `act.held` run through the library, as recovery of a `committed` execution that a test
//! leaves as a crash leaves it (`tests/execution_support`), but for a directory moved out of
//! `t` before it was entered, which only a live deletion can tell from one deleted before a
//! crash. Leases and signals that arrive within an execution (A44's withdrawn coverage, A45)
//! run at `act.moved`, `act.checked`, and `act.committed`.
#![cfg(unix)]

mod execution_support;
mod hook_support;
mod live_support;
mod observe_common;
mod plan_support;
mod support;

use std::path::Path;
use std::time::Duration;

use cardinal::contract::ActStatus;
use cardinal::testpoint;
use execution_support::{Execution, outcome};
use live_support::{GRANT, Live, assert_restored};
use plan_support::{age_tree, aged};

#[test]
fn helper() {
    execution_support::run_if_requested();
    support::helper::run_if_requested();
}

const WAIT: Duration = Duration::from_secs(30);

/// Runs recovery paused at `point`, calls `change` once the deleter reached it, releases it,
/// and returns its outcome.
fn paused(x: &Execution, point: &'static str, change: impl FnOnce()) -> serde_json::Value {
    let (mut command, out) = x.recover_command(true);
    command.env("CARDINAL_TEST_PAUSE", point);
    let mut child = command.spawn().unwrap();
    x.points.wait_reached(point, child.id(), WAIT);
    change();
    x.points.release(point);
    assert!(child.wait().unwrap().success());
    outcome(&out)
}

/// The paths of a result's `skipped_changed`.
fn kept(outcome: &serde_json::Value) -> Vec<String> {
    outcome["result"]["skipped_changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().to_owned())
        .collect()
}

fn read(path: &Path) -> String {
    String::from_utf8(std::fs::read(path).unwrap()).unwrap()
}

/// `quiet_s` of the A45 races: a signal made at a pause point is newer than it at step 8's
/// read, and the worktree's own setup, waited out, is older.
const QUIET_S: u64 = 4;

/// A45's execution: the owner's granted `act` on `target/` of a linked worktree `name`, with
/// `[sessions]` covering the fixture, `linger_s` 60 and `quiet_s` [`QUIET_S`]; `prepare` runs
/// on the setup and the worktree before the target is written, and the plan is made once the
/// worktree is quiet ([`plan_support::Setup::age_worktree`]).
fn quiet_live(name: &str, prepare: impl FnOnce(&plan_support::Setup, &Path)) -> Live {
    let mut s = plan_support::Setup::with_host(GRANT);
    let root = dunce::canonicalize(s.fx.root()).unwrap();
    s.scene.observe_config(&format!(
        "[sessions]\ncovered = ['{}']\nlinger_s = 60\nquiet_s = {QUIET_S}\n",
        root.display()
    ));
    Live::in_setup(s, name, "target", &[], |s, wt| {
        prepare(s, wt);
        plan_support::write(wt, "target/debug/app", "debug/app");
        s.age_worktree(wt);
    })
}

/// `cardinal hook claude session-start` for a session with its `cwd` in `x`'s worktree, run
/// by the test, which writes the session's lease entry (section 9.8).
fn session_start(x: &Live) {
    let input = serde_json::json!({
        "session_id": "s-race",
        "transcript_path": null,
        "cwd": x.wt,
        "permission_mode": "default",
        "hook_event_name": "SessionStart",
        "source": "startup",
    });
    let output = hook_support::output_with_stdin(
        x.s.cardinal().args(["hook", "claude", "session-start"]),
        input.to_string().as_bytes(),
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
}

/// The lease file of `x`'s worktree.
fn lease_of(x: &Live) -> std::path::PathBuf {
    x.s.state.lease(&cardinal::state::worktree_key(
        &dunce::canonicalize(&x.wt).unwrap(),
    ))
}

/// Moves `path`'s modification time an hour into the future, which counts as now (section
/// 9.8): a signal a race makes stays recent however late step 8 reads it.
fn ahead(path: &Path) {
    std::fs::File::open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_hours(1))
        .unwrap();
}

/// Writes `n` files `<rel>/f<i>` under `root`, each and their directory an hour old.
fn old_files(root: &Path, rel: &str, n: usize) {
    for i in 0..n {
        let file = format!("{rel}/f{i}");
        plan_support::write(root, &file, "x");
        aged(&root.join(file), Duration::from_hours(1));
    }
    aged(&root.join(rel), Duration::from_hours(1));
}

/// The detail of `not_in_use` in `result`.
fn in_use_detail(result: &cardinal::contract::ActResult) -> String {
    result
        .checks
        .iter()
        .find(|c| c.name == "not_in_use")
        .map_or_else(String::new, |c| c.detail.clone())
}

/// Section 9.8, `act`'s step 5: a write in place deep inside another regenerable target, below
/// its top-level entries and after the plan, is activity: `rejected` (`not_in_use`) naming the
/// file, with nothing moved; the same execution without the write commits (control).
#[test]
fn a_write_in_another_target_refuses() {
    use std::io::Write as _;
    let deep = "node_modules/pkg/lib/index.js";
    let prepare = |_: &plan_support::Setup, wt: &Path| plan_support::write(wt, deep, "x");
    let x = quiet_live("sibling", prepare);
    let file = x.wt.join(deep);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap()
        .write_all(b"y")
        .unwrap();
    ahead(&file);
    let result = x.act();
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["not_in_use"]);
    assert!(in_use_detail(&result).contains(deep), "{result:#?}");
    assert!(x.target().join("debug/app").exists());

    let x = quiet_live("sibling-control", prepare);
    let result = x.act();
    assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
    assert!(x.wt.join(deep).exists());
}

/// Section 9.8, `act`'s step 5: the entries below another regenerable target are read whole,
/// or `act` refuses. `node_modules/pkg` replaced by a new directory after the read listed
/// `node_modules/` and before it lists `pkg` (`walk.dir_checked`) leaves `not_in_use` unknown:
/// `rejected` with nothing moved; removed there, with the read held past `quiet_s`, it is
/// activity now. Nothing changed at the pause, the execution commits (control).
#[test]
fn another_target_is_read_whole() {
    let deep = "node_modules/pkg/lib/index.js";
    let prepare = |_: &plan_support::Setup, wt: &Path| plan_support::write(wt, deep, "x");
    let point = testpoint::WALK_DIR_CHECKED;
    let refused = |name: &str, change: &dyn Fn(&Path)| {
        let x = quiet_live(name, prepare);
        let result = x.act_at(point, || change(&x.wt.join("node_modules/pkg")));
        assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
        assert_eq!(result.reasons, ["not_in_use"]);
        assert!(x.target().join("debug/app").exists());
        in_use_detail(&result)
    };
    let detail = refused("sibling-replaced", &|pkg| {
        let aside = pkg.with_extension("old");
        std::fs::rename(pkg, &aside).unwrap();
        std::fs::create_dir(pkg).unwrap();
        aged(pkg, Duration::from_hours(1));
        aged(&aside, Duration::from_hours(1));
    });
    assert!(detail.contains("node_modules/pkg"), "{detail}");
    assert!(!detail.contains("not quiet"), "{detail}");
    let detail = refused("sibling-removed", &|pkg| {
        std::fs::remove_dir_all(pkg).unwrap();
        std::thread::sleep(Duration::from_millis(QUIET_S * 1000 + 200));
    });
    assert!(detail.contains("not quiet"), "{detail}");
    assert!(detail.contains("node_modules/pkg"), "{detail}");

    let x = quiet_live("sibling-whole", prepare);
    let result = x.act_at(point, || {});
    assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
}

/// Section 9.8, `act`'s step 5: another regenerable target the observation listed and gone
/// when its read begins is activity now, as a status path gone is. `packages/x/node_modules`
/// removed while the read of `node_modules` waits at `walk.target_checked`, before its own read
/// begins, refuses: `rejected` (`not_in_use`, not quiet) naming it, with nothing moved; nothing
/// removed there, the execution commits (control).
#[test]
fn another_target_gone_before_its_read_refuses() {
    let deep = "node_modules/pkg/index.js";
    let nested = "packages/x/node_modules/dep/index.js";
    let prepare = |s: &plan_support::Setup, wt: &Path| {
        s.fx.commit_file(wt, "packages/x/package.json", "{}\n", "a package");
        plan_support::write(wt, deep, "x");
        plan_support::write(wt, nested, "x");
    };
    let point = testpoint::WALK_TARGET_CHECKED;
    let x = quiet_live("sibling-gone", prepare);
    let gone = x.wt.join("packages/x/node_modules");
    let result = x.act_at(point, || std::fs::remove_dir_all(&gone).unwrap());
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["not_in_use"]);
    let detail = in_use_detail(&result);
    assert!(detail.contains("not quiet"), "{detail}");
    assert!(detail.contains("packages/x/node_modules"), "{detail}");
    assert!(x.target().join("debug/app").exists());

    let x = quiet_live("sibling-kept", prepare);
    let result = x.act_at(point, || {});
    assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
}

/// Section 9.8, `act`'s step 5: the other regenerable targets are read last, and only when
/// every other fact of `not_in_use` finds the worktree unused: the lease, the signals, the
/// reservations, the processes, and the descriptor scan, and every other invariant passes.
/// With the session lease holding the worktree, or a process working in it, `act` refuses
/// (`not_in_use`), and with a FIFO in the target, `plain_target`, `not_in_use` passing,
/// without reading `node_modules`: its read never reaches `walk.target_checked`; with none of
/// them it reads it and commits (control).
#[test]
fn a_held_worktree_reads_no_other_target() {
    let prepare = |_: &plan_support::Setup, wt: &Path| {
        plan_support::write(wt, "node_modules/pkg/index.js", "x");
    };
    let point = testpoint::WALK_TARGET_CHECKED;
    let run = |x: &Live| x.spawn(&[point], None).finish_reached(&[point], point);
    let x = quiet_live("held-no-read", prepare);
    session_start(&x);
    let (result, reached) = run(&x);
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert!(
        in_use_detail(&result).contains("the session lease holds it"),
        "{result:#?}"
    );
    assert!(!reached, "the other target was read");

    let x = quiet_live("used-no-read", prepare);
    let mode = support::helper::Mode::Sleep;
    let sleeper = support::helper::spawn(x.s.fx.scope(), &mode, &x.wt);
    let (result, reached) = run(&x);
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert!(
        in_use_detail(&result).contains("working directory inside"),
        "{result:#?}"
    );
    assert!(!reached, "the other target was read");
    drop(sleeper);

    // A FIFO made in the target after the plan, older than `quiet_s`: `not_in_use` passes and
    // `plain_target` refuses.
    let x = quiet_live("fifo-no-read", prepare);
    let fifo = x.target().join("pipe").into_os_string();
    let fifo = std::ffi::CString::new(std::os::unix::ffi::OsStringExt::into_vec(fifo)).unwrap();
    // SAFETY: `fifo` is a valid NUL-terminated path.
    let mkfifo = unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) };
    assert_eq!(mkfifo, 0, "mkfifo: {}", std::io::Error::last_os_error());
    // An hour old, set by name: opening a FIFO to set its times would wait for a writer.
    let old = rustix::fs::Timespec {
        tv_sec: i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                - 3600,
        )
        .unwrap(),
        tv_nsec: 0,
    };
    let times = rustix::fs::Timestamps {
        last_access: old,
        last_modification: old,
    };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        fifo.as_c_str(),
        &times,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
    let (result, reached) = run(&x);
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["plain_target"], "{result:#?}");
    let in_use = result
        .checks
        .iter()
        .find(|c| c.name == "not_in_use")
        .unwrap();
    assert_eq!(
        in_use.result,
        cardinal::contract::CheckResult::Pass,
        "{result:#?}"
    );
    assert!(!reached, "the other target was read");

    let x = quiet_live("free-read", prepare);
    let (result, reached) = run(&x);
    assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
    assert!(reached, "the other target was not read");
}

/// Section 9.7, step 5: the snapshot carries what it did not read. `target/obsolete` removed
/// after the snapshot read the target's names and before its `lstat` (`act.dir_listed`), a
/// removal that changes only the target's own mtime, which no signal reads, is
/// `changed_during_act` and restores the target, never a commit of the surviving entries;
/// nothing changed at the pause, the execution commits (control).
#[test]
fn an_entry_removed_while_the_snapshot_lists_restores() {
    let point = testpoint::ACT_DIR_LISTED;
    let x = Live::new("snapshot-vanished", "target", &["debug/app", "obsolete"]);
    let result = x.act_at(point, || {
        std::fs::remove_file(x.target().join("obsolete")).unwrap();
    });
    assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
    assert_eq!(result.reasons, ["changed_during_act"], "{result:#?}");
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("obsolete")),
        "{result:#?}"
    );
    assert_eq!(read(&x.target().join("debug/app")), "debug/app");
    assert!(!x.entry().exists(), "{result:#?}");

    let x = Live::new("snapshot-whole", "target", &["debug/app", "obsolete"]);
    let result = x.act_at(point, || {});
    assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
}

/// At `act.taken`, on the first entry of a directory.
mod taken {
    use super::*;

    /// The directory moved out of `t` into the worktree's `fuzz/corpus` gets its entry back
    /// and loses nothing: the take is checked against the directory's current path, and no
    /// entry is taken from it once it left.
    #[test]
    fn a_directory_moved_out_gets_its_entry_back() {
        let mut x = Execution::new("node_modules", &["a/f1", "a/f2", "b/g"]);
        std::fs::create_dir_all(x.worktree.join("fuzz/corpus")).unwrap();
        x.commit();
        let moved = x.worktree.join("fuzz/corpus/a");
        let got = paused(&x, testpoint::ACT_TAKEN, || {
            assert!(x.held("a/f1").exists());
            std::fs::rename(x.t().join("a"), &moved).unwrap();
        });
        assert_eq!(got["outcome"], "done", "{got}");
        assert_eq!(got["result"]["status"], "partial");
        assert_eq!(read(&moved.join("f1")), "a/f1");
        assert_eq!(read(&moved.join("f2")), "a/f2");
        let kept = kept(&got);
        assert!(kept.contains(&"a/f1".to_owned()), "{kept:?}");
        assert!(kept.contains(&"a/f2".to_owned()), "{kept:?}");
        // The rest of `t` is deleted.
        assert!(!x.entry().exists());
    }

    /// In a live execution, a directory of `t` not yet entered, moved out of `t` into the
    /// worktree's `fuzz/corpus`, took its entries with it: nothing was reclaimed for them, so
    /// the result is `partial` and names them kept, never `executed` as if they were gone.
    #[test]
    fn a_directory_not_yet_entered_moved_out_is_kept() {
        let x = Live::new("moved-out", "node_modules", &["a/f", "b/g1", "b/g2"]);
        let moved = x.wt.join("fuzz/corpus/b");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        let result = x.act_at(testpoint::ACT_TAKEN, || {
            assert!(!x.t().join("a/f").exists(), "a/f is the first entry taken");
            std::fs::rename(x.t().join("b"), &moved).unwrap();
        });
        assert_eq!(result.status, ActStatus::Partial, "{result:#?}");
        assert_eq!(read(&moved.join("g1")), "b/g1");
        assert_eq!(read(&moved.join("g2")), "b/g2");
        for path in ["b", "b/g1", "b/g2"] {
            assert!(
                result.skipped_changed.contains(&path.to_owned()),
                "{result:#?}"
            );
        }
        assert!(
            !result.skipped_changed.contains(&"a/f".to_owned()),
            "{result:#?}"
        );
        assert!(!x.entry().exists(), "the rest of t is deleted");
    }

    /// A directory of `t` not yet entered, replaced by a symbolic link to a directory outside
    /// that holds a file (std's `recursive_rmdir_toctou`, the race of CVE-2022-21658), is not
    /// entered and not taken: the link stays, and so does the file, though it has the name of
    /// the manifest's entry in the replaced directory.
    #[test]
    fn a_link_in_place_of_a_directory_is_not_entered() {
        let mut x = Execution::new("node_modules", &["a/f", "b/g"]);
        let outside = x.scope.path("outside");
        std::fs::create_dir_all(outside.join("dir")).unwrap();
        std::fs::write(outside.join("dir/g"), b"outside").unwrap();
        x.commit();
        let got = paused(&x, testpoint::ACT_TAKEN, || {
            std::fs::rename(x.t().join("b"), outside.join("b-moved")).unwrap();
            std::os::unix::fs::symlink(outside.join("dir"), x.t().join("b")).unwrap();
        });
        assert_eq!(got["outcome"], "done", "{got}");
        assert_eq!(got["result"]["status"], "partial");
        assert_eq!(read(&outside.join("dir/g")), "outside");
        assert_eq!(read(&outside.join("b-moved/g")), "b/g");
        assert_eq!(
            std::fs::read_link(x.t().join("b")).unwrap(),
            outside.join("dir")
        );
        assert!(!x.t().join("a").exists(), "a is deleted");
        assert!(kept(&got).contains(&"b".to_owned()));
    }

    /// A directory moved within `t` to a protected path while an entry of it is in the hold
    /// (`t/a` to `t/safe`, with `node_modules/safe` protected) loses nothing: the take's
    /// directory is no longer at its manifest path, so the entry goes back where the directory
    /// went and nothing more is taken from it, which the check would otherwise judge by the
    /// path `a` it no longer has.
    #[test]
    fn a_directory_moved_within_t_is_kept_where_it_went() {
        let mut x = Execution::new("node_modules", &["a/f1", "a/f2", "b/g"]);
        std::fs::write(
            x.config.join("repos/app.toml"),
            "[data]\nregenerable = [\"node_modules\"]\nprotected = [\"node_modules/safe\"]\n",
        )
        .unwrap();
        x.commit();
        let got = paused(&x, testpoint::ACT_TAKEN, || {
            assert!(x.held("a/f1").exists());
            std::fs::rename(x.t().join("a"), x.t().join("safe")).unwrap();
        });
        assert_eq!(got["outcome"], "done", "{got}");
        assert_eq!(got["result"]["status"], "partial");
        assert_eq!(read(&x.t().join("safe/f1")), "a/f1");
        assert_eq!(read(&x.t().join("safe/f2")), "a/f2");
        assert!(!x.t().join("b").exists(), "b is deleted");
        let kept = kept(&got);
        for path in ["a", "a/f1", "a/f2"] {
            assert!(kept.contains(&path.to_owned()), "{kept:?}");
        }
    }

    /// `t` moved back to the target's planned path while an entry of it is in the hold loses
    /// nothing: the chain of every take starts at the trash entry, where `t` no longer is, so
    /// the entry goes back into its directory in the worktree and nothing more is taken.
    #[test]
    fn t_moved_back_to_the_worktree_loses_nothing() {
        let mut x = Execution::new("node_modules", &["a/f1", "a/f2", "b/g"]);
        x.commit();
        let got = paused(&x, testpoint::ACT_TAKEN, || {
            std::fs::rename(x.t(), x.target()).unwrap();
        });
        assert_eq!(got["outcome"], "done", "{got}");
        assert_eq!(got["result"]["status"], "partial");
        for path in ["a/f1", "a/f2", "b/g"] {
            assert_eq!(read(&x.target().join(path)), path);
        }
    }

    /// A repository made in a directory after the deleter entered it (`a/.git`, or `.git` in
    /// `t` itself, made while `a/f1` is in the hold) keeps that directory whole: the entry goes
    /// back, and nothing more is taken from it or below it.
    #[test]
    fn a_repository_made_after_entering_keeps_its_directory() {
        for (repo, kept_paths) in [
            ("a", &["a/f1", "a/f2"][..]),
            ("", &["a/f1", "a/f2", "b/g"][..]),
        ] {
            let mut x = Execution::new("node_modules", &["a/f1", "a/f2", "b/g"]);
            x.commit();
            let got = paused(&x, testpoint::ACT_TAKEN, || {
                std::fs::create_dir(x.t().join(repo).join(".git")).unwrap();
            });
            assert_eq!(got["outcome"], "done", "{repo}: {got}");
            assert_eq!(got["result"]["status"], "partial");
            for path in kept_paths {
                assert_eq!(read(&x.t().join(path)), *path, "{repo}");
                assert!(kept(&got).contains(&(*path).to_owned()), "{repo}: {got}");
            }
            if repo == "a" {
                assert!(!x.t().join("b").exists(), "b is deleted");
            }
        }
    }

    /// An entry made to fail its check while another file is made at its name stays in the
    /// hold under its index, and both files survive.
    #[test]
    fn a_changed_entry_whose_name_is_taken_stays_in_the_hold() {
        let mut x = Execution::new("node_modules", &["a/f", "a/g"]);
        x.commit();
        let held = x.held("a/f");
        let got = paused(&x, testpoint::ACT_TAKEN, || {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&held)
                .unwrap();
            std::io::Write::write_all(&mut file, b" changed").unwrap();
            std::fs::write(x.t().join("a/f"), b"new").unwrap();
        });
        assert_eq!(got["outcome"], "done", "{got}");
        assert_eq!(got["result"]["status"], "partial");
        assert_eq!(read(&held), "a/f changed");
        assert_eq!(read(&x.t().join("a/f")), "new");
        assert!(!x.t().join("a/g").exists());
        let kept = kept(&got);
        assert!(kept.contains(&"a/f".to_owned()), "{kept:?}");
        assert_eq!(got["result"]["trash"], x.entry().display().to_string());
    }
}

/// At `act.held`, on the first entry, with the clock advanced past the next batch.
mod held {
    use super::*;

    /// A process opening a descriptor on an entry not yet taken stops the deletion at the next
    /// entry; what is left stays in `t`, and a recovery once it is gone finishes.
    #[test]
    fn a_process_opening_an_entry_stops_the_deletion() {
        let mut x = Execution::new("node_modules", &["a/f1", "a/f2", "b/g"]);
        x.commit();
        let ready = x.scope.path("holder.ready");
        let mut holder = None;
        let got = paused(&x, testpoint::ACT_HELD, || {
            let script = format!(
                "exec 3< '{}'; : > '{}'; exec sleep 60",
                x.t().join("b/g").display(),
                ready.display()
            );
            holder = Some(x.scope.shell(&script, x.scope.root()).spawn().unwrap());
            cardinal_test_support::process::wait_for_file(&ready, WAIT);
            x.advance_clock();
        });
        assert_eq!(got["outcome"], "stranded", "{got}");
        assert!(got["why"].as_str().unwrap().contains("in use"), "{got}");
        assert!(!x.held("a/f1").exists(), "the held entry is deleted");
        assert!(!x.t().join("a/f1").exists());
        assert_eq!(read(&x.t().join("a/f2")), "a/f2");
        assert_eq!(read(&x.t().join("b/g")), "b/g");
        assert!(!x.state.execution_result(&x.plan).exists());
        holder.unwrap().kill();
        let done = x.recover(true);
        assert_eq!(done["result"]["status"], "executed", "{done}");
    }

    /// A protected pattern added keeps the entries it covers; the rest is deleted.
    #[test]
    fn a_protected_pattern_added_keeps_what_it_covers() {
        let mut x = Execution::new("node_modules", &["a/f", "keep/x", "z"]);
        x.commit();
        let got = paused(&x, testpoint::ACT_HELD, || {
            std::fs::write(
                x.config.join("repos/app.toml"),
                "[data]\nregenerable = [\"node_modules\"]\nprotected = [\"keep\"]\n",
            )
            .unwrap();
            x.advance_clock();
        });
        assert_eq!(got["outcome"], "done", "{got}");
        assert_eq!(got["result"]["status"], "partial");
        assert_eq!(kept(&got), ["keep", "keep/x"]);
        assert_eq!(read(&x.t().join("keep/x")), "keep/x");
        assert!(!x.t().join("a").exists());
        assert!(!x.t().join("z").exists());
    }

    /// The profile dropping the target's regenerable pattern keeps every entry not yet taken.
    #[test]
    fn dropping_the_regenerable_pattern_keeps_what_is_not_taken() {
        let mut x = Execution::new("node_modules", &["a/f1", "a/f2", "b/g"]);
        x.commit();
        let got = paused(&x, testpoint::ACT_HELD, || {
            std::fs::write(
                x.config.join("repos/app.toml"),
                "[data]\nregenerable = [\"target\"]\n",
            )
            .unwrap();
            x.advance_clock();
        });
        assert_eq!(got["outcome"], "stranded", "{got}");
        assert!(
            got["why"].as_str().unwrap().contains("regenerable"),
            "{got}"
        );
        assert!(!x.t().join("a/f1").exists());
        assert_eq!(read(&x.t().join("a/f2")), "a/f2");
        assert_eq!(read(&x.t().join("b/g")), "b/g");
    }

    /// A profile that stops loading is seen beside a name in `repos/` that cannot be read and
    /// that the load does not read (`notes.txt`, a link to itself): the configuration's
    /// signature reads exactly the files the load reads, so the reload fails, and every entry
    /// not yet taken stays.
    #[test]
    fn a_profile_that_stops_loading_keeps_what_is_not_taken() {
        let mut x = Execution::new("node_modules", &["a/f1", "a/f2", "b/g"]);
        std::os::unix::fs::symlink("notes.txt", x.config.join("repos/notes.txt")).unwrap();
        x.commit();
        let got = paused(&x, testpoint::ACT_HELD, || {
            std::fs::write(x.config.join("repos/app.toml"), "[data\n").unwrap();
            x.advance_clock();
        });
        assert_eq!(got["outcome"], "stranded", "{got}");
        assert!(
            got["why"].as_str().unwrap().contains("configuration"),
            "{got}"
        );
        assert_eq!(read(&x.t().join("a/f2")), "a/f2");
        assert_eq!(read(&x.t().join("b/g")), "b/g");
    }

    /// The profiles' directory made unsearchable fails the reload, which keeps every entry not
    /// yet taken.
    #[test]
    fn an_unsearchable_profiles_directory_keeps_what_is_not_taken() {
        use std::os::unix::fs::PermissionsExt;
        let mut x = Execution::new("node_modules", &["a/f1", "a/f2", "b/g"]);
        x.commit();
        let repos = x.config.join("repos");
        let got = paused(&x, testpoint::ACT_HELD, || {
            std::fs::set_permissions(&repos, std::fs::Permissions::from_mode(0o000)).unwrap();
            x.advance_clock();
        });
        std::fs::set_permissions(&repos, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(got["outcome"], "stranded", "{got}");
        assert!(
            got["why"].as_str().unwrap().contains("configuration"),
            "{got}"
        );
        assert_eq!(read(&x.t().join("a/f2")), "a/f2");
        assert_eq!(read(&x.t().join("b/g")), "b/g");
    }
}

/// The detail of `result` names `what`.
#[track_caller]
fn assert_names(result: &cardinal::contract::ActResult, what: &str) {
    let error = result.error.as_deref().unwrap_or_default();
    assert!(error.contains(what), "{what}: {result:#?}");
}

/// The `sharedindex.*` files of `dirs` with their modification times.
fn shared_indexes(dirs: &[&Path]) -> Vec<(std::path::PathBuf, std::time::SystemTime)> {
    let mut found = Vec::new();
    for dir in dirs {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("sharedindex.")
            {
                found.push((entry.path(), entry.metadata().unwrap().modified().unwrap()));
            }
        }
    }
    found.sort();
    found
}

/// At `act.lock_opened`: `act` opened the plan's `act.lock` and has not tried it.
mod lock_opened {
    use super::*;

    /// The plan's directory removed meanwhile, with the lock path still valid: `act` reports
    /// `not_found` and moves nothing.
    #[test]
    fn a_removed_plan_is_not_found() {
        let x = Live::new("plan-removed", "target", &["debug/app"]);
        let before = x.files();
        let paused = x.spawn(&[testpoint::ACT_LOCK_OPENED], None);
        paused.wait(testpoint::ACT_LOCK_OPENED);
        std::fs::remove_dir_all(x.s.state.plan_dir(&x.plan.id)).unwrap();
        paused.release(testpoint::ACT_LOCK_OPENED);
        let output = paused.child.wait_with_output().unwrap();
        let error = plan_support::act_result(&output).expect_err("not_found");
        assert_eq!(
            error.code,
            cardinal::contract::ErrorCode::NotFound,
            "{error:?}"
        );
        assert_eq!(x.files(), before);
        assert!(!x.entry().exists());
    }
}

/// At `act.claimed`: the claim is published and the trash entry made; nothing moved, and the
/// index not pinned yet.
mod claimed {
    use super::*;

    /// `git update-index --split-index` gives `rejected` (`changed_during_act`, naming the
    /// index) with nothing moved and the shared index file's mtime unchanged (A33).
    #[test]
    fn split_index_rejected() {
        let x = Live::new("split", "target", &["debug/app"]);
        let before = x.files();
        let common = x.s.main.join(".git");
        let mut shared = Vec::new();
        let result = x.act_at(testpoint::ACT_CLAIMED, || {
            x.s.fx.split_index(&x.wt);
            shared = shared_indexes(&[&x.git_dir(), &common]);
            assert_ne!(shared, Vec::new());
        });
        assert_restored(
            &x,
            &result,
            ActStatus::Rejected,
            &["changed_during_act"],
            &before,
        );
        assert_names(&result, "the index is split");
        assert_eq!(shared_indexes(&[&x.git_dir(), &common]), shared);
    }

    /// The index file moved away, which git would list as empty: `rejected`
    /// (`changed_during_act`, naming the index) with nothing moved (A33).
    #[test]
    fn index_missing_rejected() {
        let x = Live::new("no-index", "target", &["debug/app"]);
        let before = x.files();
        let away = x.git_dir().join("index.away");
        let result = x.act_at(testpoint::ACT_CLAIMED, || {
            std::fs::rename(x.index(), &away).unwrap();
        });
        assert_restored(
            &x,
            &result,
            ActStatus::Rejected,
            &["changed_during_act"],
            &before,
        );
        assert_names(&result, &x.index().display().to_string());
    }

    /// The index file replaced by a symbolic link that names nothing, which git lists as an
    /// empty index, after the plan: `git status` then lists every tracked path as deleted, a
    /// tracked deletion counts as activity now (section 9.8), and `act` gives `rejected`
    /// (`not_in_use`) at its checks with nothing moved. The pin, which takes only a regular
    /// file, is never reached.
    #[test]
    fn index_a_dangling_link_rejected() {
        let x = Live::with_host(GRANT, "dangling", "target", &["debug/app"], |_, _| {});
        let index = x.index();
        std::fs::remove_file(&index).unwrap();
        std::os::unix::fs::symlink(x.s.fx.path("nowhere"), &index).unwrap();
        let before = x.files();
        let result = x.act();
        assert_restored(&x, &result, ActStatus::Rejected, &["not_in_use"], &before);
        let check = result
            .checks
            .iter()
            .find(|c| c.name == "not_in_use")
            .unwrap();
        assert!(check.detail.contains("a tracked deletion"), "{result:#?}");
    }

    /// Codex's step-8 counterexample (A33, A35): `git reset` to a commit that tracked
    /// `target/x` before the pin, and `git reset HEAD -- target/x` at `act.moved`, leave both
    /// listings clean; the pin alone catches it, naming the index, with `target/x`'s bytes
    /// restored and the claim never `committed`.
    #[test]
    fn index_reset_around_the_move() {
        let tracked = std::cell::RefCell::new(String::new());
        let x = Live::with_host(GRANT, "reset", "target", &["debug/app"], |s, wt| {
            plan_support::write(wt, "target/x", "tracked once");
            s.fx.add_force(wt, "target/x");
            *tracked.borrow_mut() = s.fx.commit(wt, "track target/x");
            s.fx.rm_cached(wt, "target/x");
            s.fx.commit(wt, "untrack target/x");
        });
        let tracked = tracked.into_inner();
        let before = x.files();
        let paused = x.spawn(&[testpoint::ACT_CLAIMED, testpoint::ACT_MOVED], None);
        paused.wait(testpoint::ACT_CLAIMED);
        x.s.fx.reset_path(&x.wt, &tracked, "target/x");
        plan_support::backdate(&x.index());
        paused.release(testpoint::ACT_CLAIMED);
        paused.wait(testpoint::ACT_MOVED);
        assert!(!x.target().exists());
        x.s.fx.reset_path(&x.wt, "HEAD", "target/x");
        plan_support::backdate(&x.index());
        let claim = std::fs::read_to_string(x.s.state.execution_claim(&x.plan.id)).unwrap();
        assert!(claim.contains("\"claimed\""), "{claim}");
        let result = paused.finish(&[testpoint::ACT_MOVED]);
        assert_restored(
            &x,
            &result,
            ActStatus::Rejected,
            &["changed_during_act"],
            &before,
        );
        assert_names(&result, &format!("the index {}", x.index().display()));
        assert_eq!(read(&x.target().join("x")), "tracked once");
        let claim = std::fs::read_to_string(x.s.state.execution_claim(&x.plan.id)).unwrap();
        assert!(claim.contains("\"claimed\""), "never committed: {claim}");
    }

    /// On macOS's case-insensitive volume, an index entry `Target/x` added with
    /// `core.ignoreCase` false before the pin reaches the target's parent under a spelling
    /// step 2 did not list: the commit restores the target, naming the entry (A36).
    #[cfg(target_os = "macos")]
    #[test]
    fn alias_entry_restores() {
        let x = Live::with_host(GRANT, "alias", "target", &["x"], |s, wt| {
            s.fx.config_core(wt, "core.ignoreCase", "false");
        });
        if !x.wt.join("TARGET").exists() {
            eprintln!("skipped alias_entry_restores: the volume is case-sensitive");
            return;
        }
        let before = x.files();
        let result = x.act_at(testpoint::ACT_CLAIMED, || {
            x.s.fx.add(&x.wt, "Target/x");
            plan_support::backdate(&x.index());
        });
        assert_restored(
            &x,
            &result,
            ActStatus::Rejected,
            &["changed_during_act"],
            &before,
        );
        assert_names(&result, "the index entry Target/x");
    }

    /// The target's parent moved into the worktree's `fuzz/corpus` gives `expired`
    /// (`identity_changed:parent`), with the target restored into the moved parent and
    /// nothing deleted.
    #[test]
    fn parent_moved() {
        let x = Live::new("parent-moved", "pkg/node_modules", &["a/index.js"]);
        let moved = x.wt.join("fuzz/corpus/pkg");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        let result = x.act_at(testpoint::ACT_CLAIMED, || {
            std::fs::rename(x.wt.join("pkg"), &moved).unwrap();
        });
        assert_eq!(result.status, ActStatus::Expired, "{result:#?}");
        assert_eq!(result.reasons, ["identity_changed:parent"], "{result:#?}");
        assert_eq!(read(&moved.join("node_modules/a/index.js")), "a/index.js");
        assert!(!x.entry().exists());
    }

    /// The target replaced by another directory gives `expired` (`identity_changed:target`),
    /// with both kept.
    #[test]
    fn target_replaced() {
        let x = Live::new("replaced", "target", &["debug/app"]);
        let old = x.wt.join("target.old");
        let result = x.act_at(testpoint::ACT_CLAIMED, || {
            std::fs::rename(x.target(), &old).unwrap();
            plan_support::write(&x.wt, "target/new", "new");
        });
        assert_eq!(result.status, ActStatus::Expired, "{result:#?}");
        assert_eq!(result.reasons, ["identity_changed:target"], "{result:#?}");
        assert_eq!(read(&old.join("debug/app")), "debug/app");
        assert_eq!(read(&x.target().join("new")), "new");
        assert!(!x.entry().exists());
    }

    /// An empty directory made at `t` makes the rename fail, with its inode unchanged and
    /// nothing moved (`trash_occupied`).
    #[test]
    fn an_empty_directory_at_t_is_left() {
        use std::os::unix::fs::MetadataExt as _;
        let x = Live::new("occupied", "target", &["debug/app"]);
        let before = x.files();
        let mut ino = 0;
        let result = x.act_at(testpoint::ACT_CLAIMED, || {
            std::fs::create_dir(x.t()).unwrap();
            ino = std::fs::metadata(x.t()).unwrap().ino();
        });
        assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
        assert_eq!(result.reasons, ["trash_occupied"], "{result:#?}");
        assert_eq!(x.files(), before);
        assert_eq!(std::fs::metadata(x.t()).unwrap().ino(), ino);
    }
}

/// At `act.moved`: the target is in the trash entry, nothing checked after the move.
mod moved {
    use super::*;

    /// A file appended in the moved tree gives `changed_during_act`, the target restored
    /// whole with the append.
    #[test]
    fn an_append_restores() {
        let x = Live::new("append", "target", &["debug/app", "debug/log"]);
        let result = x.act_at(testpoint::ACT_MOVED, || {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(x.t().join("debug/log"))
                .unwrap();
            std::io::Write::write_all(&mut file, b" more").unwrap();
        });
        assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
        assert_eq!(result.reasons, ["changed_during_act"], "{result:#?}");
        assert_names(&result, "debug/log");
        assert_eq!(read(&x.target().join("debug/log")), "debug/log more");
        assert!(!x.entry().exists());
    }

    /// A protected pattern added to the profile gives `no_protected`, the target restored.
    #[test]
    fn a_protected_pattern_restores() {
        let x = Live::new("protected", "target", &["debug/app"]);
        let before = x.files();
        let result = x.act_at(testpoint::ACT_MOVED, || {
            x.s.profile(
                "generic",
                &plan_support::PROFILE.replace("protected = [", "protected = [\"target/debug\", "),
            );
        });
        assert_restored(&x, &result, ActStatus::Rejected, &["no_protected"], &before);
    }

    /// `origin` set at `act.moved` to the URL of a profile that protects `target/draft`, which
    /// the target holds: `changed_during_act` naming `origin`, the target restored, as the
    /// profiles the new URL selects could protect what the deletion would remove.
    #[test]
    fn an_origin_selecting_a_protection_restores() {
        let x = Live::new("origin", "target", &["debug/app", "draft/x"]);
        x.s.profile(
            "other",
            "[match]\nremotes = [\"github.com/o/other\"]\n[data]\nprotected = [\"target/draft\"]\n",
        );
        let before = x.files();
        let result = x.act_at(testpoint::ACT_MOVED, || {
            x.s.fx
                .remote_set_url(&x.s.main, "origin", "https://github.com/o/other.git");
        });
        assert_restored(
            &x,
            &result,
            ActStatus::Rejected,
            &["changed_during_act"],
            &before,
        );
        assert_names(&result, "origin changed");
    }

    /// `origin` set at `act.moved` to a URL that holds credentials: `changed_during_act`
    /// naming that `origin` changed, and no output or file of the state dir holds the URL.
    #[test]
    fn a_credential_url_is_never_recorded() {
        let x = Live::new("credential", "target", &["debug/app"]);
        let before = x.files();
        let config = x.s.main.join(".git/config");
        let text = std::fs::read_to_string(&config).unwrap();
        let url = "https://github.com/acme/app.git";
        assert!(text.contains(url), "{text}");
        let result = x.act_at(testpoint::ACT_MOVED, || {
            // Built in parts, so no scanner takes the test's URL for a real credential.
            let secret = ["https://u:", "s3cret", "@github.com/acme/app.git"].concat();
            std::fs::write(&config, text.replace(url, &secret)).unwrap();
        });
        assert_restored(
            &x,
            &result,
            ActStatus::Rejected,
            &["changed_during_act"],
            &before,
        );
        assert_names(&result, "origin changed");
        assert!(!format!("{result:?}").contains("s3cret"), "{result:#?}");
        assert_no_credential(&x);
    }

    /// An `origin` that holds credentials from before the plan: the claim records it without
    /// its userinfo, the deletion, which reads `origin` again before each batch, finishes, and
    /// no file of the state dir holds the credential.
    #[test]
    fn a_credential_origin_is_claimed_without_it() {
        let url = "https://github.com/acme/app.git";
        // Built in parts, so no scanner takes the test's URL for a real credential.
        let secret = ["https://u:", "s3cret", "@github.com/acme/app.git"].concat();
        let x = Live::with_host(
            GRANT,
            "credential-claim",
            "target",
            &["debug/app"],
            |s, _| {
                let config = s.main.join(".git/config");
                let text = std::fs::read_to_string(&config).unwrap();
                assert!(text.contains(url), "{text}");
                std::fs::write(&config, text.replace(url, &secret)).unwrap();
            },
        );
        let claim = x.s.state.execution_claim(&x.plan.id);
        let mut recorded = None;
        let result = x.act_at(testpoint::ACT_COMMITTED, || {
            let claim: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&claim).unwrap()).unwrap();
            recorded = Some(claim["repo"]["remote"].clone());
        });
        assert_eq!(recorded, Some(serde_json::json!(url)));
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(!x.target().exists());
        assert_no_credential(&x);
    }

    /// Asserts that no file of the state dir holds the tests' credential, `s3cret`.
    fn assert_no_credential(x: &Live) {
        let mut dirs = vec![x.s.state_dir.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else {
                    let bytes = std::fs::read(&path).unwrap_or_default();
                    let found = bytes.windows(6).any(|w| w == b"s3cret");
                    assert!(!found, "{} holds the credential", path.display());
                }
            }
        }
    }

    /// An `origin` git cannot read after the move, a `url` with no value: `changed_during_act`,
    /// the target restored, as a failed read is not an absent remote.
    #[test]
    fn an_unreadable_origin_restores() {
        let x = Live::new("unread", "target", &["debug/app"]);
        let before = x.files();
        let config = x.s.main.join(".git/config");
        let text = std::fs::read_to_string(&config).unwrap();
        let result = x.act_at(testpoint::ACT_MOVED, || {
            std::fs::write(&config, format!("{text}[remote \"origin\"]\n\turl\n")).unwrap();
        });
        std::fs::write(&config, text).unwrap();
        assert_restored(
            &x,
            &result,
            ActStatus::Rejected,
            &["changed_during_act"],
            &before,
        );
        assert_names(&result, "origin could not be read");
    }

    /// `git worktree lock` gives `not_managed`, the target restored.
    #[test]
    fn a_worktree_lock_restores() {
        let x = Live::new("locked", "target", &["debug/app"]);
        let before = x.files();
        let result = x.act_at(testpoint::ACT_MOVED, || {
            x.s.fx.worktree_lock(&x.s.main, &x.wt);
        });
        assert_restored(&x, &result, ActStatus::Rejected, &["not_managed"], &before);
    }

    /// An `[observe] managed` root saved into `host.toml` at `act.moved`, naming the
    /// worktree through a symbolic link to its parent, gives `not_managed`, the target
    /// restored: the root and the worktree are compared as the volume resolves them, as
    /// observation compares them.
    #[test]
    fn a_managed_root_by_an_alias_restores() {
        let x = Live::new("aliased", "target", &["debug/app"]);
        let before = x.files();
        let alias = x.s.fx.path("alias");
        std::os::unix::fs::symlink(x.wt.parent().unwrap(), &alias).unwrap();
        let result = x.act_at(testpoint::ACT_MOVED, || {
            let host = x.s.config_dir.join("host.toml");
            let text = std::fs::read_to_string(&host).unwrap();
            let line = format!(
                "[observe]\nmanaged = [{{ app = \"t3code\", root = {:?} }}]\n",
                alias.display().to_string()
            );
            std::fs::write(&host, text.replacen("[observe]\n", &line, 1)).unwrap();
        });
        assert_restored(&x, &result, ActStatus::Rejected, &["not_managed"], &before);
    }

    /// A process holding a descriptor on a file of the moved tree gives `not_in_use`, the
    /// target restored.
    #[test]
    fn a_descriptor_restores() {
        let x = Live::new("held", "target", &["debug/app"]);
        let before = x.files();
        let mut holder = None;
        let result = x.act_at(testpoint::ACT_MOVED, || {
            let mut command = x.s.fx.command("sleep", x.s.fx.root());
            command
                .arg("60")
                .stdin(std::fs::File::open(x.t().join("debug/app")).unwrap());
            holder = Some(command.spawn().unwrap());
        });
        assert_restored(&x, &result, ActStatus::Rejected, &["not_in_use"], &before);
        holder.unwrap().kill();
    }

    /// `git status` with optional locks and `core.untrackedCache` true rewrites the index
    /// without changing an entry: the commit restores the target (`changed_during_act`,
    /// naming the index), and a new plan of the target executes (A35, A38).
    #[test]
    fn status_refresh_restores() {
        let mut x = Live::with_host(GRANT, "refresh", "target", &["debug/app"], |s, wt| {
            s.fx.config_core(wt, "core.untrackedCache", "true");
            s.fx.status(wt);
        });
        let before = x.files();
        let result = x.act_at(testpoint::ACT_MOVED, || {
            x.s.fx.status(&x.wt);
            plan_support::backdate(&x.index());
        });
        assert_restored(
            &x,
            &result,
            ActStatus::Rejected,
            &["changed_during_act"],
            &before,
        );
        assert_names(&result, &format!("the index {}", x.index().display()));
        x.replan();
        let result = x.act();
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(!x.target().exists());
    }

    /// A45: `session-start` in the worktree at `act.moved`, before step 7 reads the lease, with
    /// step 8 out of reach: the target restored, `rejected` (`not_in_use`), the check naming the session; the same execution
    /// without it commits (control).
    #[test]
    fn session_start_restores() {
        let x = quiet_live("moved-session", |_, _| {});
        let before = x.files();
        // A failure injected at `act.checked` stands in for step 8: only step 7's read can
        // report `not_in_use`.
        let paused = x.spawn(&[testpoint::ACT_MOVED], Some(testpoint::ACT_CHECKED));
        paused.wait(testpoint::ACT_MOVED);
        session_start(&x);
        let result = paused.finish(&[testpoint::ACT_MOVED]);
        assert_restored(&x, &result, ActStatus::Rejected, &["not_in_use"], &before);
        let check = result
            .checks
            .iter()
            .find(|c| c.name == "not_in_use")
            .unwrap();
        assert!(check.detail.contains("session s-race"), "{result:#?}");

        let x = quiet_live("moved-control", |_, _| {});
        let result = x.act_at(testpoint::ACT_MOVED, || {});
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
    }

    /// A new `target/` made at the old path, with a `.gitignore` holding `!draft`, changes
    /// nothing: the execution commits, and the new tree stays.
    #[test]
    fn a_new_target_at_the_old_path_stays() {
        let x = Live::new("recreated", "target", &["debug/app"]);
        let result = x.act_at(testpoint::ACT_MOVED, || {
            plan_support::write(&x.wt, "target/.gitignore", "!draft\n");
            plan_support::write(&x.wt, "target/draft", "draft");
        });
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert_eq!(read(&x.target().join("draft")), "draft");
        assert!(!x.target().join("debug").exists());
        assert!(!x.entry().exists());
    }
}

/// At `act.checked`: every check after the move passed; the commit's checks have not run.
/// Each change alone restores the target (`changed_during_act`, naming it).
mod checked {
    use super::*;

    /// Runs `act` paused at `act.checked`, applies `change`, and asserts the target restored
    /// with `changed_during_act` naming `what`.
    #[track_caller]
    fn assert_change_restores(x: &Live, what: &str, change: impl FnOnce()) {
        let before = x.files();
        let result = x.act_at(testpoint::ACT_CHECKED, change);
        assert_restored(
            x,
            &result,
            ActStatus::Rejected,
            &["changed_during_act"],
            &before,
        );
        assert_names(&result, what);
    }

    /// Section 9.8, step 8: a process started at `act.checked`, after step 7's scan, with its
    /// working directory inside the worktree is found by step 8's scan: `not_in_use` fails and
    /// the target is restored; one started there with its working directory outside the
    /// worktree leaves the execution to commit (control).
    #[test]
    fn a_process_started_inside_restores() {
        let x = Live::new("cwd-inside", "target", &["debug/app"]);
        let before = x.files();
        let mut sleeper = None;
        let result = x.act_at(testpoint::ACT_CHECKED, || {
            let mode = support::helper::Mode::Sleep;
            sleeper = Some(support::helper::spawn(x.s.fx.scope(), &mode, &x.wt));
        });
        assert_restored(&x, &result, ActStatus::Rejected, &["not_in_use"], &before);
        let pid = sleeper.as_ref().unwrap().id().to_string();
        let detail = in_use_detail(&result);
        assert!(detail.contains("working directory inside"), "{detail}");
        assert!(detail.contains(&pid), "{detail}");
        drop(sleeper);

        let x = Live::new("cwd-outside", "target", &["debug/app"]);
        let mut sleeper = None;
        let result = x.act_at(testpoint::ACT_CHECKED, || {
            let mode = support::helper::Mode::Sleep;
            sleeper = Some(support::helper::spawn(x.s.fx.scope(), &mode, x.s.fx.root()));
        });
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        drop(sleeper);
    }

    /// Section 9.8, step 8, with the state dir given through a symbolic link to it: a process
    /// started at `act.checked` whose working directory is the physical path inside the trash
    /// entry's `t`, the spelling the OS reports, is found by step 8's scan, which resolves the
    /// trash entry as the deletion guard does: `not_in_use` fails and the target is restored.
    /// One started there outside both leaves the execution to commit (control).
    #[test]
    fn a_process_in_the_trash_through_an_aliased_state_dir_restores() {
        let aliased = |name: &str| {
            let mut s = plan_support::Setup::with_host(GRANT);
            let link = s.fx.root().join("state-link");
            std::os::unix::fs::symlink(&s.scene.state_dir, &link).unwrap();
            s.scene.state_dir = link;
            Live::in_setup(s, name, "target", &["debug/app"], |_, _| {})
        };
        let x = aliased("cwd-trash");
        let before = x.files();
        let mut sleeper = None;
        let result = x.act_at(testpoint::ACT_CHECKED, || {
            let mode = support::helper::Mode::Sleep;
            let t = dunce::canonicalize(x.t()).unwrap();
            sleeper = Some(support::helper::spawn(x.s.fx.scope(), &mode, &t));
        });
        assert_restored(&x, &result, ActStatus::Rejected, &["not_in_use"], &before);
        let pid = sleeper.as_ref().unwrap().id().to_string();
        let detail = in_use_detail(&result);
        assert!(detail.contains(&pid), "{detail}");
        drop(sleeper);

        let x = aliased("cwd-elsewhere");
        let mut sleeper = None;
        let result = x.act_at(testpoint::ACT_CHECKED, || {
            let mode = support::helper::Mode::Sleep;
            sleeper = Some(support::helper::spawn(x.s.fx.scope(), &mode, x.s.fx.root()));
        });
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        drop(sleeper);
    }

    /// `origin` set to another URL.
    #[test]
    fn origin_set() {
        let x = Live::new("origin-set", "target", &["debug/app"]);
        assert_change_restores(&x, "origin changed", || {
            x.s.fx
                .remote_set_url(&x.s.main, "origin", "https://github.com/o/other.git");
        });
    }

    /// An ancestor `.gitignore` saved.
    #[test]
    fn an_ancestor_gitignore_saved() {
        let x = Live::new("saved", "target", &["debug/app"]);
        let gitignore = x.wt.join(".gitignore");
        assert_change_restores(&x, &gitignore.display().to_string(), || {
            let mut text = std::fs::read_to_string(&gitignore).unwrap();
            text.push_str("# saved\n");
            std::fs::write(&gitignore, text).unwrap();
        });
    }

    /// An ancestor `.gitignore` rewritten with the same size and its mtime set back, which
    /// only the hash shows.
    #[test]
    fn an_ancestor_gitignore_rewritten_in_place() {
        let x = Live::new("same-size", "target", &["debug/app"]);
        let gitignore = x.wt.join(".gitignore");
        assert_change_restores(&x, &gitignore.display().to_string(), || {
            let mtime = std::fs::metadata(&gitignore).unwrap().modified().unwrap();
            let text = std::fs::read_to_string(&gitignore).unwrap();
            let swapped = text.replacen("target/", "targe?/", 1);
            assert_eq!(swapped.len(), text.len());
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&gitignore)
                .unwrap();
            std::io::Write::write_all(&mut &file, swapped.as_bytes()).unwrap();
            file.set_modified(mtime).unwrap();
        });
    }

    /// An ancestor `.gitignore` moved away and replaced by a symbolic link to it, which only
    /// its `lstat` shows.
    #[test]
    fn an_ancestor_gitignore_replaced_by_a_link() {
        let x = Live::new("linked", "target", &["debug/app"]);
        let gitignore = x.wt.join(".gitignore");
        let away = x.wt.join(".gitignore.away");
        assert_change_restores(&x, &gitignore.display().to_string(), || {
            std::fs::rename(&gitignore, &away).unwrap();
            std::os::unix::fs::symlink(&away, &gitignore).unwrap();
        });
    }

    /// An ancestor `.gitignore` made unreadable, which `check-ignore -v` passes over with a
    /// warning (A39).
    #[test]
    fn an_unreadable_ancestor_gitignore() {
        use std::os::unix::fs::PermissionsExt as _;
        let x = Live::new("unreadable", "target", &["debug/app"]);
        let gitignore = x.wt.join(".gitignore");
        assert_change_restores(&x, &gitignore.display().to_string(), || {
            std::fs::set_permissions(&gitignore, std::fs::Permissions::from_mode(0o000)).unwrap();
        });
        std::fs::set_permissions(&gitignore, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    /// The common dir's `info/exclude` saved.
    #[test]
    fn info_exclude_saved() {
        let x = Live::new("exclude", "target", &["debug/app"]);
        let exclude = x.s.main.join(".git/info/exclude");
        assert_change_restores(&x, &exclude.display().to_string(), || {
            std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
            std::fs::write(&exclude, "# saved\n").unwrap();
        });
    }

    /// The file `core.excludesFile` names, saved.
    #[test]
    fn the_excludes_file_saved() {
        let ignore = std::cell::RefCell::new(std::path::PathBuf::new());
        let x = Live::with_host(GRANT, "excludes", "target", &["debug/app"], |s, wt| {
            let path = s.fx.path("global-ignore");
            std::fs::write(&path, "*.swp\n").unwrap();
            s.fx.config_core(wt, "core.excludesFile", &path.display().to_string());
            *ignore.borrow_mut() = path;
        });
        let ignore = ignore.into_inner();
        assert_change_restores(&x, &ignore.display().to_string(), || {
            std::fs::write(&ignore, "*.swp\n*.tmp\n").unwrap();
        });
    }

    /// A relative `XDG_CONFIG_HOME` in the environment of the plan and the `act`, whose
    /// `git/ignore` holds the only rule that excludes the target: git reads it from the
    /// worktree root it runs in, and so does the execution, so emptying it at `act.checked`
    /// restores the target.
    #[test]
    fn a_relative_excludes_file_emptied() {
        let mut s = plan_support::Setup::with_host(GRANT);
        s.scene
            .cardinal_env
            .push(("XDG_CONFIG_HOME".into(), "review-xdg".into()));
        let x = Live::in_setup(s, "xdg", "target", &["debug/app"], |s, wt| {
            s.fx.commit_file(
                wt,
                ".gitignore",
                "node_modules/\n.DS_Store\nfuzz/corpus/\nfuzz/artifacts/\nreview-xdg/\n",
                "the target ignored only through the excludes file",
            );
            plan_support::write_synced(wt, "review-xdg/git/ignore", b"/target/\n");
        });
        let ignore = x.wt.join("review-xdg/git/ignore");
        assert_change_restores(&x, "review-xdg/git/ignore", || {
            std::fs::write(&ignore, "").unwrap();
        });
    }

    /// The index file moved away (A39).
    #[test]
    fn the_index_moved_away() {
        let x = Live::new("index-away", "target", &["debug/app"]);
        let away = x.git_dir().join("index.away");
        assert_change_restores(&x, &x.index().display().to_string(), || {
            std::fs::rename(x.index(), &away).unwrap();
        });
    }

    /// The profiles' directory made unsearchable, which fails the reload (A39).
    #[test]
    fn unsearchable_profiles() {
        use std::os::unix::fs::PermissionsExt as _;
        let x = Live::new("profiles", "target", &["debug/app"]);
        let repos = x.s.config_dir.join("repos");
        assert_change_restores(&x, "the configuration does not load", || {
            std::fs::set_permissions(&repos, std::fs::Permissions::from_mode(0o000)).unwrap();
        });
        std::fs::set_permissions(&repos, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `core.ignoreCase` changed in the worktree's configuration.
    #[test]
    fn ignore_case_changed() {
        let x = Live::new("ignore-case", "target", &["debug/app"]);
        assert_change_restores(&x, "core.ignorecase", || {
            let now = x.s.fx.config_get(&x.wt, "core.ignorecase");
            let flipped = if now.as_deref() == Some("true") {
                "false"
            } else {
                "true"
            };
            x.s.fx.config_core(&x.wt, "core.ignoreCase", flipped);
        });
    }

    /// A tracked file marked skip-worktree rewrites the index, which the pin shows.
    #[test]
    fn a_skip_worktree_entry() {
        let x = Live::new("skip", "target", &["debug/app"]);
        assert_change_restores(&x, &format!("the index {}", x.index().display()), || {
            x.s.fx.skip_worktree(&x.wt, "src/lib.rs");
        });
    }

    /// The worktree root renamed away with a copy put at its path.
    #[test]
    fn the_root_replaced() {
        let x = Live::new("root", "target", &["debug/app"]);
        let away = x.s.fx.path("root-away");
        let before = x.files();
        let result = x.act_at(testpoint::ACT_CHECKED, || {
            std::fs::rename(&x.wt, &away).unwrap();
            std::fs::create_dir(&x.wt).unwrap();
            std::fs::copy(away.join(".git"), x.wt.join(".git")).unwrap();
        });
        assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
        assert_eq!(result.reasons, ["changed_during_act"], "{result:#?}");
        assert_names(&result, "the worktree root");
        // The target went back into its parent, the root that moved away.
        assert_eq!(
            plan_support::listing(&away.join("target")),
            before,
            "{result:#?}"
        );
        assert!(!x.entry().exists());
    }

    /// `git worktree move` of the worktree away and of a second worktree into its path: the
    /// commit's reads stay bound to the checked git dir, the root's identity changed, and the
    /// target goes back into its parent in the moved worktree (A34, A35).
    #[test]
    fn worktrees_swapped() {
        let x = Live::new("swap-a", "target", &["debug/app"]);
        let other = x.s.worktree("swap-b");
        let moved = x.s.fx.path("swap-a2");
        let before = x.files();
        let result = x.act_at(testpoint::ACT_CHECKED, || {
            x.s.fx.worktree_move(&x.s.main, &x.wt, &moved);
            x.s.fx.worktree_move(&x.s.main, &other, &x.wt);
        });
        assert_eq!(result.status, ActStatus::Rejected, "{result:#?}");
        assert_eq!(result.reasons, ["changed_during_act"], "{result:#?}");
        assert_names(&result, "the worktree root");
        assert_eq!(plan_support::listing(&moved.join("target")), before);
        assert!(!x.wt.join("target").exists(), "nothing went into swap-b");
        assert!(!x.entry().exists());
    }

    /// `core.sparseCheckout` set alone changes nothing, and the execution commits.
    #[test]
    fn sparse_checkout_alone_commits() {
        let x = Live::new("sparse-alone", "target", &["debug/app"]);
        let result = x.act_at(testpoint::ACT_CHECKED, || {
            x.s.fx.config_core(&x.wt, "core.sparseCheckout", "true");
        });
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(!x.target().exists());
    }

    /// A44: the worktree's root withdrawn from `[sessions] covered` before step 8's reload,
    /// the grant kept: the target restored, `dry_run` with `sessions_uncovered`; the same
    /// rewrite keeping the root covered commits (control).
    #[test]
    fn coverage_withdrawn_restores() {
        let covering = |x: &Live, root: &Path| {
            let path = x.s.config_dir.join("host.toml");
            let host: String = read(&path)
                .lines()
                .map(|line| {
                    if line.starts_with("covered = ") {
                        format!("covered = ['{}']\n", root.display())
                    } else {
                        format!("{line}\n")
                    }
                })
                .collect();
            std::fs::write(path, host).unwrap();
        };
        let x = Live::new("uncovered", "target", &["debug/app"]);
        let elsewhere = x.s.fx.path("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let before = x.files();
        let result = x.act_at(testpoint::ACT_CHECKED, || covering(&x, &elsewhere));
        assert_restored(
            &x,
            &result,
            ActStatus::DryRun,
            &["sessions_uncovered"],
            &before,
        );
        assert_names(&result, "under no [sessions] covered root");

        let x = Live::new("still-covered", "target", &["debug/app"]);
        let root = dunce::canonicalize(&x.wt).unwrap();
        let result = x.act_at(testpoint::ACT_CHECKED, || covering(&x, &root));
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
    }

    /// Runs A45's execution paused at `act.checked`, applies `change`, and asserts the target
    /// restored, `rejected` (`not_in_use`) naming `what`.
    #[track_caller]
    fn assert_in_use_restores(x: &Live, what: &str, change: impl FnOnce()) {
        let before = x.files();
        let result = x.act_at(testpoint::ACT_CHECKED, change);
        assert_restored(x, &result, ActStatus::Rejected, &["not_in_use"], &before);
        assert_names(&result, what);
    }

    /// A45's control: the same execution, nothing changed at `act.checked`, commits.
    #[test]
    fn quiet_commits() {
        let x = quiet_live("checked-quiet", |_, wt| {
            plan_support::write(wt, "node_modules/x/index.js", "x");
        });
        let result = x.act_at(testpoint::ACT_CHECKED, || {});
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(!x.target().exists());
    }

    /// A45: `session-start` in the worktree after step 7's read of the lease.
    #[test]
    fn session_start_restores() {
        let x = quiet_live("checked-session", |_, _| {});
        assert_in_use_restores(&x, "session s-race", || session_start(&x));
    }

    /// A45: a commit made without the index, as `git commit` in another worktree moving
    /// this one's branch would: `HEAD`'s reflog gets an entry, and the index stays as pinned.
    /// The commit modifies a tracked file that exists in the worktree, whose own mtime stays
    /// old, so only the reflog is the signal.
    #[test]
    fn commit_restores() {
        let x = quiet_live("checked-commit", |_, _| {});
        let commit =
            x.s.fx
                .commit_file(&x.s.main, "src/main.rs", "fn main() { }\n", "change");
        assert_in_use_restores(&x, "logs/HEAD", || {
            x.s.fx.update_ref(&x.wt, "HEAD", &commit);
            ahead(&x.git_dir().join("logs/HEAD"));
        });
    }

    /// A45: an untracked file made in the worktree, which step 8's fresh `git status` lists.
    #[test]
    fn new_status_path_refuses() {
        let x = quiet_live("checked-untracked", |_, _| {});
        assert_in_use_restores(&x, "notes.txt", || {
            plan_support::write(&x.wt, "notes.txt", "x");
            ahead(&x.wt.join("notes.txt"));
        });
    }

    /// A45: ignored output in a directory no entry of step 4's observation held (`dist/`,
    /// under an exclude rule): step 8's status lists ignored entries as observation does and
    /// reads the new one.
    #[test]
    fn new_ignored_output_refuses() {
        let x = quiet_live("checked-dist", |s, _| {
            std::fs::write(s.main.join(".git/info/exclude"), "dist/\n").unwrap();
        });
        assert!(!x.wt.join("dist").exists());
        // `dist`'s own mtime set back: only its top-level entry is recent.
        assert_in_use_restores(&x, "dist/out.js", || {
            plan_support::write(&x.wt, "dist/out.js", "x");
            ahead(&x.wt.join("dist/out.js"));
            aged(&x.wt.join("dist"), Duration::from_hours(1));
        });
    }

    /// Section 9.8, step 8: an ignored entry step 4 listed and gone when step 8's read opens
    /// it is activity now, as a target gone before step 5's read is: `node_modules`, another
    /// regenerable target, and `dist/`, an ignored entry no regenerable pattern matches, each
    /// removed at `act.checked`, a removal that changes only the worktree root's mtime, which
    /// no signal reads, restore the target with `rejected` (`not_in_use`) naming it. Nothing
    /// removed, the execution commits (control).
    #[test]
    fn a_listed_entry_gone_before_its_read_restores() {
        let prepare = |s: &plan_support::Setup, wt: &Path| {
            std::fs::write(s.main.join(".git/info/exclude"), "dist/\n").unwrap();
            plan_support::write(wt, "node_modules/x/index.js", "x");
            plan_support::write(wt, "dist/out.js", "x");
        };
        for gone in ["node_modules", "dist"] {
            let x = quiet_live(&format!("gone-{gone}"), prepare);
            let path = x.wt.join(gone);
            assert_in_use_restores(&x, &format!("{gone}, listed, is gone"), || {
                std::fs::remove_dir_all(&path).unwrap();
            });
        }
        let x = quiet_live("gone-none", prepare);
        let result = x.act_at(testpoint::ACT_CHECKED, || {});
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
    }

    /// Section 9.8, step 8: `quiet_s` raised at `act.checked` from 4 s to 40 minutes, the
    /// target's entry written 30 minutes before and everything else an hour: step 5 found the
    /// entry old, and step 8 judges it again under the reloaded window: the target restored,
    /// `rejected` (`not_in_use`) naming it. Raised to 10 minutes, under which it is old too, the
    /// execution commits (control).
    #[test]
    fn raised_quiet_restores() {
        let run = |name: &str, quiet_s: u64| {
            let mut s = plan_support::Setup::with_host(GRANT);
            let root = dunce::canonicalize(s.fx.root()).unwrap();
            s.scene.observe_config(&format!(
                "[sessions]\ncovered = ['{}']\nlinger_s = 60\nquiet_s = {QUIET_S}\n",
                root.display()
            ));
            let x = Live::in_setup(s, name, "target", &[], |s, wt| {
                plan_support::write(wt, "target/debug/app", "debug/app");
                age_tree(wt, Duration::from_hours(1));
                age_tree(
                    &s.main.join(".git/worktrees").join(name),
                    Duration::from_hours(1),
                );
                aged(&wt.join("target/debug/app"), Duration::from_mins(30));
            });
            let before = x.files();
            let result = x.act_at(testpoint::ACT_CHECKED, || {
                let host = x.s.config_dir.join("host.toml");
                let text = read(&host);
                let raised = text.replace(
                    &format!("quiet_s = {QUIET_S}\n"),
                    &format!("quiet_s = {quiet_s}\n"),
                );
                assert_ne!(raised, text);
                std::fs::write(host, raised).unwrap();
            });
            (x, before, result)
        };
        let (x, before, result) = run("checked-raised", 2400);
        assert_restored(&x, &result, ActStatus::Rejected, &["not_in_use"], &before);
        assert_names(&result, "target/debug/app");

        let (x, _, result) = run("checked-raised-less", 600);
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(!x.target().exists());
    }

    /// Section 9.8, step 8: `linger_s` raised at `act.checked` from 60 s to an hour, a session
    /// in the lease that ended 30 minutes before: under the reloaded `linger_s` it lingers, so
    /// the target is restored, `rejected` (`not_in_use`) naming it. Raised to 10 minutes,
    /// under which it does not linger, the execution commits (control).
    #[test]
    fn raised_linger_restores() {
        let run = |name: &str, linger_s: u64| {
            let x = quiet_live(name, |_, _| {});
            let ended = cardinal::state::now_ms() - 30 * 60 * 1000;
            let lease = cardinal::contract::SessionLease {
                schema: cardinal::contract::SessionLeaseSchema,
                worktree: dunce::canonicalize(&x.wt).unwrap().display().to_string(),
                overflow: false,
                sessions: vec![cardinal::contract::LeaseEntry {
                    session_id: "s-ended".into(),
                    agent: None,
                    uncertain: false,
                    transcript_path: None,
                    last_event: cardinal::contract::LeaseEvent::SessionEnd,
                    last_event_ms: ended,
                    ended_ms: Some(ended),
                }],
            };
            let path = lease_of(&x);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, serde_json::to_vec(&lease).unwrap()).unwrap();
            let before = x.files();
            let result = x.act_at(testpoint::ACT_CHECKED, || {
                let host = x.s.config_dir.join("host.toml");
                let text = read(&host);
                let raised = text.replace("linger_s = 60\n", &format!("linger_s = {linger_s}\n"));
                assert_ne!(raised, text);
                std::fs::write(host, raised).unwrap();
            });
            (x, before, result)
        };
        let (x, before, result) = run("checked-lingers", 3600);
        assert_restored(&x, &result, ActStatus::Rejected, &["not_in_use"], &before);
        assert_names(&result, "s-ended");

        let (x, _, result) = run("checked-lingers-less", 600);
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(!x.target().exists());
    }

    /// Section 9.8, step 8: 600 old untracked paths, listed by step 4 and again by step 8's
    /// status, are 600 paths, not 1,200 over the cap: the execution commits. With 401 more made
    /// at `act.checked`, 1,001 paths with none recent, it restores with `unknown` (control).
    #[test]
    fn listed_paths_count_once() {
        let prepare = |_: &plan_support::Setup, wt: &Path| old_files(wt, "untracked", 600);
        let x = quiet_live("checked-600", prepare);
        let result = x.act_at(testpoint::ACT_CHECKED, || {});
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");

        let x = quiet_live("checked-1001", prepare);
        let before = x.files();
        let result = x.act_at(testpoint::ACT_CHECKED, || old_files(&x.wt, "more", 401));
        assert_restored(&x, &result, ActStatus::Rejected, &["not_in_use"], &before);
        assert_names(&result, "more than 1000");
    }

    /// Runs A45's execution `x` paused at `act.checked` and then at `quiet.dir_listed` within
    /// step 8, where step 8's read of the signals has read the names of the first ignored entry
    /// it lists, `node_modules`, and `lstat`ed none; applies `change` there and returns the
    /// result. The point is released until `act.checked`, so earlier reads pass it.
    fn at_step_8_listing(x: &Live, change: impl FnOnce()) -> cardinal::contract::ActResult {
        let (listed, checked) = (testpoint::QUIET_DIR_LISTED, testpoint::ACT_CHECKED);
        let paused = x.spawn(&[listed, checked], None);
        paused.release(listed);
        paused.wait(checked);
        paused.rearm(listed);
        paused.release(checked);
        paused.wait(listed);
        change();
        paused.finish(&[listed])
    }

    /// Section 9.8, step 8: a direct entry of another regenerable target, `node_modules/b.js`,
    /// an hour old, unlinked from outside the worktree after step 8's read listed
    /// `node_modules` and before its `lstat` (`quiet.dir_listed`), a removal that changes only
    /// `node_modules`'s own mtime, which no signal reads, is activity now: the target restored,
    /// `rejected` (`not_in_use`) naming the entry. Nothing removed there, the execution
    /// commits (control).
    #[test]
    fn an_entry_removed_while_a_sibling_is_listed_restores() {
        let prepare = |_: &plan_support::Setup, wt: &Path| {
            for name in ["a.js", "b.js", "c.js"] {
                plan_support::write(wt, &format!("node_modules/{name}"), "x");
                aged(&wt.join("node_modules").join(name), Duration::from_hours(1));
            }
        };
        let x = quiet_live("listed-removed", prepare);
        let before = x.files();
        let gone = x.wt.join("node_modules/b.js");
        let result = at_step_8_listing(&x, || std::fs::remove_file(&gone).unwrap());
        assert_restored(&x, &result, ActStatus::Rejected, &["not_in_use"], &before);
        assert_names(&result, "node_modules/b.js, removed while it was read");

        let x = quiet_live("listed-kept", prepare);
        let result = at_step_8_listing(&x, || {});
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(!x.target().exists());
    }

    /// Section 9.8, steps 7 and 8: the reads after the move finish within `[observe]
    /// git_timeout_ms` of step 7's start, here 3 s, checked before each entry. Step 8's read
    /// held at `quiet.dir_listed`, after it listed `node_modules`, past it, `node_modules/a.js`
    /// written meanwhile, stops before that entry's `lstat`: the target restored, `rejected`
    /// (`not_in_use` `unknown`) with "the last read did not finish in time", never `fail` from
    /// the entry read late. Released at once, nothing written, the execution commits
    /// (control).
    #[test]
    fn a_late_read_after_the_move_restores() {
        let late = bounded_live("late");
        let before = late.files();
        let result = at_step_8_listing(&late, || {
            plan_support::write(&late.wt, "node_modules/a.js", "y");
            std::thread::sleep(Duration::from_millis(BOUND_MS + 500));
        });
        assert_late(&late, &result, &before);

        let x = bounded_live("in-time");
        let result = at_step_8_listing(&x, || {});
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(!x.target().exists());
    }

    /// Section 9.8, step 8: the deadline of the reads after the move is checked again once
    /// step 8's scan of the working directories returns. Held at `act.cwd_read` past it, the
    /// target is restored, `rejected` (`not_in_use` `unknown`) with "the last read did not
    /// finish in time", never committed on reads that ended late. Released at once, the
    /// execution commits (control).
    #[test]
    fn a_late_cwd_scan_restores() {
        let point = testpoint::ACT_CWD_READ;
        let run = |x: &Live, hold: Duration| {
            let paused = x.spawn(&[point], None);
            paused.wait(point);
            std::thread::sleep(hold);
            paused.finish(&[point])
        };
        let late = bounded_live("late-cwd");
        let before = late.files();
        let result = run(&late, Duration::from_millis(BOUND_MS + 500));
        assert_late(&late, &result, &before);

        let x = bounded_live("in-time-cwd");
        let result = run(&x, Duration::ZERO);
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(!x.target().exists());
    }

    /// `[observe] git_timeout_ms` of the deadline tests, the bound of the reads after the move.
    const BOUND_MS: u64 = 3000;

    /// A45's quiet execution of `target` beside `node_modules/a.js`, under [`BOUND_MS`].
    fn bounded_live(name: &str) -> Live {
        let mut s = plan_support::Setup::with_host(GRANT);
        s.scene.git_timeout_ms = BOUND_MS;
        let root = dunce::canonicalize(s.fx.root()).unwrap();
        s.scene.observe_config(&format!(
            "[sessions]\ncovered = ['{}']\nlinger_s = 60\nquiet_s = {QUIET_S}\n",
            root.display()
        ));
        Live::in_setup(s, name, "target", &[], |s, wt| {
            plan_support::write(wt, "node_modules/a.js", "x");
            plan_support::write(wt, "target/debug/app", "debug/app");
            s.age_worktree(wt);
        })
    }

    /// The target restored, `rejected` (`not_in_use` `unknown`) with "the last read did not
    /// finish in time".
    #[track_caller]
    fn assert_late(
        x: &Live,
        result: &cardinal::contract::ActResult,
        before: &std::collections::BTreeMap<std::path::PathBuf, String>,
    ) {
        assert_restored(x, result, ActStatus::Rejected, &["not_in_use"], before);
        assert_names(result, "the last read did not finish in time");
        let check = result
            .checks
            .iter()
            .find(|c| c.name == "not_in_use")
            .unwrap();
        assert_eq!(
            check.result,
            cardinal::contract::CheckResult::Unknown,
            "{result:#?}"
        );
    }

    /// The index file replaced by a symbolic link that names nothing at `act.checked`, after
    /// the pin: step 8, which takes only a regular file as the index, restores the target with
    /// `rejected` (`changed_during_act`, naming the index).
    #[test]
    fn index_a_dangling_link_rejected() {
        let x = Live::new("checked-dangling", "target", &["debug/app"]);
        let before = x.files();
        let result = x.act_at(testpoint::ACT_CHECKED, || {
            let index = x.index();
            std::fs::remove_file(&index).unwrap();
            std::os::unix::fs::symlink(x.s.fx.path("nowhere"), &index).unwrap();
        });
        assert_restored(
            &x,
            &result,
            ActStatus::Rejected,
            &["changed_during_act"],
            &before,
        );
        assert_names(&result, "is not a regular file");
    }

    /// A45: a tracked file deleted, which `git status` lists without a time: activity now.
    #[test]
    fn tracked_deletion_refuses() {
        let x = quiet_live("checked-deleted", |_, _| {});
        assert_in_use_restores(&x, "a tracked deletion", || {
            std::fs::remove_file(x.wt.join("src/main.rs")).unwrap();
        });
    }

    /// A45: a build writing into an ignored entry other than the target.
    #[test]
    fn ignored_output_refuses() {
        let x = quiet_live("checked-ignored", |_, wt| {
            plan_support::write(wt, "node_modules/x/index.js", "x");
        });
        assert_in_use_restores(&x, "node_modules", || {
            plan_support::write(&x.wt, "node_modules/new.js", "x");
            ahead(&x.wt.join("node_modules/new.js"));
        });
    }

    /// The caller's grant withdrawn restores the target and reports as without it:
    /// `dry_run` with `execution_not_granted`.
    #[test]
    fn a_withdrawn_grant() {
        let x = Live::new("withdrawn", "target", &["debug/app"]);
        let before = x.files();
        let result = x.act_at(testpoint::ACT_CHECKED, || x.write_host(""));
        assert_restored(
            &x,
            &result,
            ActStatus::DryRun,
            &["execution_not_granted"],
            &before,
        );
    }
}

/// At `act.committed`: `committed` is renamed into place and not synced; nothing taken.
mod committed {
    use super::*;

    /// A45's limit: `session-start` after the commit's rename finds the target absent; the
    /// execution goes on and the session's lease entry is written.
    #[test]
    fn session_start_finds_it_absent() {
        let x = quiet_live("committed-session", |_, _| {});
        let result = x.act_at(testpoint::ACT_COMMITTED, || {
            session_start(&x);
            assert!(!x.target().exists());
        });
        assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
        assert!(!x.target().exists());
        assert!(read(&lease_of(&x)).contains("s-race"));
    }

    /// A new file renamed over a manifest name is kept, with `partial`, its path in
    /// `skipped_changed`, the trash entry left, and `reclaim_stranded`.
    #[test]
    fn a_file_renamed_over_is_kept() {
        let x = Live::new("renamed-over", "target", &["debug/app", "debug/log"]);
        let result = x.act_at(testpoint::ACT_COMMITTED, || {
            std::fs::write(x.t().join("new"), "new").unwrap();
            std::fs::rename(x.t().join("new"), x.t().join("debug/log")).unwrap();
        });
        assert_eq!(result.status, ActStatus::Partial, "{result:#?}");
        assert!(
            result.skipped_changed.contains(&"debug/log".to_owned()),
            "{result:#?}"
        );
        assert_eq!(read(&x.t().join("debug/log")), "new");
        assert!(!x.t().join("debug/app").exists());
        assert_eq!(result.trash, Some(x.entry().display().to_string()));
        assert_eq!(x.stranded_items(), 1);
    }

    /// `origin` set to another URL after the commit stops the deletion before its first
    /// batch: `partial`, the moved tree kept whole in the trash entry, and `reclaim_stranded`.
    #[test]
    fn origin_set_stops_the_deletion() {
        let x = Live::new("origin-deleting", "target", &["debug/app"]);
        let result = x.act_at(testpoint::ACT_COMMITTED, || {
            x.s.fx
                .remote_set_url(&x.s.main, "origin", "https://github.com/o/other.git");
        });
        assert_eq!(result.status, ActStatus::Partial, "{result:#?}");
        assert_names(&result, "origin changed");
        assert_eq!(read(&x.t().join("debug/app")), "debug/app");
        assert_eq!(x.stranded_items(), 1);
    }

    /// `git init` in a directory of the moved tree keeps that directory whole, with
    /// `partial`.
    #[test]
    fn a_repository_made_inside_is_kept() {
        let x = Live::new("init", "target", &["debug/app", "other/file"]);
        let result = x.act_at(testpoint::ACT_COMMITTED, || {
            for dir in ["objects", "refs"] {
                std::fs::create_dir_all(x.t().join("debug/.git").join(dir)).unwrap();
            }
            std::fs::write(x.t().join("debug/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        });
        assert_eq!(result.status, ActStatus::Partial, "{result:#?}");
        assert_eq!(read(&x.t().join("debug/app")), "debug/app");
        assert!(!x.t().join("other").exists());
    }
}

/// A runner in the worktree, started with its own test points and paused at
/// `inuse.waiting`, released there to wait for the in-use lock; its stage appends a line to
/// `counter`.
fn waiting_runner(
    x: &Live,
    counter: &Path,
) -> (cardinal_test_support::Child, cardinal_test_support::Scope) {
    let scope = cardinal_test_support::Scope::new();
    let points = support::testpoints::TestPoints::new(&scope);
    let mut runner = x.s.cardinal_in(&x.wt);
    runner.args([
        "run",
        "--",
        "sh",
        "-c",
        &format!("echo run >> '{}'", counter.display()),
    ]);
    points.apply(&mut runner, &[testpoint::INUSE_WAITING], None);
    let runner = runner.spawn().unwrap();
    points.wait_reached(testpoint::INUSE_WAITING, runner.id(), live_support::WAIT);
    points.release(testpoint::INUSE_WAITING);
    (runner, scope)
}

/// Jobs that start meanwhile (A27): a process that lists the target at each pause point finds
/// it whole before `act.moved` and absent from it on, and whole again after a restore.
#[test]
fn jobs_meanwhile() {
    let x = Live::new("jobs", "target", &["debug/app", "debug/log"]);
    let before = x.files();
    let points = [
        testpoint::ACT_EXCLUDED,
        testpoint::ACT_CLAIMED,
        testpoint::ACT_MOVED,
        testpoint::ACT_CHECKED,
        testpoint::ACT_COMMITTED,
    ];
    let paused = x.spawn(&points, None);
    paused.wait(testpoint::ACT_EXCLUDED);
    assert_eq!(x.files(), before);
    for pair in points.windows(2) {
        paused.release(pair[0]);
        paused.wait(pair[1]);
        if pair[1] == testpoint::ACT_CLAIMED {
            assert_eq!(x.files(), before, "{}", pair[1]);
        } else {
            assert!(!x.target().exists(), "{}", pair[1]);
        }
    }
    let result = paused.finish(&[testpoint::ACT_COMMITTED]);
    assert_eq!(result.status, ActStatus::Executed, "{result:#?}");

    let x = Live::new("jobs-restored", "target", &["debug/app", "debug/log"]);
    let result = x.act_at(testpoint::ACT_MOVED, || {
        assert!(!x.target().exists());
        std::fs::write(x.t().join("debug/log"), "changed").unwrap();
    });
    assert_eq!(result.reasons, ["changed_during_act"], "{result:#?}");
    assert_eq!(read(&x.target().join("debug/app")), "debug/app");
    assert_eq!(read(&x.target().join("debug/log")), "changed");
}

/// C15: a runner that waits for the in-use lock while `act` holds it starts only once `act`
/// lets go, then runs once. Arriving at `act.committed`, after step 8's scan, it starts once
/// `act` committed. Arriving earlier, from `act.excluded` through `act.checked`, it has its
/// working directory in the worktree when step 7's or step 8's scan reads it, so `act`
/// refuses (`not_in_use`), without moving anything or restoring the target, and the runner
/// runs once `act` lets go: arriving at `act.checked`, it has not started when the restore
/// renamed the target back (`act.restored`), where the in-use lock is still held, probed
/// directly, and the target is back.
#[test]
fn runner_waits_for_the_commit() {
    let x = Live::new("runner", "target", &["debug/app"]);
    let counter = x.s.fx.path("counter");
    let paused = x.spawn(&[testpoint::ACT_COMMITTED], None);
    paused.wait(testpoint::ACT_COMMITTED);
    let (mut runner, _scope) = waiting_runner(&x, &counter);
    std::thread::sleep(Duration::from_millis(200));
    assert!(!counter.exists(), "the runner started before the commit");
    let result = paused.finish(&[testpoint::ACT_COMMITTED]);
    assert_eq!(result.status, ActStatus::Executed, "{result:#?}");
    assert_eq!(runner.wait().unwrap().code(), Some(0));
    assert_eq!(read(&counter), "run\n");

    let x = Live::new("runner-checked", "target", &["debug/app"]);
    let before = x.files();
    let counter = x.s.fx.path("counter-checked");
    let paused = x.spawn(&[testpoint::ACT_CHECKED, testpoint::ACT_RESTORED], None);
    paused.wait(testpoint::ACT_CHECKED);
    let (mut runner, _scope) = waiting_runner(&x, &counter);
    paused.release(testpoint::ACT_CHECKED);
    paused.wait(testpoint::ACT_RESTORED);
    // `act` holds the in-use lock past the rename back: the runner's shared lock cannot be
    // taken, and the target is whole in its place.
    let key = cardinal::state::worktree_key(&dunce::canonicalize(&x.wt).unwrap());
    let lock = x.s.state.worktree_lock(&key);
    let shared = cardinal::state::try_lock(&lock, cardinal::state::LockMode::Shared).unwrap();
    assert!(
        shared.is_none(),
        "the in-use lock was released before the restore"
    );
    assert_eq!(x.files(), before);
    assert!(!counter.exists(), "the runner started before the restore");
    let result = paused.finish(&[testpoint::ACT_RESTORED]);
    assert_restored(&x, &result, ActStatus::Rejected, &["not_in_use"], &before);
    assert_eq!(runner.wait().unwrap().code(), Some(0));
    assert_eq!(read(&counter), "run\n");

    let x = Live::new("runner-early", "target", &["debug/app"]);
    let before = x.files();
    let counter = x.s.fx.path("counter-early");
    let paused = x.spawn(&[testpoint::ACT_EXCLUDED], None);
    paused.wait(testpoint::ACT_EXCLUDED);
    let (mut runner, _scope) = waiting_runner(&x, &counter);
    let result = paused.finish(&[testpoint::ACT_EXCLUDED]);
    assert_restored(&x, &result, ActStatus::Rejected, &["not_in_use"], &before);
    assert_eq!(runner.wait().unwrap().code(), Some(0));
    assert_eq!(read(&counter), "run\n");
}
