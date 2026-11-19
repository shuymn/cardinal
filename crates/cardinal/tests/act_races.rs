//! Races with the deletion (A28; `docs/architecture.md` sections 9.7 and 13.7), each at a
//! pause point, after the deleter's `.reached` file (section 13.1). Unit X1b's deletion runs
//! through the library, as recovery of a `committed` execution that a test leaves as a crash
//! leaves it (`tests/execution_support`), in a helper process paused at `act.taken`, with the
//! first entry in the hold and unchecked, or at `act.held`, with it checked and not deleted.
//! X1c adds the races of `act` itself (section 14.2).
#![cfg(unix)]

mod execution_support;
mod support;

use std::path::Path;
use std::time::Duration;

use cardinal::testpoint;
use execution_support::{Execution, outcome};

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
