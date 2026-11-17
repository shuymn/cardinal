//! The git fixture guard (`docs/architecture.md` section 13.3): git runs only inside the
//! fixture root, with a constructed environment.

mod support;

use std::ffi::OsString;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use support::fixture::Fixture;

fn refuses(what: &str, run: impl FnOnce()) {
    let result = catch_unwind(AssertUnwindSafe(run));
    let err = result.expect_err(&format!("{what} was not refused"));
    let message = err
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    assert!(message.contains("refusing git"), "{what}: {message}");
}

/// The state of a repository that a refused call must leave alone.
fn state(fixture: &Fixture, repo: &Path) -> String {
    let refs = fixture.git(repo, &["show-ref", "--head"]);
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
    let author = fixture.git(&repo, &["log", "-1", "--format=%an <%ae> %cn"]);
    assert_eq!(
        author.trim(),
        "Fixture Author <author@fixture.invalid> Fixture Committer"
    );
    let wt = fixture.worktree_add(&repo, "wt", "feature");
    assert!(wt.join("dir/a.txt").exists());
    let bare = fixture.init_bare("remote.git");
    fixture.git(&repo, &["remote", "add", "origin", &bare.to_string_lossy()]);
    fixture.git(&repo, &["push", "-q", "origin", "main"]);
    assert_eq!(fixture.git(&bare, &["rev-parse", "main"]).trim(), head);
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
        fixture.git(&repo, &["config", "commit.gpgsign"]).trim(),
        "false"
    );
}

#[test]
fn directory_outside_the_root_is_refused() {
    let fixture = Fixture::new();
    let other = Fixture::new();
    let outside = other.init("repo");
    refuses("a directory outside", || {
        fixture.git(&outside, &["status"]);
    });
    refuses("a relative directory that climbs out", || {
        fixture.git(&fixture.root().join(".."), &["status"]);
    });
}

#[test]
fn path_arguments_outside_the_root_are_refused() {
    let fixture = Fixture::new();
    let repo = fixture.init("repo");
    let other = Fixture::new();
    let outside_repo = other.init("repo");
    other.commit_file(&outside_repo, "a.txt", "a\n", "first");
    let before = state(&other, &outside_repo);
    let outside = outside_repo.to_string_lossy().into_owned();
    let git_dir = outside_repo.join(".git").to_string_lossy().into_owned();
    let cases: Vec<(&str, Vec<String>)> = vec![
        (
            "-C",
            vec![
                "-C".into(),
                outside.clone(),
                "commit".into(),
                "--allow-empty".into(),
                "-m".into(),
                "x".into(),
            ],
        ),
        ("-C attached", vec![format!("-C{outside}"), "status".into()]),
        (
            "--git-dir=",
            vec![format!("--git-dir={git_dir}"), "status".into()],
        ),
        (
            "--git-dir",
            vec!["--git-dir".into(), git_dir.clone(), "status".into()],
        ),
        (
            "--work-tree=",
            vec![format!("--work-tree={outside}"), "status".into()],
        ),
        ("absolute init", vec!["init".into(), outside.clone()]),
        (
            "worktree add",
            vec!["worktree".into(), "add".into(), format!("{outside}/wt")],
        ),
        ("relative climb", vec!["init".into(), "../../escape".into()]),
        ("home", vec!["init".into(), "~/escape".into()]),
        (
            "config value",
            vec!["config".into(), format!("core.hooksPath={outside}")],
        ),
    ];
    for (what, args) in cases {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        refuses(what, || {
            fixture.git(&repo, &args);
        });
    }
    assert_eq!(
        state(&other, &outside_repo),
        before,
        "the outside repository is untouched"
    );
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
    refuses("a decoy .git file", || {
        fixture.git(&decoy, &["add", "--", "b.txt"]);
    });
    refuses("a decoy .git file", || {
        fixture.git(&decoy, &["commit", "--allow-empty", "-m", "x"]);
    });
    assert_eq!(
        state(&other, &outside),
        before,
        "the decoy's target is untouched"
    );
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
        ("SSH_AUTH_SOCK", "/tmp/agent"),
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
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "SSH_AUTH_SOCK",
    ] {
        assert_eq!(get(name), None, "{name} must not pass");
    }
    let root = fixture.root().to_string_lossy().into_owned();
    for name in ["HOME", "XDG_CONFIG_HOME", "GIT_CONFIG_GLOBAL"] {
        assert!(
            get(name).unwrap().starts_with(&root),
            "{name} points inside the fixture"
        );
    }
    assert_eq!(get("GIT_CONFIG_NOSYSTEM").as_deref(), Some("1"));
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
