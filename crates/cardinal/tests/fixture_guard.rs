//! The git fixture guard (`docs/architecture.md` section 13.3): git runs only inside the
//! fixture root, through typed operations, with a constructed environment.

use std::ffi::OsString;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use cardinal_test_support::fixture::Fixture;

fn panic_message(run: impl FnOnce()) -> Option<String> {
    let err = catch_unwind(AssertUnwindSafe(run)).err()?;
    Some(
        err.downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default(),
    )
}

fn refuses(what: &str, run: impl FnOnce()) {
    let message = panic_message(run).unwrap_or_else(|| panic!("{what} was not refused"));
    assert!(message.contains("refusing git"), "{what}: {message}");
}

/// The state of a repository that a refused call must leave alone.
fn state(fixture: &Fixture, repo: &Path) -> String {
    let refs = fixture.show_ref(repo);
    let config = std::fs::read_to_string(repo.join(".git/config")).unwrap();
    let index = std::fs::read(repo.join(".git/index")).unwrap();
    format!("{refs}\n{config}\n{}", index.len())
}

#[test]
fn named_operations_work_inside_the_fixture() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    let head = fixture.commit_file(&repo, "dir/a.txt", "a\n", "first");
    assert_eq!(head.len(), 40);
    let author = fixture.log(&repo, "%an <%ae> %cn");
    assert_eq!(
        author.trim(),
        "Fixture Author <author@fixture.invalid> Fixture Committer"
    );
    let wt = fixture.worktree_add(&repo, "wt", "feature");
    assert!(wt.join("dir/a.txt").exists());
    let bare = fixture.init_bare("remote.git");
    fixture.push(&repo, &bare, "main");
    assert_eq!(fixture.rev_parse(bare.path(), "main"), head);
    fixture.config_set(&repo, "branch.main.note", "x");
    assert_eq!(
        fixture.config_get(&repo, "branch.main.note").as_deref(),
        Some("x")
    );
    assert_eq!(fixture.config_get(&repo, "branch.main.missing"), None);
    let url = "https://github.com/acme/app.git";
    fixture.remote_add_url(&repo, "github", url);
    assert_eq!(
        fixture.config_get(&repo, "remote.github.url").as_deref(),
        Some(url)
    );
}

#[test]
fn plain_git_has_no_transport() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    fixture.commit_file(&repo, "a.txt", "a\n", "first");
    let bare = fixture.init_bare("remote.git");
    fixture.remote_add(&repo, "origin", &bare);

    let output = fixture.push_remote(&repo, "origin", "main");
    assert!(
        !output.status.success(),
        "a push through a remote went through"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not allowed"), "{stderr}");
    assert_eq!(fixture.for_each_ref(bare.path()), "");
}

#[test]
fn config_outside_the_allowed_sections_is_refused() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    fixture.commit_file(&repo, "a.txt", "a\n", "first");
    let before = state(&fixture, &repo);
    for key in [
        "protocol.allow",
        "protocol.file.allow",
        "core.hooksPath",
        "core.fsmonitor",
        "alias.x",
        "include.path",
        "remote.origin.url",
        "credential.helper",
        "branch",
    ] {
        refuses(key, || fixture.config_set(&repo, key, "always"));
    }
    assert_eq!(state(&fixture, &repo), before);
}

#[test]
fn names_that_read_as_options_or_paths_are_refused() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    fixture.commit_file(&repo, "a.txt", "a\n", "first");
    refuses("an option as a branch", || {
        fixture.branch(&repo, "--git-dir=/tmp");
    });
    refuses("a climb in a ref", || {
        fixture.update_ref(&repo, "refs/heads/../../x", "HEAD");
    });
    refuses("an option as a revision", || {
        fixture.rev_parse(&repo, "--git-dir");
    });
    refuses("a space in a tag", || fixture.tag(&repo, "a b"));
    refuses("an option as a path", || fixture.add(&repo, "--all"));
    for url in [
        "/tmp/remote.git",
        "file:///tmp/remote.git",
        "ext::sh -c x",
        "https://",
    ] {
        refuses(url, || fixture.remote_add_url(&repo, "origin", url));
    }
}

#[test]
fn push_into_another_fixture_is_refused() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    fixture.commit_file(&repo, "a.txt", "a\n", "first");
    let other = Fixture::new();
    let outside = other.init_bare("other.git");
    refuses("a push into another fixture", || {
        fixture.push(&repo, &outside, "main");
    });
    refuses("a remote in another fixture", || {
        fixture.remote_add(&repo, "origin", &outside);
    });
    assert_eq!(other.for_each_ref(outside.path()), "");
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_dir(target, link).unwrap();
}

#[test]
fn paths_through_links_out_of_the_root_are_refused() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    let other = Fixture::new();
    let outside_repo = other.init("repo");
    symlink(&outside_repo, &repo.join("dir-link"));
    refuses("a file below a link", || {
        fixture.commit_file(&repo, "dir-link/x.txt", "x\n", "escape");
    });
    refuses("a climb after a link", || {
        fixture.commit_file(&repo, "dir-link/../../../escape.txt", "x\n", "escape");
    });
    refuses("a link as the repository", || {
        fixture.status(&repo.join("dir-link"));
    });
    assert!(!outside_repo.join("x.txt").exists());
}

#[test]
fn isolated_config_disables_signing_and_hooks() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    let hook = repo.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    fixture.commit_file(&repo, "a.txt", "a\n", "hooks do not run");
    assert_eq!(
        fixture.config_get(&repo, "commit.gpgsign").as_deref(),
        Some("false")
    );
}

#[test]
fn directory_outside_the_root_is_refused() {
    let fixture = Fixture::new();
    let other = Fixture::new();
    let outside = other.init("repo");
    refuses("a directory outside", || {
        fixture.status(&outside);
    });
    refuses("a relative directory that climbs out", || {
        fixture.status(&fixture.root().join(".."));
    });
}

#[test]
fn decoy_git_file_pointing_outside_is_refused() {
    let fixture = Fixture::new();
    let other = Fixture::new();
    let outside = other.init("repo");
    other.commit_file(&outside, "a.txt", "a\n", "first");
    let before = state(&other, &outside);

    let decoy = fixture.path("decoy");
    std::fs::create_dir_all(&decoy).unwrap();
    std::fs::write(
        decoy.join(".git"),
        format!("gitdir: {}\n", outside.join(".git").display()),
    )
    .unwrap();
    std::fs::write(decoy.join("b.txt"), "b\n").unwrap();
    refuses("a decoy .git file", || fixture.add(&decoy, "b.txt"));
    refuses("a decoy .git file", || {
        fixture.commit(&decoy, "x");
    });
    assert_eq!(
        state(&other, &outside),
        before,
        "the decoy's target is untouched"
    );
}

#[test]
fn not_a_repository_is_told_apart_from_other_failures() {
    let fixture = Fixture::new();
    let plain = fixture.path("plain");
    std::fs::create_dir(&plain).unwrap();
    let message = panic_message(|| {
        fixture.status(&plain);
    })
    .expect("status outside a repository");
    assert!(message.contains("is not a repository"), "{message}");

    let broken = fixture.path("broken");
    std::fs::create_dir(&broken).unwrap();
    std::fs::write(broken.join(".git"), "not a gitdir line\n").unwrap();
    let message = panic_message(|| {
        fixture.status(&broken);
    })
    .expect("status with a broken .git file");
    assert!(message.contains("cannot resolve"), "{message}");
}

#[test]
fn environment_is_constructed_not_inherited() {
    let fixture = Fixture::new();
    let inherited: Vec<(OsString, OsString)> = [
        ("PATH", "/usr/bin"),
        ("GIT_DIR", "/real/.git"),
        ("GIT_WORK_TREE", "/real"),
        ("GIT_INDEX_FILE", "/real/.git/index"),
        ("GIT_COMMON_DIR", "/real/.git"),
        ("GIT_CONFIG_PARAMETERS", "'user.name'='x'"),
        ("GIT_CONFIG_COUNT", "1"),
        ("HOME", "/home/real"),
        ("XDG_CONFIG_HOME", "/home/real/.config"),
        ("TMPDIR", "/real/tmp"),
        ("TEMP", "C:\\real\\tmp"),
        ("SSH_AUTH_SOCK", "/tmp/agent"),
        ("BASH_ENV", "/real/startup"),
        ("CARDINAL_TEST_HELPER", "spawn"),
        ("CARDINAL_TEST_RUN", "run-1"),
    ]
    .iter()
    .map(|(k, v)| ((*k).into(), (*v).into()))
    .collect();
    let env = fixture.env_from(&inherited);
    let get = |name: &str| {
        env.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.to_string_lossy().into_owned())
    };
    assert_eq!(get("PATH").as_deref(), Some("/usr/bin"));
    assert_eq!(get("CARDINAL_TEST_RUN").as_deref(), Some("run-1"));
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "SSH_AUTH_SOCK",
        "BASH_ENV",
        "CARDINAL_TEST_HELPER",
    ] {
        assert_eq!(get(name), None, "{name} must not pass");
    }
    let root = fixture.root().to_string_lossy().into_owned();
    let mut inside = vec![
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "GIT_CONFIG_GLOBAL",
    ];
    inside.extend(if cfg!(windows) {
        ["TEMP", "TMP"].as_slice()
    } else {
        ["TMPDIR"].as_slice()
    });
    for name in inside {
        assert!(
            get(name).is_some_and(|value| value.starts_with(&root)),
            "{name} points inside the fixture"
        );
    }
    assert_eq!(get("GIT_CONFIG_NOSYSTEM").as_deref(), Some("1"));
    assert_eq!(get("GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
    assert_eq!(
        get("GIT_CEILING_DIRECTORIES").as_deref(),
        fixture
            .root()
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .as_deref()
    );
}

#[test]
fn fixture_root_is_outside_any_repository() {
    let fixture = Fixture::new();
    assert!(
        fixture
            .root()
            .ancestors()
            .all(|dir| !dir.join(".git").exists()),
        "{}",
        fixture.root().display()
    );
}

/// Cleaning up a filesystem monitor touches only the repository it was configured for, also
/// when a test replaced that repository's `.git` with a gitfile naming another one.
#[test]
fn fsmonitor_cleanup_follows_no_replaced_git_dir() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    let other = fixture.init("other");
    let other_monitor = fixture.fsmonitor(&other);
    let monitor = fixture.fsmonitor(&repo);
    std::fs::rename(repo.join(".git"), repo.join("moved.git")).unwrap();
    std::fs::write(
        repo.join(".git"),
        format!("gitdir: {}\n", other.join(".git").display()),
    )
    .unwrap();
    drop(monitor);
    assert_eq!(
        fixture.config_get(&other, "core.fsmonitor").as_deref(),
        Some("true"),
        "the other repository's setting was removed"
    );
    std::fs::remove_file(repo.join(".git")).unwrap();
    std::fs::rename(repo.join("moved.git"), repo.join(".git")).unwrap();
    drop(other_monitor);
}
