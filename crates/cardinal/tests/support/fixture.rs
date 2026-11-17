//! Git fixtures (`docs/architecture.md` section 13.3).
//!
//! Fixtures live in the OS temp dir, outside any repository. Git runs only through
//! [`Fixture::git`] and the named operations, which panic unless the directory, every
//! argument read as a path (symbolic links followed), and the git dir and common dir git
//! resolves for the call are inside the fixture root. The isolated config disables every
//! transport, so only [`Fixture::push`] moves objects between repositories, and only into a
//! [`BareRepo`] the fixture created. The environment is constructed, not inherited: `GIT_DIR`,
//! `GIT_WORK_TREE`, `GIT_INDEX_FILE`, and every other inherited `GIT_*` never reach git, so a
//! test run from a git hook cannot touch the repository the tests run in.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// Fixed identity and dates for every commit a fixture makes.
const IDENTITY: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "Fixture Author"),
    ("GIT_AUTHOR_EMAIL", "author@fixture.invalid"),
    ("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z"),
    ("GIT_COMMITTER_NAME", "Fixture Committer"),
    ("GIT_COMMITTER_EMAIL", "committer@fixture.invalid"),
    ("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z"),
];

/// Non-git variables Windows programs need to start at all.
const WINDOWS_PASSTHROUGH: &[&str] = &[
    "SystemRoot",
    "SystemDrive",
    "TEMP",
    "TMP",
    "PATHEXT",
    "ComSpec",
];

/// Options of `git` itself that take a value in the next argument.
const GLOBAL_WITH_VALUE: &[&str] = &["-C", "-c", "--git-dir", "--work-tree", "--namespace"];

pub struct Fixture {
    _dir: TempDir,
    root: PathBuf,
}

/// A bare repository created by [`Fixture::init_bare`]: the only push target, so a push
/// cannot name a URL or path of its own.
pub struct BareRepo {
    path: PathBuf,
}

impl BareRepo {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Fixture {
    /// A new fixture root in the OS temp dir, with an isolated git configuration that
    /// disables signing, hooks, and every transport (`file` too, so a local path or
    /// `file://` remote cannot write elsewhere; ssh would use the real user's keys).
    pub fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("cardinal-fixture-")
            .tempdir()
            .expect("create the fixture root");
        let root = dunce::canonicalize(dir.path()).expect("canonical fixture root");
        if let Some(repo) = root
            .ancestors()
            .skip(1)
            .find(|dir| std::fs::symlink_metadata(dir.join(".git")).is_ok())
        {
            panic!(
                "fixtures must live outside any repository, but the temp dir {} is inside {}",
                root.display(),
                repo.display()
            );
        }
        for dir in ["home/.config", "hooks"] {
            std::fs::create_dir_all(root.join(dir)).expect("create fixture dirs");
        }
        let hooks = root.join("hooks").to_string_lossy().replace('\\', "/");
        std::fs::write(
            root.join("home/.gitconfig"),
            format!(
                "[core]\n\thooksPath = \"{hooks}\"\n\tautocrlf = false\n\
                 [commit]\n\tgpgsign = false\n[tag]\n\tgpgsign = false\n\
                 [init]\n\tdefaultBranch = main\n[gc]\n\tauto = 0\n[maintenance]\n\tauto = false\n\
                 [protocol]\n\tallow = never\n"
            ),
        )
        .expect("write the fixture git config");
        Self { _dir: dir, root }
    }

    /// The canonical fixture root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `root/rel`; panics if `rel` leaves the root.
    pub fn path(&self, rel: &str) -> PathBuf {
        let path = normalize(&self.root.join(rel));
        assert!(
            path.starts_with(&self.root),
            "{rel} leaves the fixture root"
        );
        path
    }

    /// The environment every child gets: built from scratch, with `PATH` from `inherited`.
    pub fn env_from(&self, inherited: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
        let home = self.root.join("home");
        let ceiling = self.root.parent().unwrap_or(&self.root).to_path_buf();
        let mut env: Vec<(OsString, OsString)> = vec![
            ("HOME".into(), home.clone().into()),
            ("XDG_CONFIG_HOME".into(), home.join(".config").into()),
            ("GIT_CONFIG_GLOBAL".into(), home.join(".gitconfig").into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CEILING_DIRECTORIES".into(), ceiling.into()),
            ("LC_ALL".into(), "C".into()),
        ];
        env.extend(IDENTITY.iter().map(|(k, v)| ((*k).into(), (*v).into())));
        let pass = |name: &OsString| {
            let name = name.to_string_lossy();
            name.eq_ignore_ascii_case("PATH")
                || (cfg!(windows)
                    && WINDOWS_PASSTHROUGH
                        .iter()
                        .any(|keep| name.eq_ignore_ascii_case(keep)))
        };
        if cfg!(windows) {
            env.push(("USERPROFILE".into(), home.into()));
        }
        env.extend(inherited.iter().filter(|(k, _)| pass(k)).cloned());
        env
    }

    /// [`Fixture::env_from`] with this process's environment.
    pub fn env(&self) -> Vec<(OsString, OsString)> {
        self.env_from(&std::env::vars_os().collect::<Vec<_>>())
    }

    /// A command that runs in `dir` (inside the root) with the constructed environment.
    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>, dir: &Path) -> Command {
        self.assert_inside(dir, "directory");
        let mut command = Command::new(program);
        command.env_clear().envs(self.env()).current_dir(dir);
        command
    }

    /// Runs git in `dir` and returns its stdout; panics if git fails or the guard refuses.
    pub fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.git_output(dir, args);
        assert!(
            output.status.success(),
            "git {args:?} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git output is UTF-8")
    }

    /// Runs git in `dir` after the guard checks, without checking its status.
    pub fn git_output(&self, dir: &Path, args: &[&str]) -> Output {
        self.run_git(dir, &[], args)
    }

    fn run_git(&self, dir: &Path, config: &[&str], args: &[&str]) -> Output {
        self.check_args(dir, args);
        self.check_resolved_git_dirs(dir, args);
        self.command("git", dir)
            .args(config)
            .args(args)
            .output()
            .expect("run git")
    }

    /// `git push -q -u <target> branch` from `repo`, which also sets `branch`'s upstream to
    /// the target's path. The target must be a repository of this fixture whose git dir is
    /// inside the root; only this call enables the file transport.
    pub fn push(&self, repo: &Path, target: &BareRepo, branch: &str) {
        self.assert_inside(target.path(), "push target");
        self.check_resolved_git_dirs(target.path(), &[]);
        let path = target.path().to_string_lossy();
        let output = self.run_git(
            repo,
            &["-c", "protocol.file.allow=always"],
            &["push", "-q", "-u", &path, branch],
        );
        assert!(
            output.status.success(),
            "git push {path} {branch} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// `git init` of a new repository `root/name` on branch `main`.
    pub fn init(&self, name: &str) -> PathBuf {
        let repo = self.path(name);
        self.git(&self.root, &["init", "-q", "-b", "main", "--", name]);
        repo
    }

    /// `git init --bare` of a new repository `root/name`.
    pub fn init_bare(&self, name: &str) -> BareRepo {
        let path = self.path(name);
        self.git(
            &self.root,
            &["init", "-q", "--bare", "-b", "main", "--", name],
        );
        BareRepo { path }
    }

    /// Writes `rel` in `repo`, commits it, and returns the new HEAD.
    pub fn commit_file(&self, repo: &Path, rel: &str, contents: &str, message: &str) -> String {
        let file = normalize(&repo.join(rel));
        self.assert_inside(&file, "file");
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&file, contents).expect("write the file");
        self.git(repo, &["add", "--", rel]);
        self.git(repo, &["commit", "-q", "-m", message]);
        self.git(repo, &["rev-parse", "HEAD"]).trim().to_owned()
    }

    /// `git worktree add -b branch root/name`; returns the worktree path.
    pub fn worktree_add(&self, repo: &Path, name: &str, branch: &str) -> PathBuf {
        let path = self.path(name);
        let target = path.to_string_lossy().into_owned();
        self.git(repo, &["worktree", "add", "-q", "-b", branch, &target]);
        path
    }

    fn assert_inside(&self, path: &Path, what: &str) {
        let resolved = resolve(path);
        assert!(
            resolved.starts_with(&self.root),
            "refusing git: {what} {} is outside the fixture root {}",
            path.display(),
            self.root.display()
        );
    }

    /// Each `-C` directory, then every argument read as a path relative to the directory
    /// git ends up in, must resolve inside the root. Arguments that are not paths resolve
    /// inside it trivially.
    fn check_args(&self, dir: &Path, args: &[&str]) {
        self.assert_inside(dir, "directory");
        let mut cwd = dir.to_path_buf();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            if !arg.starts_with('-') {
                break;
            }
            if *arg == "-C" {
                if let Some(value) = rest.next() {
                    cwd = cwd.join(value);
                    self.assert_inside(&cwd, "directory");
                }
            } else if GLOBAL_WITH_VALUE.contains(arg) {
                rest.next();
            }
        }
        for arg in args {
            self.check_arg(&cwd, arg);
        }
    }

    /// The argument itself, its `name=value` value, and an attached short-option value
    /// (`-C/abs`) must not name a home directory and must resolve inside the root. Settings
    /// that re-enable a transport are refused outright.
    fn check_arg(&self, cwd: &Path, arg: &str) {
        let lower = arg.to_ascii_lowercase();
        assert!(
            !(lower.contains("protocol.") && lower.contains("allow")),
            "refusing git: argument {arg} changes which transports git may use"
        );
        let mut candidates = vec![arg];
        if let Some((_, value)) = arg.split_once('=') {
            candidates.push(value);
        }
        if arg.starts_with('-') && !arg.starts_with("--") {
            candidates.extend(arg.get(2..).filter(|value| !value.is_empty()));
        }
        for candidate in candidates {
            assert!(
                !candidate.starts_with('~'),
                "refusing git: argument {arg} names a home directory"
            );
            self.assert_inside(&cwd.join(candidate), "argument");
        }
    }

    /// Asks git which git dir and common dir the call would use, with the same global
    /// options, and refuses any outside the root (a decoy `.git` file pointing elsewhere).
    fn check_resolved_git_dirs(&self, dir: &Path, args: &[&str]) {
        let mut global = Vec::new();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            if !arg.starts_with('-') {
                break;
            }
            global.push(*arg);
            if GLOBAL_WITH_VALUE.contains(arg) {
                global.extend(rest.next());
            }
        }
        let output = self
            .command("git", dir)
            .args(&global)
            .args([
                "rev-parse",
                "--path-format=absolute",
                "--absolute-git-dir",
                "--git-common-dir",
            ])
            .output()
            .expect("run git rev-parse");
        if !output.status.success() {
            // Not inside a repository (git init, git clone): nothing to resolve.
            return;
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            self.assert_inside(Path::new(line.trim()), "git dir");
        }
    }
}

/// Symbolic link chains longer than this are not followed further.
const MAX_LINKS: u32 = 40;

/// Removes `.` and `..` lexically.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Where `path` leads on disk. Each symbolic link along it is followed, dangling ones too,
/// before a later `..` applies, as the OS would; then the longest existing ancestor is
/// canonicalized.
fn resolve(path: &Path) -> PathBuf {
    canonical_prefix(&follow_links(path, 0))
}

fn follow_links(path: &Path, depth: u32) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);
                if depth < MAX_LINKS
                    && let Ok(target) = std::fs::read_link(&out)
                {
                    let base = out.parent().map_or_else(PathBuf::new, Path::to_path_buf);
                    out = follow_links(&base.join(target), depth + 1);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `path` with its longest existing ancestor canonicalized, followed by the rest.
fn canonical_prefix(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = dunce::canonicalize(existing) {
            let mut out = canonical;
            out.extend(rest.iter().rev());
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}
