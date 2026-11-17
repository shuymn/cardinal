//! Git fixtures (`docs/architecture.md` section 13.3): repositories inside a [`Scope`],
//! changed and read only through typed operations.
//!
//! No operation takes free-form git arguments or hands out a command to extend: each builds
//! its own arguments from names, paths, and values it checks. Before git runs, the directory
//! must be inside the root, and git's own answer to which git dir and common dir the call
//! would use must be inside it too (a decoy `.git` file pointing elsewhere is refused). A
//! directory that is not a repository is told apart from any other failure to resolve one.
//! The scope's git config disables every transport, so only [`Fixture::push`] moves objects,
//! and only into a [`BareRepo`] of this fixture.

#![allow(
    clippy::must_use_candidate,
    reason = "git runs for its effect; a test often ignores what an operation returns"
)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Output;

use crate::scope::{Command, Scope, normalize};

/// The config sections a test may write: branch metadata and keys of its own. Everything else
/// (`core`, `alias`, `include`, `protocol`, `remote`, `credential`, …) can run programs, move
/// data elsewhere, or change which repository git uses.
const CONFIG_SECTIONS: &[&str] = &["branch", "cardinal", "test"];

pub struct Fixture {
    scope: Scope,
}

/// A bare repository created by [`Fixture::init_bare`]: the only push target and remote, so
/// neither can name a URL or path of its own.
pub struct BareRepo {
    path: PathBuf,
}

impl BareRepo {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A filesystem monitor configured for one fixture repository by [`Fixture::fsmonitor`].
/// Dropping it stops the monitor daemon running for the repository, if any
/// (`git fsmonitor--daemon stop`), and removes the setting, but only while the directory
/// still resolves to the git dirs it had when the monitor was configured: a `.git` replaced
/// meanwhile (say, by a gitfile naming another repository) is left alone, and a daemon left
/// running is the scope's to clean up.
pub struct Fsmonitor<'a> {
    fixture: &'a Fixture,
    repo: PathBuf,
    dirs: Vec<PathBuf>,
}

impl Drop for Fsmonitor<'_> {
    fn drop(&mut self) {
        if self.fixture.git_dirs(&self.repo).as_ref() != Some(&self.dirs) {
            return;
        }
        self.fixture
            .output(&self.repo, &["fsmonitor--daemon", "stop"]);
        self.fixture.output(
            &self.repo,
            &["config", "--local", "--unset", "core.fsmonitor"],
        );
    }
}

/// Which repository git would use in a directory.
enum Resolution {
    Repository,
    NotARepository,
}

impl Fixture {
    pub fn new() -> Self {
        Self {
            scope: Scope::new(),
        }
    }

    /// The scope that owns the fixture root, for starting other processes in it.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// The canonical fixture root.
    pub fn root(&self) -> &Path {
        self.scope.root()
    }

    /// `root/rel`; panics if `rel` leaves the root.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.scope.path(rel)
    }

    /// See [`Scope::env_from`].
    pub fn env_from(&self, inherited: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
        self.scope.env_from(inherited)
    }

    /// See [`Scope::command`].
    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>, dir: &Path) -> Command {
        self.scope.command(program, dir)
    }

    /// `git init` of a new repository `root/name` on branch `main`.
    pub fn init(&self, name: &str) -> PathBuf {
        let repo = self.path(name);
        self.resolve(self.root());
        self.run(self.root(), &["init", "-q", "-b", "main", "--", name]);
        repo
    }

    /// `git init --bare` of a new repository `root/name`.
    pub fn init_bare(&self, name: &str) -> BareRepo {
        let path = self.path(name);
        self.resolve(self.root());
        self.run(
            self.root(),
            &["init", "-q", "--bare", "-b", "main", "--", name],
        );
        BareRepo { path }
    }

    /// Writes `rel` in `repo`, commits it, and returns the new HEAD.
    pub fn commit_file(&self, repo: &Path, rel: &str, contents: &str, message: &str) -> String {
        let file = self.file(repo, rel);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&file, contents).expect("write the file");
        self.add(repo, rel);
        self.commit(repo, message)
    }

    /// `git add -- rel`.
    pub fn add(&self, repo: &Path, rel: &str) {
        self.file(repo, rel);
        self.git(repo, &["add", "--", rel]);
    }

    /// `git commit -q -m message` of what is staged; returns the new HEAD.
    pub fn commit(&self, repo: &Path, message: &str) -> String {
        self.git(repo, &["commit", "-q", "-m", message]);
        self.rev_parse(repo, "HEAD")
    }

    /// `git worktree add -b branch root/name`; returns the worktree path.
    pub fn worktree_add(&self, repo: &Path, name: &str, branch: &str) -> PathBuf {
        let branch = checked_name("branch", branch);
        let path = self.path(name);
        self.assert_inside(&path, "worktree");
        let target = path.to_string_lossy().into_owned();
        self.git(
            repo,
            &["worktree", "add", "-q", "-b", branch, "--", &target],
        );
        path
    }

    /// `git push -q -u <target> branch` from `repo`, which also sets `branch`'s upstream to
    /// the target's path. Only this call enables the file transport.
    pub fn push(&self, repo: &Path, target: &BareRepo, branch: &str) {
        let branch = checked_name("branch", branch);
        self.assert_inside(target.path(), "push target");
        self.require_repo(target.path());
        let path = target.path().to_string_lossy();
        self.git(
            repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "push",
                "-q",
                "-u",
                &path,
                branch,
            ],
        );
    }

    /// `git remote add name <target>`.
    pub fn remote_add(&self, repo: &Path, name: &str, target: &BareRepo) {
        let name = checked_name("remote", name);
        self.assert_inside(target.path(), "remote");
        let path = target.path().to_string_lossy();
        self.git(repo, &["remote", "add", name, &path]);
    }

    /// `git remote add name url`, for a remote that git only reports, such as a GitHub URL a
    /// test needs as `origin`. `url` must be `https://` with a plain host and path; no
    /// transport is enabled, so git never reaches it.
    pub fn remote_add_url(&self, repo: &Path, name: &str, url: &str) {
        let name = checked_name("remote", name);
        self.git(repo, &["remote", "add", name, checked_url(url)]);
    }

    /// `git remote set-url name url`, with `url` checked as in [`Fixture::remote_add_url`].
    pub fn remote_set_url(&self, repo: &Path, name: &str, url: &str) {
        let name = checked_name("remote", name);
        self.git(repo, &["remote", "set-url", name, checked_url(url)]);
    }

    /// `git push -q remote branch` through a named remote, with no transport enabled: the
    /// output shows git refusing it.
    pub fn push_remote(&self, repo: &Path, remote: &str, branch: &str) -> Output {
        let remote = checked_name("remote", remote);
        let branch = checked_name("branch", branch);
        self.require_repo(repo);
        self.output(repo, &["push", "-q", remote, branch])
    }

    /// `git branch name`.
    pub fn branch(&self, repo: &Path, name: &str) {
        self.git(repo, &["branch", "--", checked_name("branch", name)]);
    }

    /// `git switch name`: checks out an existing branch.
    pub fn switch(&self, repo: &Path, name: &str) {
        self.git(repo, &["switch", "-q", "--", checked_name("branch", name)]);
    }

    /// `git switch --detach`: detaches HEAD at the current commit.
    pub fn detach(&self, repo: &Path) {
        self.git(repo, &["switch", "-q", "--detach"]);
    }

    /// `git tag name`.
    pub fn tag(&self, repo: &Path, name: &str) {
        self.git(repo, &["tag", "--", checked_name("tag", name)]);
    }

    /// `git update-ref refname rev`.
    pub fn update_ref(&self, repo: &Path, refname: &str, rev: &str) {
        self.git(
            repo,
            &[
                "update-ref",
                checked_name("ref", refname),
                checked_name("revision", rev),
            ],
        );
    }

    /// `git symbolic-ref name target`: points the ref `name` at the ref `target`.
    pub fn symbolic_ref(&self, repo: &Path, name: &str, target: &str) {
        self.git(
            repo,
            &[
                "symbolic-ref",
                checked_name("ref", name),
                checked_name("ref", target),
            ],
        );
    }

    /// `git symbolic-ref --delete name`.
    pub fn symbolic_ref_delete(&self, repo: &Path, name: &str) {
        self.git(
            repo,
            &["symbolic-ref", "--delete", checked_name("ref", name)],
        );
    }

    /// `git branch --set-upstream-to=upstream branch`: records `upstream`, a branch or
    /// remote-tracking branch, as `branch`'s upstream.
    pub fn set_upstream(&self, repo: &Path, branch: &str, upstream: &str) {
        let upstream = format!("--set-upstream-to={}", checked_name("upstream", upstream));
        self.git(
            repo,
            &["branch", &upstream, "--", checked_name("branch", branch)],
        );
    }

    /// `git worktree lock -- worktree`: marks a linked worktree of `repo` as locked.
    pub fn worktree_lock(&self, repo: &Path, worktree: &Path) {
        self.assert_inside(worktree, "worktree");
        let target = worktree.to_string_lossy();
        self.git(repo, &["worktree", "lock", "--", &target]);
    }

    /// `git update-index --split-index`: the index of `repo` becomes a split index, whose
    /// entries live in a `sharedindex.*` file of the git dir.
    pub fn split_index(&self, repo: &Path) {
        self.git(repo, &["update-index", "--split-index"]);
    }

    /// Sets `core.fsmonitor=true` in `repo`'s local config, overriding the scope's
    /// `core.fsmonitor=false`, until the returned guard is dropped. Meanwhile a git command that
    /// reads the index of `repo` may start git's filesystem monitor daemon (macOS and Windows),
    /// which runs inside the fixture repository and leaves the process group of the git that
    /// started it.
    pub fn fsmonitor(&self, repo: &Path) -> Fsmonitor<'_> {
        self.git(repo, &["config", "--local", "core.fsmonitor", "true"]);
        let dirs = self
            .git_dirs(repo)
            .expect("resolve the repository a monitor is configured for");
        Fsmonitor {
            fixture: self,
            repo: repo.to_path_buf(),
            dirs,
        }
    }

    /// Whether git's filesystem monitor daemon runs for the repository at `dir`:
    /// `git fsmonitor--daemon status`. Where git has no built-in monitor (Linux), none runs.
    pub fn fsmonitor_running(&self, dir: &Path) -> bool {
        self.require_repo(dir);
        self.output(dir, &["fsmonitor--daemon", "status"])
            .status
            .success()
    }

    /// `git stash push -q`.
    pub fn stash_push(&self, repo: &Path) {
        self.git(repo, &["stash", "push", "-q"]);
    }

    /// `git config --local key value`, for keys in the `branch`, `cardinal`, and `test`
    /// sections only.
    pub fn config_set(&self, repo: &Path, key: &str, value: &str) {
        let key = checked_name("config key", key);
        let section = key.split('.').next().unwrap_or_default();
        assert!(
            key.contains('.')
                && CONFIG_SECTIONS
                    .iter()
                    .any(|allowed| section.eq_ignore_ascii_case(allowed)),
            "refusing git: config key {key} is outside the sections tests may set"
        );
        self.git(repo, &["config", "--local", key, value]);
    }

    /// `git config key`, or `None` when it is unset.
    pub fn config_get(&self, repo: &Path, key: &str) -> Option<String> {
        let key = checked_name("config key", key);
        self.require_repo(repo);
        let output = self.output(repo, &["config", "--get", key]);
        output.status.success().then(|| {
            String::from_utf8(output.stdout)
                .expect("git output is UTF-8")
                .trim_end()
                .to_owned()
        })
    }

    /// `git rev-parse --verify rev`.
    pub fn rev_parse(&self, repo: &Path, rev: &str) -> String {
        let rev = checked_name("revision", rev);
        self.git(repo, &["rev-parse", "--verify", rev])
            .trim()
            .to_owned()
    }

    /// `git show-ref --head`.
    pub fn show_ref(&self, repo: &Path) -> String {
        self.git(repo, &["show-ref", "--head"])
    }

    /// `git for-each-ref`.
    pub fn for_each_ref(&self, repo: &Path) -> String {
        self.git(repo, &["for-each-ref"])
    }

    /// `git log --format=format`.
    pub fn log(&self, repo: &Path, format: &str) -> String {
        self.git(repo, &["log", &format!("--format={format}")])
    }

    /// `git status --porcelain`.
    pub fn status(&self, repo: &Path) -> String {
        self.git(repo, &["status", "--porcelain"])
    }

    /// `rel` inside `repo`, checked to resolve inside the root.
    fn file(&self, repo: &Path, rel: &str) -> PathBuf {
        assert!(
            !rel.starts_with('-'),
            "refusing git: path {rel} reads as an option"
        );
        let file = normalize(&repo.join(rel));
        self.assert_inside(&file, "file");
        file
    }

    /// Runs git in `repo`, which must be a repository of this fixture; panics if git fails.
    fn git(&self, repo: &Path, args: &[&str]) -> String {
        self.require_repo(repo);
        self.run(repo, args)
    }

    fn run(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.output(dir, args);
        assert!(
            output.status.success(),
            "git {args:?} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git output is UTF-8")
    }

    fn output(&self, dir: &Path, args: &[&str]) -> Output {
        self.assert_inside(dir, "directory");
        self.scope
            .command("git", dir)
            .args(args)
            .output()
            .expect("run git")
    }

    fn require_repo(&self, dir: &Path) {
        match self.resolve(dir) {
            Resolution::Repository => {}
            Resolution::NotARepository => {
                panic!("refusing git: {} is not a repository", dir.display())
            }
        }
    }

    /// The git dir and common dir git would use in `dir`, or `None` when it cannot resolve
    /// them. Never panics for a directory inside the root.
    fn git_dirs(&self, dir: &Path) -> Option<Vec<PathBuf>> {
        let output = self.output(
            dir,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--absolute-git-dir",
                "--git-common-dir",
            ],
        );
        output.status.success().then(|| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|line| PathBuf::from(line.trim()))
                .collect()
        })
    }

    /// Asks git which git dir and common dir it would use in `dir`, and refuses any outside
    /// the root. Panics on any failure other than "not a git repository".
    fn resolve(&self, dir: &Path) -> Resolution {
        let output = self.output(
            dir,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--absolute-git-dir",
                "--git-common-dir",
            ],
        );
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.starts_with("fatal: not a git repository (or any"),
                "git cannot resolve the repository at {}: {stderr}",
                dir.display()
            );
            return Resolution::NotARepository;
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            self.assert_inside(Path::new(line.trim()), "git dir");
        }
        Resolution::Repository
    }

    fn assert_inside(&self, path: &Path, what: &str) {
        assert!(
            self.scope.contains(path),
            "refusing git: {what} {} is outside the fixture root {}",
            path.display(),
            self.root().display()
        );
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

/// A branch, tag, ref, remote, revision, or config key: not an option, not a path out, only
/// characters such names need.
fn checked_name<'a>(what: &str, name: &'a str) -> &'a str {
    assert!(
        !name.is_empty()
            && !name.starts_with('-')
            && !name.contains("..")
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-')),
        "refusing git: {what} {name:?} is not a plain name"
    );
    name
}

/// A remote URL git only reports: `https://`, then a host and path of plain characters.
fn checked_url(url: &str) -> &str {
    let rest = url.strip_prefix("https://").unwrap_or_default();
    assert!(
        rest.contains('/')
            && !rest.starts_with('/')
            && !rest.contains("..")
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-')),
        "refusing git: remote URL {url:?} is not a plain https URL"
    );
    url
}
