//! C20 for the hook: cardinal creates no files inside target repositories
//! (`docs/architecture.md` sections 13.5 and 16). The worktree is listed recursively, `.git`
//! and ignored files included, with each entry's type, size, and modification time, before
//! and after the hooks run in it and a rewritten command whose stages write nothing runs in
//! it.

mod hook_support;
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use cardinal_test_support::fixture::Fixture;
use hook_support::{PROFILE, cardinal, configure, input, output_with_stdin};
use serde_json::{Value, json};
use support::cardinal::Dirs;

/// A profile matched by the fixture's origin, so the hook runs git in the repository.
const APP: &str = "[match]\nremotes = [\"github.com/o/app\"]\n[[rule]]\nid = \"test\"\ncommands = [\"probe\"]\nclass = \"app-test\"\n";

/// Every entry below `root`: type, size, and modification time.
fn listing(root: &Path) -> BTreeMap<PathBuf, (String, u64, Option<SystemTime>)> {
    fn walk(
        root: &Path,
        dir: &Path,
        out: &mut BTreeMap<PathBuf, (String, u64, Option<SystemTime>)>,
    ) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let meta = std::fs::symlink_metadata(entry.path()).unwrap();
            let kind = format!("{:?}", meta.file_type());
            out.insert(
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                (kind, meta.len(), meta.modified().ok()),
            );
            if meta.is_dir() {
                walk(root, &entry.path(), out);
            }
        }
    }
    let mut out = BTreeMap::new();
    let meta = std::fs::symlink_metadata(root).unwrap();
    out.insert(PathBuf::new(), ("root".into(), 0, meta.modified().ok()));
    walk(root, root, &mut out);
    out
}

/// A repository with a commit, an ignored build directory, and an origin remote.
fn repository(fixture: &Fixture) -> PathBuf {
    let repo = fixture.init("app");
    fixture.commit_file(&repo, ".gitignore", "target/\n", "ignore target");
    fixture.commit_file(&repo, "sub/lib.txt", "lib\n", "add lib");
    std::fs::create_dir_all(repo.join("target/debug")).unwrap();
    std::fs::write(repo.join("target/debug/out.bin"), b"built").unwrap();
    fixture.remote_add_url(&repo, "origin", "https://github.com/o/app.git");
    repo
}

fn end_input(event: &str, cwd: &Path) -> Value {
    json!({
        "session_id": "sess-repo",
        "transcript_path": "/tmp/t.jsonl",
        "cwd": cwd,
        "permission_mode": "default",
        "hook_event_name": event,
        "reason": "clear",
        "agent_id": "agent-1",
        "agent_type": "general-purpose"
    })
}

/// The three hooks, run with the repository as cwd, leave it unchanged; the pre-tool-use
/// hook also ran git there to match the profile by its remote.
#[test]
fn hooks_write_nothing_in_the_repository() {
    let fixture = Fixture::new();
    let repo = repository(&fixture);
    let dirs = Dirs::new();
    configure(
        &dirs,
        &hook_support::host(hook_support::RUNNER, ""),
        &[("app", APP), ("generic", PROFILE)],
    );
    let before = listing(&repo);

    let doc = hook_support::run_hook(
        cardinal(fixture.scope(), &dirs, &repo.join("sub")),
        &input("probe x", &repo.join("sub")),
    );
    if cfg!(windows) {
        assert_eq!(doc, None, "phase 0 does not rewrite on Windows");
    } else {
        let doc = doc.expect("a rewrite");
        let spec = &hook_support::specs(hook_support::rewritten(&doc))[0];
        assert_eq!(
            spec.rule.as_deref(),
            Some("app/test"),
            "matched through git"
        );
    }
    for (event, name) in [
        ("SubagentStop", "subagent-stop"),
        ("SessionEnd", "session-end"),
    ] {
        let output = output_with_stdin(
            cardinal(fixture.scope(), &dirs, &repo).args(["hook", "claude", name]),
            end_input(event, &repo).to_string().as_bytes(),
        );
        assert_eq!(output.status.code(), Some(0), "{name}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "", "{name}");
    }

    assert_eq!(listing(&repo), before);
    assert!(
        dirs.state()
            .join("sessions")
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    );
}

/// A rewritten command whose stages write nothing runs in the repository and leaves it
/// unchanged; the real runner started both wrapped stages there.
#[cfg(unix)]
#[test]
fn rewritten_command_writes_nothing_in_the_repository() {
    let fixture = Fixture::new();
    let repo = repository(&fixture);
    let bin = fixture.path("bin");
    std::fs::create_dir_all(&bin).unwrap();
    hook_support::probe(&bin);
    let runner = hook_support::runner_path(&hook_support::real_runner(&bin));
    let dirs = Dirs::new();
    configure(
        &dirs,
        &hook_support::host(&runner, ""),
        &[("app", APP), ("generic", PROFILE)],
    );
    let command = "cd sub && probe --err x 2>&1 | tail -5 || echo failed; probe --env > /dev/null";
    let doc = hook_support::run_hook(
        cardinal(fixture.scope(), &dirs, &repo),
        &input(command, &repo),
    )
    .expect("a rewrite");
    let rewritten = hook_support::rewritten(&doc);
    let before = listing(&repo);

    let path = std::env::join_paths(
        std::iter::once(bin.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let output = fixture
        .command("sh", &repo)
        .env("PATH", path)
        .env("CARDINAL_STATE_DIR", dirs.state())
        .env("CARDINAL_CONFIG_DIR", dirs.config())
        .args(["-c", rewritten])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("probe argv: <--err> <x>"));
    let jobs = hook_support::started_jobs(dirs.state());
    assert_eq!(jobs.len(), 2, "{jobs:#?}");

    assert_eq!(listing(&repo), before);
}
