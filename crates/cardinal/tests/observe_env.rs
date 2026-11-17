//! A2: git and gh run with a constructed environment from which the caller's
//! repository-location variables are absent (`docs/architecture.md` section 8.1).

mod observe_common;
mod support;

use std::collections::BTreeMap;
use std::path::Path;

use observe_common::Scene;

/// Every file below `dir` with its contents, for "untouched" checks.
fn contents(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    out
}

/// `cardinal inventory` started as a git hook would start it, with `GIT_DIR`,
/// `GIT_WORK_TREE`, and `GIT_INDEX_FILE` of a decoy repository exported, observes the
/// requested repository and leaves the decoy untouched.
#[test]
fn inherited_git_vars_ignored() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    let head = scene.commit(&wt, "feature.txt");
    let decoy = scene.fx.init("decoy");
    let decoy_head = scene
        .fx
        .commit_file(&decoy, "decoy.txt", "decoy\n", "decoy");
    scene
        .fx
        .git(&decoy, &["checkout", "-q", "-b", "decoy-branch"]);
    let before = contents(&decoy.join(".git"));

    let output = scene
        .cardinal()
        .env("GIT_DIR", decoy.join(".git"))
        .env("GIT_WORK_TREE", &decoy)
        .env("GIT_INDEX_FILE", decoy.join(".git/index"))
        .env("GIT_COMMON_DIR", decoy.join(".git"))
        .env("GIT_CONFIG_PARAMETERS", "'core.bare'='true'")
        .args(["inventory", "--repo"])
        .arg(&scene.repo)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let doc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let repos = doc["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 1, "{doc:#}");
    let repo = &repos[0];
    assert_eq!(
        repo["main_worktree"].as_str().unwrap(),
        scene.repo.to_string_lossy()
    );
    let worktrees = repo["worktrees"].as_array().unwrap();
    let paths: Vec<&str> = worktrees
        .iter()
        .map(|w| w["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        [scene.repo.to_string_lossy(), wt.to_string_lossy()],
        "{doc:#}"
    );
    assert_eq!(worktrees[1]["head"], head.as_str());
    assert_eq!(worktrees[1]["branch"], "feat");
    for w in worktrees {
        assert_ne!(w["head"], decoy_head.as_str());
        assert_ne!(w["branch"], "decoy-branch");
        assert_eq!(w["facts"]["dirty"], "no", "{w:#}");
    }
    assert_eq!(
        contents(&decoy.join(".git")),
        before,
        "the decoy is untouched"
    );
}

/// The children see exactly the constructed environment: no inherited `GIT_*` location
/// variables, and the variables that turn off optional locks and prompts.
#[cfg(unix)]
#[test]
fn children_get_the_constructed_environment() {
    use std::os::unix::fs::PermissionsExt;

    let mut scene = Scene::new();
    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH");
    let log = scene.fx.path("git-env.log");
    let wrapper = scene.fx.path("git-wrapper");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nenv >> '{}'\necho --- >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            log.display(),
            real_git.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    scene.observe_config(&format!("git = '{}'", wrapper.display()));

    let decoy = scene.fx.init("decoy");
    let output = scene
        .cardinal()
        .env("GIT_DIR", decoy.join(".git"))
        .env("GIT_WORK_TREE", &decoy)
        .env("GIT_INDEX_FILE", decoy.join(".git/index"))
        .env("GIT_OBJECT_DIRECTORY", decoy.join(".git/objects"))
        .env("GIT_NAMESPACE", "decoy")
        .env("CARDINAL_UNRELATED", "1")
        .args(["inventory", "--repo"])
        .arg(&scene.repo)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    for (what, file) in [("git", log), ("gh", scene.gh_dir.join("env.log"))] {
        let text = std::fs::read_to_string(&file).unwrap_or_else(|_| panic!("{what} ran"));
        assert!(!text.is_empty(), "{what} ran");
        for line in text.lines().filter(|l| *l != "---") {
            let name = line.split('=').next().unwrap();
            assert!(
                !matches!(
                    name,
                    "GIT_DIR"
                        | "GIT_WORK_TREE"
                        | "GIT_INDEX_FILE"
                        | "GIT_OBJECT_DIRECTORY"
                        | "GIT_NAMESPACE"
                        | "GIT_CEILING_DIRECTORIES"
                        | "CARDINAL_UNRELATED"
                        | "CARDINAL_STATE_DIR"
                ),
                "{what} inherited {line}"
            );
        }
        for block in text.split("---\n").filter(|b| !b.trim().is_empty()) {
            for required in [
                "GIT_OPTIONAL_LOCKS=0",
                "GIT_TERMINAL_PROMPT=0",
                "GH_PROMPT_DISABLED=1",
                "GH_NO_UPDATE_NOTIFIER=1",
                "NO_COLOR=1",
            ] {
                assert!(
                    block.lines().any(|l| l == required),
                    "{what} lacks {required}"
                );
            }
        }
    }
}

/// gh is asked about the origin's host, whatever `GH_HOST` or `GH_REPO` the caller exports,
/// and neither reaches gh.
#[test]
fn gh_asks_the_origin_host() {
    let scene = Scene::new();
    let wt = scene.worktree("feat", "feat");
    scene.commit(&wt, "feature.txt");
    scene.push(&wt, "feat");
    let config = scene.config();
    let mut env = scene.env();
    env.push(("GH_HOST".into(), "evil.example.com".into()));
    env.push(("GH_REPO".into(), "evil/repo".into()));
    env.push(("GH_TOKEN".into(), "token".into()));
    let reservations = observe_common::FakeReservations::none();
    let inv = cardinal::observe::Observer::new(&config)
        .with_env(env)
        .with_reservations(&reservations)
        .inventory(
            &scene.state(),
            &cardinal::observe::InventoryOptions {
                repos: vec![scene.repo.clone()],
                sizes: false,
            },
        )
        .unwrap();
    assert_eq!(inv.repos[0].github.as_deref(), Some(observe_common::GITHUB));
    let calls = scene.gh_calls();
    assert!(calls.len() >= 2, "{calls:?}");
    for call in &calls {
        assert!(
            call.contains(&format!("--repo github.com/{}", observe_common::GITHUB)),
            "{call}"
        );
    }
    if cfg!(unix) {
        let env = std::fs::read_to_string(scene.gh_dir.join("env.log")).unwrap();
        let names: Vec<&str> = env.lines().filter_map(|l| l.split('=').next()).collect();
        assert!(!names.contains(&"GH_HOST"), "{names:?}");
        assert!(!names.contains(&"GH_REPO"), "{names:?}");
        assert!(names.contains(&"GH_TOKEN"), "gh keeps its authentication");
    }
}
