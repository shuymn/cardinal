//! The source commit `cardinal --version` names (`docs/architecture.md` section 11.2), read
//! by `build.rs` at build time and tested by `tests/version.rs`.
//!
//! Git is asked, never its files parsed: which repository the manifest directory is in, where
//! that repository keeps HEAD and refs, HEAD's commit and date, and whether a tracked file
//! differs from HEAD. The caller runs git; [`read`] gets git's output, so the build script
//! spawns it with the environment of [`git_env`] and the tests spawn it through
//! `cardinal-test-support`. Any step that fails leaves the commit unknown, and the version
//! is then the package version alone. A crate package is never read through git: whatever
//! repository holds it is not the one its sources came from.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Where cardinal's repository keeps the crate. `git rev-parse --show-prefix` in the manifest
/// directory prints it in a checkout of cardinal, and something else when the sources are
/// inside another repository (vendored into it, or a crate package unpacked in its tree),
/// whose commit is not the one the sources came from.
const PREFIX: &str = "crates/cardinal/";

/// The files `cargo package` writes into a crate package's root: the manifest as written
/// (`Cargo.toml.orig`, beside the normalized `Cargo.toml`) in every package, and the commit
/// packaged (`.cargo_vcs_info.json`) in one made from a checkout. Either marks sources
/// unpacked from a package, wherever they sit.
const PACKAGE_MARKERS: &[&str] = &["Cargo.toml.orig", ".cargo_vcs_info.json"];

/// The longest chain of symbolic refs git follows (`SYMREF_MAXDEPTH`).
const SYMREF_DEPTH: usize = 5;

/// The options before each git command: no optional locks, so `status` does not write the
/// index, and no filesystem monitor, which a repository's configuration could otherwise
/// start as a daemon (as observation runs git, section 8.1).
pub const GIT_OPTIONS: &[&str] = &["--no-optional-locks", "-c", "core.fsmonitor=false"];

/// The variables git gets from the build's environment: where to find git, its configuration
/// (so a `safe.directory` the builder set applies), and on Windows what programs need to
/// start. `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, and every other variable that would
/// point git at another repository do not pass.
const INHERITED: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "XDG_CONFIG_HOME",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
];

/// The variables Windows programs need, passed on Windows only.
const INHERITED_WINDOWS: &[&str] = &[
    "SystemRoot",
    "SystemDrive",
    "ComSpec",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "TEMP",
    "TMP",
];

/// The variables git always gets: no optional locks, no lazy fetch of a partial clone's
/// missing objects, no prompts.
pub const GIT_ADDED: &[(&str, &str)] = &[
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("GIT_NO_LAZY_FETCH", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// The environment git runs with, built from `inherited`: the [`INHERITED`] variables (and
/// on Windows [`INHERITED_WINDOWS`], whose names match in any case there), then
/// [`GIT_ADDED`].
#[must_use]
pub fn git_env(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    windows: bool,
) -> Vec<(OsString, OsString)> {
    let pass = |name: &str| {
        if windows {
            INHERITED
                .iter()
                .chain(INHERITED_WINDOWS)
                .any(|keep| name.eq_ignore_ascii_case(keep))
        } else {
            INHERITED.contains(&name)
        }
    };
    let mut env: Vec<(OsString, OsString)> = inherited
        .into_iter()
        .filter(|(name, _)| name.to_str().is_some_and(pass))
        .collect();
    env.extend(GIT_ADDED.iter().map(|(k, v)| ((*k).into(), (*v).into())));
    env
}

/// HEAD's commit when the sources were built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    /// The commit ID abbreviated to 9 hex digits, or more where 9 are ambiguous, as cargo and
    /// rustc print theirs.
    pub short: String,
    /// The committer date, `YYYY-MM-DD` in the committer's time zone.
    pub date: String,
    /// Whether a tracked file differed from HEAD, in the index or the working tree.
    pub dirty: bool,
}

/// What the build reads from git.
#[derive(Debug, Default)]
pub struct Source {
    /// HEAD's commit, `None` when the sources are in no checkout of cardinal or git failed.
    pub commit: Option<Commit>,
    /// The existing files and directories git changes when HEAD comes to name another commit:
    /// the worktree's `HEAD`, the file of each ref of the chain HEAD names where git keeps it
    /// (`rev-parse --git-path`: a linked worktree's own refs, `refs/worktree/*` among them, in
    /// its git dir, the others in the common dir), `packed-refs`, and a reftable stack. A ref
    /// that is only packed gets the nearest existing directory that will hold its loose file,
    /// since cargo reruns a build script on every build while a path it watches is missing.
    pub watch: Vec<PathBuf>,
}

/// Reads the source commit of the crate in `manifest` through `git`, which runs git with the
/// given arguments (after [`GIT_OPTIONS`]) there and returns its stdout when it exits 0.
pub fn read(manifest: &Path, git: impl Fn(&[&str]) -> Option<String>) -> Source {
    if PACKAGE_MARKERS
        .iter()
        .any(|marker| manifest.join(marker).exists())
    {
        return Source::default();
    }
    let Some(dirs) = git(&[
        "rev-parse",
        "--path-format=absolute",
        "--show-prefix",
        "--absolute-git-dir",
        "--git-common-dir",
    ]) else {
        return Source::default();
    };
    let mut lines = dirs.lines();
    let (Some(PREFIX), Some(git_dir), Some(common_dir)) =
        (lines.next(), lines.next(), lines.next())
    else {
        return Source::default();
    };
    let (git_dir, common_dir) = (Path::new(git_dir), Path::new(common_dir));
    let watch = watch(git_dir, common_dir, &ref_files(&git, &symrefs(&git)));
    let commit = git(&[
        "rev-list",
        "-1",
        "--abbrev=9",
        "--format=%h %cd",
        "--date=short",
        "HEAD",
    ])
    .and_then(|out| parse_commit(&out))
    .and_then(|(short, date)| {
        let status = git(&["status", "--porcelain", "--untracked-files=no"])?;
        Some(Commit {
            short,
            date,
            dirty: !status.trim().is_empty(),
        })
    });
    Source { commit, watch }
}

/// The abbreviated ID and date from `rev-list --format`, whose last line is `%h %cd` after a
/// `commit <id>` line.
fn parse_commit(out: &str) -> Option<(String, String)> {
    let (short, date) = out.lines().last()?.split_once(' ')?;
    let hex = short.len() >= 7 && short.bytes().all(|b| b.is_ascii_hexdigit());
    let ymd = date.len() == 10
        && date.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        });
    (hex && ymd).then(|| (short.to_owned(), date.to_owned()))
}

/// The refs HEAD names, one level at a time: `refs/heads/a`, then `refs/heads/b` where `a` is
/// itself a symbolic ref to `b`; none for a detached HEAD. A git without `--no-recurse` gets
/// the last ref of the chain alone.
fn symrefs(git: impl Fn(&[&str]) -> Option<String>) -> Vec<String> {
    let mut chain: Vec<String> = Vec::new();
    while chain.len() < SYMREF_DEPTH {
        let name = chain.last().map_or("HEAD", String::as_str);
        let Some(target) = git(&["symbolic-ref", "--no-recurse", "-q", name]) else {
            break;
        };
        chain.push(target.trim().to_owned());
    }
    if chain.is_empty() {
        chain.extend(git(&["symbolic-ref", "-q", "HEAD"]).map(|r| r.trim().to_owned()));
    }
    chain
}

/// The loose file of each ref in `refs`, absolute, where git keeps it in this worktree; none
/// when git cannot say.
fn ref_files(git: impl Fn(&[&str]) -> Option<String>, refs: &[String]) -> Vec<PathBuf> {
    let refs: Vec<&str> = refs
        .iter()
        .map(String::as_str)
        .filter(|r| r.starts_with("refs/"))
        .collect();
    if refs.is_empty() {
        return Vec::new();
    }
    let mut args = vec!["rev-parse", "--path-format=absolute"];
    for name in &refs {
        args.extend(["--git-path", name]);
    }
    git(&args).map_or_else(Vec::new, |out| out.lines().map(PathBuf::from).collect())
}

fn watch(git_dir: &Path, common_dir: &Path, ref_files: &[PathBuf]) -> Vec<PathBuf> {
    let mut watch = vec![git_dir.join("HEAD")];
    for loose in ref_files {
        if loose.is_file() {
            watch.push(loose.clone());
        } else if let Some(dir) = loose
            .ancestors()
            .skip(1)
            .take_while(|dir| *dir != common_dir && *dir != git_dir)
            .find(|dir| dir.is_dir())
        {
            watch.push(dir.to_path_buf());
        }
    }
    watch.push(common_dir.join("packed-refs"));
    watch.push(common_dir.join("reftable"));
    watch.push(git_dir.join("reftable"));
    watch.sort();
    watch.dedup();
    watch.retain(|path| path.exists());
    watch
}

/// The version `cardinal --version` prints after its name: `package`, then the commit in
/// parentheses as cargo and rustc print theirs, `-dirty` after the ID as `git describe
/// --dirty` marks a changed tree.
#[must_use]
pub fn version(package: &str, commit: Option<&Commit>) -> String {
    match commit {
        None => package.to_owned(),
        Some(c) => {
            let dirty = if c.dirty { "-dirty" } else { "" };
            format!("{package} ({}{dirty} {})", c.short, c.date)
        }
    }
}
