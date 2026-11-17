//! A repository scene for the observation tests: a main worktree whose `origin` is a
//! GitHub URL, remote-tracking refs written directly (observation never fetches, and the
//! fixture disables every transport), a fake `gh` answering from files, and a cardinal
//! configuration that points at it. Reservations are the real ones of the scene's state
//! dir, read through `admission`.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of the scene"
)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cardinal::config::Config;
use cardinal::contract::{Error, Inventory, Worktree};
use cardinal::observe::{InventoryOptions, Observer, WorktreeObservation};
use cardinal::state::StateDir;

use cardinal_test_support::Command;
use cardinal_test_support::fixture::Fixture;

pub const GITHUB: &str = "acme/app";

pub struct Scene {
    pub fx: Fixture,
    pub repo: PathBuf,
    pub gh_dir: PathBuf,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    /// Extra `host.toml` lines appended to the generated `[observe]` table.
    observe_extra: String,
    /// `[observe] gh_timeout_ms`: long enough for a loaded machine by default.
    pub gh_timeout_ms: u64,
    /// `[observe] git_timeout_ms`: long enough for a loaded machine by default.
    pub git_timeout_ms: u64,
}

impl Scene {
    /// A repository `repo` on `main` with one commit, `origin` set to a GitHub URL, and
    /// `origin/main` (also `origin/HEAD`) at that commit as `main`'s upstream.
    pub fn new() -> Self {
        let fx = Fixture::new();
        let repo = fx.init("repo");
        fx.commit_file(&repo, "README.md", "hello\n", "initial");
        fx.remote_add_url(&repo, "origin", &format!("https://github.com/{GITHUB}.git"));
        let gh_dir = fx.path("gh");
        let config_dir = fx.path("config");
        let state_dir = fx.path("state");
        std::fs::create_dir_all(&gh_dir).unwrap();
        std::fs::write(fx.path("observe.gitconfig"), "").unwrap();
        std::fs::create_dir_all(config_dir.join("repos")).unwrap();
        let scene = Self {
            fx,
            repo,
            gh_dir,
            config_dir,
            state_dir,
            observe_extra: String::new(),
            gh_timeout_ms: 15_000,
            git_timeout_ms: 15_000,
        };
        scene.push(&scene.repo, "main");
        scene.fx.symbolic_ref(
            &scene.repo,
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        );
        scene.write_gh();
        scene.set_open("[]");
        scene.set_merged_default("[]");
        scene
    }

    /// The HEAD object ID of `dir`.
    pub fn head(&self, dir: &Path) -> String {
        self.fx.rev_parse(dir, "HEAD")
    }

    /// A linked worktree `name` on a new branch `branch` from the main worktree's HEAD.
    pub fn worktree(&self, name: &str, branch: &str) -> PathBuf {
        self.fx.worktree_add(&self.repo, name, branch)
    }

    /// A commit in `dir`; returns the new HEAD.
    pub fn commit(&self, dir: &Path, file: &str) -> String {
        self.fx.commit_file(dir, file, &format!("{file}\n"), file)
    }

    /// Records `branch` of `dir` as pushed: `origin/<branch>` at its HEAD, and that ref as
    /// its upstream (what a push and fetch would leave behind).
    pub fn push(&self, dir: &Path, branch: &str) {
        let head = self.head(dir);
        self.fx
            .update_ref(dir, &format!("refs/remotes/origin/{branch}"), &head);
        self.fx
            .set_upstream(dir, branch, &format!("origin/{branch}"));
    }

    /// Moves `origin/main` to `oid`, as a merge on GitHub followed by a fetch would.
    pub fn merge_into_base(&self, oid: &str) {
        self.fx
            .update_ref(&self.repo, "refs/remotes/origin/main", oid);
    }

    /// The open pull request list gh returns.
    pub fn set_open(&self, json: &str) {
        std::fs::write(self.gh_dir.join("open.out"), json).unwrap();
    }

    /// The merged pull requests gh returns when searching for `oid`.
    pub fn set_merged(&self, oid: &str, json: &str) {
        std::fs::write(self.gh_dir.join(format!("merged-{oid}.out")), json).unwrap();
    }

    /// The merged pull requests gh returns for any other search.
    pub fn set_merged_default(&self, json: &str) {
        std::fs::write(self.gh_dir.join("merged.out"), json).unwrap();
    }

    /// Makes the `kind` (`open` or `merged`) query exit with `code`.
    pub fn gh_fails(&self, kind: &str, code: u8) {
        std::fs::write(self.gh_dir.join(format!("{kind}.code")), code.to_string()).unwrap();
    }

    /// Makes the `kind` query write `line` to stderr.
    pub fn gh_stderr(&self, kind: &str, line: &str) {
        std::fs::write(self.gh_dir.join(format!("{kind}.err")), format!("{line}\n")).unwrap();
    }

    /// Makes the merged pull request search for `oid` fall back to the default answer.
    pub fn unset_merged(&self, oid: &str) {
        std::fs::remove_file(self.gh_dir.join(format!("merged-{oid}.out"))).unwrap();
    }

    /// Makes the `kind` query hang past any deadline.
    pub fn gh_hangs(&self, kind: &str) {
        std::fs::write(self.gh_dir.join(format!("{kind}.sleep")), "").unwrap();
    }

    /// Every gh invocation so far, one line of arguments each.
    pub fn gh_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.gh_dir.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The fake gh program.
    pub fn gh_program(&self) -> PathBuf {
        if cfg!(windows) {
            self.gh_dir.join("gh.cmd")
        } else {
            self.gh_dir.join("gh")
        }
    }

    #[cfg(unix)]
    fn write_gh(&self) {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.gh_dir.display();
        let script = format!(
            r#"#!/bin/sh
dir='{dir}'
printf '%s\n' "$*" >> "$dir/calls.log"
env > "$dir/env.log"
kind=""; search=""; prev=""
for a in "$@"; do
  [ "$prev" = "--state" ] && kind=$a
  [ "$prev" = "--search" ] && search=$a
  prev=$a
done
[ -f "$dir/$kind.sleep" ] && exec sleep 30
f="$dir/$kind-$search.out"; [ -f "$f" ] || f="$dir/$kind.out"
[ -f "$f" ] && cat "$f"
[ -f "$dir/$kind.err" ] && cat "$dir/$kind.err" >&2
code=0; [ -f "$dir/$kind.code" ] && code=$(cat "$dir/$kind.code")
exit "$code"
"#
        );
        let path = self.gh_program();
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(windows)]
    fn write_gh(&self) {
        let dir = self.gh_dir.display();
        let script = format!(
            "@echo off\r\n\
             setlocal enabledelayedexpansion\r\n\
             echo %*>> \"{dir}\\calls.log\"\r\n\
             set kind=\r\nset search=\r\nset prev=\r\n\
             for %%a in (%*) do (\r\n\
               if \"!prev!\"==\"--state\" set kind=%%~a\r\n\
               if \"!prev!\"==\"--search\" set search=%%~a\r\n\
               set prev=%%~a\r\n\
             )\r\n\
             if exist \"{dir}\\!kind!.sleep\" ping -n 31 127.0.0.1 >nul\r\n\
             set f={dir}\\!kind!-!search!.out\r\n\
             if not exist \"!f!\" set f={dir}\\!kind!.out\r\n\
             if exist \"!f!\" type \"!f!\"\r\n\
             if exist \"{dir}\\!kind!.err\" type \"{dir}\\!kind!.err\" 1>&2\r\n\
             set code=0\r\n\
             if exist \"{dir}\\!kind!.code\" set /p code=<\"{dir}\\!kind!.code\"\r\n\
             exit /b !code!\r\n"
        );
        std::fs::write(self.gh_program(), script).unwrap();
    }

    /// Appends lines to the `[observe]` table of `host.toml`.
    pub fn observe_config(&mut self, lines: &str) {
        self.observe_extra.push_str(lines);
        self.observe_extra.push('\n');
    }

    /// Writes `repos/<name>.toml`.
    pub fn profile(&self, name: &str, toml: &str) {
        std::fs::write(
            self.config_dir.join("repos").join(format!("{name}.toml")),
            toml,
        )
        .unwrap();
    }

    /// Writes `host.toml` and loads the configuration.
    pub fn config(&self) -> Config {
        let host = format!(
            "[observe]\ngh = '{}'\ngh_timeout_ms = {}\ngit_timeout_ms = {}\n{}",
            self.gh_program().display(),
            self.gh_timeout_ms,
            self.git_timeout_ms,
            self.observe_extra
        );
        std::fs::write(self.config_dir.join("host.toml"), host).unwrap();
        Config::load(&self.config_dir).unwrap()
    }

    pub fn state(&self) -> StateDir {
        StateDir::open(&self.state_dir).unwrap()
    }

    /// What observation's children get on top of the fixture's environment: an empty
    /// global git configuration, and gh's configuration inside the fixture, so neither the
    /// owner's git nor gh configuration (or keychain entry) is ever read.
    fn isolation(&self) -> [(OsString, OsString); 2] {
        [
            (
                "GIT_CONFIG_GLOBAL".into(),
                self.fx.path("observe.gitconfig").into(),
            ),
            (
                "GH_CONFIG_DIR".into(),
                self.fx.path("home/.config/gh").into(),
            ),
        ]
    }

    /// The environment observation builds its children from: the fixture's (`HOME`,
    /// `XDG_CONFIG_HOME` inside the fixture, `GIT_CONFIG_NOSYSTEM=1`, `PATH` only from this
    /// process) with [`Scene::isolation`] on top.
    pub fn env(&self) -> Vec<(OsString, OsString)> {
        let isolation = self.isolation();
        let mut env: Vec<(OsString, OsString)> = self
            .fx
            .scope()
            .env()
            .into_iter()
            .filter(|(k, _)| !isolation.iter().any(|(name, _)| name == k))
            .collect();
        env.extend(isolation);
        env
    }

    /// `inventory --repo <repo>` through the library, with `--sizes` if `sizes`.
    pub fn inventory_with(&self, sizes: bool) -> Inventory {
        let config = self.config();
        Observer::new(&config)
            .with_env(self.env())
            .inventory(
                &self.state(),
                &InventoryOptions {
                    repos: vec![self.repo.clone()],
                    sizes,
                },
            )
            .unwrap()
    }

    /// `inventory --repo <repo>`.
    pub fn inventory(&self) -> Inventory {
        self.inventory_with(false)
    }

    /// `observe_worktree` of `path`.
    pub fn observe(&self, path: &Path) -> Result<WorktreeObservation, Error> {
        let config = self.config();
        Observer::new(&config)
            .with_env(self.env())
            .observe_worktree(&self.state(), path)
    }

    /// The `cardinal` binary with this scene's state and config directories, the scene's
    /// environment (`env_clear` first), and the fixture root as its working directory. Every
    /// caller passes `--repo <fixture repository>`.
    pub fn cardinal(&self) -> Command {
        self.config();
        let mut command = self
            .fx
            .command(env!("CARGO_BIN_EXE_cardinal"), self.fx.root());
        for (name, value) in self.isolation() {
            command.env(name, value);
        }
        command
            .env("CARDINAL_STATE_DIR", &self.state_dir)
            .env("CARDINAL_CONFIG_DIR", &self.config_dir);
        command
    }
}

/// The worktree at `path` in an inventory.
pub fn worktree<'a>(inventory: &'a Inventory, path: &Path) -> &'a Worktree {
    let path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    inventory
        .repos
        .iter()
        .flat_map(|r| &r.worktrees)
        .find(|w| Path::new(&w.path) == path)
        .unwrap_or_else(|| panic!("{} not in {inventory:#?}", path.display()))
}

/// A merged pull request list with one entry.
pub fn merged_json(number: u64, head: &str) -> String {
    format!(
        r#"[{{"number":{number},"headRefOid":"{head}","mergedAt":"2026-01-02T00:00:00Z","url":"https://github.com/{GITHUB}/pull/{number}"}}]"#
    )
}

/// An open pull request list entry.
pub fn open_entry(number: u64, branch: &str, head: &str, cross: bool) -> String {
    format!(
        r#"{{"number":{number},"headRefName":"{branch}","headRefOid":"{head}","isCrossRepository":{cross},"url":"https://github.com/{GITHUB}/pull/{number}"}}"#
    )
}

/// Upper bound for a call that must give up at its deadline.
pub const PROMPT: Duration = Duration::from_secs(20);
