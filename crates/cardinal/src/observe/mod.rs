//! Observation: git and gh with constructed environments and deadlines, repository and
//! worktree facts, merge evidence, ignored data, disk usage, and outlived processes
//! (`docs/architecture.md` section 8).
//!
//! Every fact is read fresh. A git or gh call that fails to start, exits with a status that
//! carries no meaning for the fact, prints output that does not parse, or misses its
//! deadline makes the fact `unknown`, never "safe". Observation writes nothing: git runs
//! with `--no-optional-locks` and `GIT_OPTIONAL_LOCKS=0`, never `fetch`, with every
//! configured filter driver disabled, and with `GIT_NO_LAZY_FETCH=1`, so a missing object in
//! a partial clone fails the step instead of being fetched; with a git that does not honor
//! that variable in every read (before 2.45.1) nothing that reads objects runs in a promisor
//! repository (section 8.1). `inventory` never deletes reservations or signals processes.

mod data;
mod gh;
mod git;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub use crate::os::child::{ChildError, Output};

use crate::admission;
use crate::config::{Config, DataList, Selection};
use crate::contract::{
    ArbiterView, BaseRef, CheckResult, DerivedState, DirtyUnknown, DirtyUnknownReason, DiskState,
    Error, ErrorCode, Facts, GhStatus, HostInfo, IgnoredClass, IgnoredEntry, InUse, Inventory,
    InventorySchema, MergeEvidence, Os, Outlived, PrRef, ReclaimSummary, Repo, ReservationView,
    Resources, ScopedError, SessionSnapshot, Tri, Worktree,
};
use crate::os::child;
use crate::os::{Detail, ProcessTable};
use crate::state::{Probe, StateDir, find_worktree_root, now_ms, probe, worktree_key};
use crate::testpoint;

use data::DataPatterns;
use git::ListedWorktree;

/// `cardinal inventory` arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InventoryOptions {
    /// `--repo`: main worktrees to observe in addition to the configured ones.
    pub repos: Vec<PathBuf>,
    /// `--sizes`: walk worktrees for `allocated_bytes`.
    pub sizes: bool,
}

/// One worktree observed from scratch, with its repository (whose `worktrees` is empty).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeObservation {
    pub repo: Repo,
    pub worktree: Worktree,
}

/// `cardinal inventory`. Read-only: never deletes or signals anything.
pub fn inventory(
    config: &Config,
    state: &StateDir,
    options: &InventoryOptions,
) -> Result<Inventory, Error> {
    Observer::new(config).inventory(state, options)
}

/// The most bytes read from each of a git or gh child's stdout and stderr. A repository
/// whose status or file list is larger makes those facts `unknown`.
const MAX_OUTPUT: u64 = 256 * 1024 * 1024;

/// The bounds of a git or gh child: `timeout_ms` from now and [`MAX_OUTPUT`].
fn limits(timeout_ms: u64) -> child::Limits {
    child::Limits {
        deadline: Instant::now() + Duration::from_millis(timeout_ms),
        max_output: MAX_OUTPUT,
    }
}

/// Observes the worktree containing `path` from scratch; `plan` and `act` use it.
pub fn observe_worktree(
    config: &Config,
    state: &StateDir,
    path: &Path,
) -> Result<WorktreeObservation, Error> {
    Observer::new(config).observe_worktree(state, path)
}

/// `content_in_base` (`docs/architecture.md` section 8.2): whether every file the branch at
/// `head` changed since its merge base with `base` has the same content and mode in `base`,
/// with up to 20 of the paths that differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentInBase {
    pub value: Tri,
    pub diff: Vec<String>,
}

impl ContentInBase {
    /// Not read: `unknown` with no paths.
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            value: Tri::Unknown,
            diff: Vec::new(),
        }
    }
}

/// Reads `content_in_base` for the worktree at `worktree` from tree-to-tree diffs, which read
/// no index. Observation reads it only when merge evidence is `none`, which `act` repeats for
/// `--squash-merged` in its own observation. A step that fails is `unknown`.
#[must_use]
pub fn content_in_base(
    config: &Config,
    worktree: &Path,
    head: &str,
    base: &BaseRef,
) -> ContentInBase {
    Observer::new(config)
        .content_in_base(worktree, head, base)
        .unwrap_or_else(|_| ContentInBase::unknown())
}

/// How many paths `content_diff` lists.
const CONTENT_DIFF_PATHS: usize = 20;

/// The disk states of `disk/`, as the last `reconcile` pass left them (section 8.5), in file
/// name order; a file that cannot be read is reported in `errors`.
pub fn disk_states(state: &StateDir, errors: &mut Vec<ScopedError>) -> Vec<DiskState> {
    let dir = state.disk_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            errors.push(scoped("disk", format!("{}: {err}", dir.display())));
            return Vec::new();
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    let mut states = Vec::new();
    for path in paths {
        match std::fs::read(&path)
            .map_err(|err| err.to_string())
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|err| err.to_string()))
        {
            Ok(state) => states.push(state),
            Err(_) if !path.exists() => {}
            Err(message) => errors.push(scoped(&format!("disk:{}", path.display()), message)),
        }
    }
    states
}

/// Runs `git --no-optional-locks -C <dir> <args>` with the constructed environment of
/// section 8.1 and `[observe] git_timeout_ms`. This is the one way cardinal runs git.
pub fn git<S: AsRef<OsStr>>(config: &Config, dir: &Path, args: &[S]) -> Result<Output, ChildError> {
    Observer::new(config).git(dir, args)
}

/// Observation with its inputs: the configuration and the environment git and gh children
/// are built from. Reservations are read through `admission::list_reservations`.
pub struct Observer<'a> {
    config: &'a Config,
    /// The environment of git children.
    env: Vec<(OsString, OsString)>,
    /// The environment of gh children.
    gh_env: Vec<(OsString, OsString)>,
    /// The caller's `GH_HOST`: a host besides `github.com` whose remotes are GitHub.
    gh_host: Option<String>,
    /// `git --version` as `(major, minor)`, read once when first needed; `None` when it
    /// could not be read.
    version: OnceLock<Option<(u32, u32, u32)>>,
}

/// The first git that refuses a lazy fetch under `GIT_NO_LAZY_FETCH=1`.
/// 2.45.0 reads the variable, but its inexact rename detection, which `status` runs on a
/// staged rename, fetches without asking it.
const NO_LAZY_FETCH_SINCE: (u32, u32, u32) = (2, 45, 1);

/// How many paths `dirty_unknown` lists.
const DIRTY_UNKNOWN_PATHS: usize = 20;

/// The most bytes of paths, by [`arg_cost`], one `git check-attr` command line carries
/// outside Windows.
const CHECK_ATTR_ARGS: usize = 64 * 1024;

/// The characters of a Windows command line (`CreateProcessW`), its terminating NUL left
/// out.
const WINDOWS_COMMAND_LINE: usize = 32_766;

/// The characters of a command line cmd.exe runs, as it runs a batch file on Windows.
const CMD_COMMAND_LINE: usize = 8_191;

/// What the standard library puts before the quoted path of a batch file it starts on Windows,
/// whose command line it then closes with one more `"`.
const CMD_PREFIX: &str = "cmd.exe /e:ON /v:OFF /d /c \"";

/// Facts shared by every worktree of one inventory.
struct Shared<'a> {
    state: &'a StateDir,
    /// `None` when the reservations could not be read.
    reservations: Option<Vec<ReservationView>>,
    /// `None` when the process table could not be read.
    processes: Option<ProcessTable>,
    sizes: bool,
}

/// Repository-level facts every worktree of the repository uses.
struct RepoContext {
    repo: Repo,
    github: Option<gh::GithubRepo>,
    /// A repository-level step that failed (the base could not be read).
    failure: Option<String>,
    listed: Vec<ListedWorktree>,
    patterns: DataPatterns,
    /// `None` when the open pull requests are unknown.
    open_prs: Option<Vec<gh::OpenPr>>,
}

/// The history facts of one worktree.
struct History {
    merge_evidence: MergeEvidence,
    merged_pr: Option<PrRef>,
    unpushed: Tri,
    open_pr: Tri,
    gh_error: Option<String>,
}

impl History {
    /// History read without git: every fact that needs objects `unknown`, the open pull
    /// request from the repository's list.
    fn unknown(ctx: &RepoContext, branch: Option<&str>, head: Option<&str>) -> Self {
        Self {
            merge_evidence: MergeEvidence::Unknown,
            merged_pr: None,
            unpushed: Tri::Unknown,
            open_pr: match &ctx.open_prs {
                Some(prs) => tri(gh::open_pr_for(prs, branch, head).is_some()),
                None => Tri::Unknown,
            },
            gh_error: None,
        }
    }
}

/// The facts that need a worktree's index.
struct IndexFacts {
    status: Option<git::Status>,
    tracked: Option<Tracked>,
    dirty: Tri,
    dirty_unknown: Option<DirtyUnknown>,
}

impl IndexFacts {
    fn unknown() -> Self {
        Self {
            status: None,
            tracked: None,
            dirty: Tri::Unknown,
            dirty_unknown: None,
        }
    }
}

/// What one worktree observation found, besides the worktree itself.
struct WorktreeResult {
    worktree: Worktree,
    errors: Vec<ScopedError>,
    gh_error: Option<String>,
}

impl<'a> Observer<'a> {
    /// Observation with this process's environment.
    #[must_use]
    pub fn new(config: &'a Config) -> Self {
        Self {
            config,
            env: Vec::new(),
            gh_env: Vec::new(),
            gh_host: None,
            version: OnceLock::new(),
        }
        .with_env(std::env::vars_os())
    }

    /// Builds the child environments from `inherited` instead of this process's environment.
    #[must_use]
    pub fn with_env(mut self, inherited: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        let inherited: Vec<(OsString, OsString)> = inherited.into_iter().collect();
        self.gh_host = child::lookup(&inherited, "GH_HOST")
            .and_then(OsStr::to_str)
            .map(str::to_owned);
        self.env = child::git_env(inherited.iter().cloned());
        self.gh_env = child::gh_env(inherited);
        self
    }

    /// See [`git`](fn@git).
    pub fn git<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Result<Output, ChildError> {
        self.git_with(dir, args, &[])
    }

    /// [`Observer::git`] with `extra` added to the constructed environment, for the
    /// configuration that disables a worktree's filter drivers.
    fn git_with<S: AsRef<OsStr>>(
        &self,
        dir: &Path,
        args: &[S],
        extra: &[(OsString, OsString)],
    ) -> Result<Output, ChildError> {
        let observe = &self.config.host.observe;
        let mut command = git_prefix(dir);
        command.extend(args.iter().map(|a| a.as_ref().to_os_string()));
        let env;
        let env = if extra.is_empty() {
            &self.env
        } else {
            env = self.env.iter().chain(extra).cloned().collect::<Vec<_>>();
            &env
        };
        child::run(
            &observe.git,
            &command,
            env,
            dir,
            limits(observe.git_timeout_ms),
        )
    }

    /// `git --version` as `(major, minor)`, read once per observation, run in `dir` so that
    /// it does not inherit this process's working directory.
    fn version(&self, dir: &Path) -> Option<(u32, u32, u32)> {
        *self.version.get_or_init(|| {
            let out = self.git_ok(dir, &["--version"]).ok()?;
            git::parse_version(&out.stdout)
        })
    }

    /// Why no command that reads objects may run in `dir`, if that is so: a git before
    /// 2.45.1, which can fetch a missing object from a promisor remote despite
    /// `GIT_NO_LAZY_FETCH`, where the configuration git reads in `dir` (a linked worktree's
    /// `config.worktree` included) names a promisor: `extensions.partialClone`, a
    /// `remote.<name>.partialCloneFilter`, or a `remote.<name>.promisor` that is not false. A
    /// configuration or version that cannot be read fails closed.
    fn objects_unreadable(&self, dir: &Path) -> Option<String> {
        let out = match self.git(
            dir,
            &[
                "config",
                "-z",
                "--get-regexp",
                r"^(extensions\.partialclone|remote\..*\.(promisor|partialclonefilter))$",
            ],
        ) {
            // Exit 1: no such key.
            Ok(out) if out.code == Some(1) => return None,
            Ok(out) => out.success("git config --get-regexp"),
            Err(err) => Err(err.to_string()),
        };
        let promisor = match out {
            Ok(out) => git::parse_promisor(&out.stdout),
            Err(message) => {
                return Some(format!(
                    "cannot tell whether the repository is a partial clone: {message}"
                ));
            }
        };
        if !promisor {
            return None;
        }
        promisor_unreadable(self.version(dir))
    }

    /// git that must exit 0; anything else is an error message.
    fn git_ok<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Result<Output, String> {
        self.git_ok_with(dir, args, &[])
    }

    /// [`Observer::git_ok`] with `extra` added to the environment (see [`Observer::git_with`]).
    fn git_ok_with<S: AsRef<OsStr>>(
        &self,
        dir: &Path,
        args: &[S],
        extra: &[(OsString, OsString)],
    ) -> Result<Output, String> {
        let what = args.first().map_or("git".to_owned(), |a| {
            format!("git {}", a.as_ref().to_string_lossy())
        });
        self.git_with(dir, args, extra)
            .map_err(|err| err.to_string())?
            .success(&what)
    }

    /// gh with `args`, run in `dir` (the observed repository or worktree) so that it never
    /// inherits this process's working directory.
    fn gh(&self, dir: &Path, args: &[OsString]) -> Result<Output, String> {
        let observe = &self.config.host.observe;
        let out = child::run(
            &observe.gh,
            args,
            &self.gh_env,
            dir,
            limits(observe.gh_timeout_ms),
        )
        .map_err(|err| err.to_string())?;
        out.success("gh")
    }

    /// `cardinal inventory`.
    pub fn inventory(
        &self,
        state: &StateDir,
        options: &InventoryOptions,
    ) -> Result<Inventory, Error> {
        let mut errors = Vec::new();
        let shared = self.shared(state, options.sizes, &mut errors);
        let host = Self::host_info(state, &mut errors);

        let mut repos: Vec<Repo> = Vec::new();
        for path in self.repo_paths(options) {
            match self.repo_context(&path) {
                Ok(ctx) => {
                    if repos.iter().any(|r| r.common_dir == ctx.repo.common_dir) {
                        continue;
                    }
                    let (repo, repo_errors) = self.observe_repo(ctx, &shared);
                    errors.extend(repo_errors);
                    repos.push(repo);
                }
                Err(message) => errors.push(scoped(&format!("repo:{}", path.display()), message)),
            }
        }

        let (budget, reserved, reservations) = budget_view(self.config, shared.reservations);
        let outlived = outlived(state, shared.processes.as_ref(), &mut errors);
        let disk = disk_states(state, &mut errors);
        Ok(Inventory {
            schema: InventorySchema,
            observed_ms: now_ms(),
            host,
            budget,
            reserved,
            reservations,
            repos,
            outlived,
            disk,
            arbiter: ArbiterView::default(),
            errors,
        })
    }

    /// Observes the worktree containing `path` from scratch.
    pub fn observe_worktree(
        &self,
        state: &StateDir,
        path: &Path,
    ) -> Result<WorktreeObservation, Error> {
        let root = find_worktree_root(path).ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                format!("{} is not inside a git worktree", path.display()),
            )
        })?;
        let ctx = self
            .repo_context(&root)
            .map_err(|message| Error::new(ErrorCode::Internal, message))?;
        let index = ctx
            .listed
            .iter()
            .position(|w| !w.bare && canonical(&w.path) == root)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::NotFound,
                    format!("git does not list {} as a worktree", root.display()),
                )
            })?;
        let shared = self.shared(state, false, &mut Vec::new());
        let result = self.observe_one(&ctx, index, &shared);
        let mut repo = ctx.repo;
        if let Some(message) = result.gh_error {
            repo.gh = GhStatus::Unknown;
            repo.gh_error.get_or_insert(message);
        }
        Ok(WorktreeObservation {
            repo,
            worktree: result.worktree,
        })
    }

    fn shared<'s>(
        &self,
        state: &'s StateDir,
        sizes: bool,
        errors: &mut Vec<ScopedError>,
    ) -> Shared<'s> {
        // The process table is read before any git or gh child starts, so none of them can
        // be mistaken for a process working inside a worktree.
        let processes = ProcessTable::snapshot(Detail::all())
            .map_err(|err| errors.push(scoped("processes", err.to_string())))
            .ok();
        // Read under the admission lock; a lock that is not free in time makes the in-use
        // facts unknown, never "no reservations".
        let reservations = admission::list_reservations(state, &self.config.host)
            .map_err(|err| errors.push(scoped("reservations", err.message)))
            .ok();
        Shared {
            state,
            reservations,
            processes,
            sizes,
        }
    }

    fn host_info(state: &StateDir, errors: &mut Vec<ScopedError>) -> HostInfo {
        let boot_s = crate::os::boot_time_s()
            .map_err(|err| errors.push(scoped("host", err.to_string())))
            .unwrap_or(0);
        let os = if cfg!(target_os = "macos") {
            Os::Macos
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        };
        HostInfo {
            id: state.host_id().to_owned(),
            hostname: crate::os::hostname().unwrap_or_default(),
            os,
            arch: std::env::consts::ARCH.to_owned(),
            boot_s,
        }
    }

    /// The main worktrees of profiles with `match.paths`, `[observe] repos`, and `--repo`, in
    /// that order.
    fn repo_paths(&self, options: &InventoryOptions) -> Vec<PathBuf> {
        let profiles = self
            .config
            .profiles
            .iter()
            .filter_map(|p| p.matcher.as_ref())
            .flat_map(|m| m.paths.iter().cloned());
        let mut paths: Vec<PathBuf> = Vec::new();
        for path in profiles
            .chain(self.config.host.observe.repos.iter().cloned())
            .chain(options.repos.iter().cloned())
        {
            let path = std::path::absolute(&path).unwrap_or(path);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        paths
    }

    /// Repository facts, read through the worktree at or containing `path`.
    fn repo_context(&self, path: &Path) -> Result<RepoContext, String> {
        let dir = dunce::canonicalize(path).map_err(|err| format!("{}: {err}", path.display()))?;
        let out = self.git_ok(
            &dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let common = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let common_dir = canonical(Path::new(&common));
        let out = self.git_ok(&dir, &["worktree", "list", "--porcelain", "-z"])?;
        let listed = git::parse_worktree_list(&out.stdout)?;
        let main_worktree = canonical(&listed[0].path);

        // Exit 2 is "no such remote"; any other failure leaves the remote unknown.
        let remote = match self.git(&dir, &["remote", "get-url", "origin"]) {
            Ok(out) if out.code == Some(0) => {
                Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()))
            }
            Ok(out) if out.code == Some(2) => Ok(None),
            Ok(out) => Err(format!(
                "git remote get-url origin failed: {}",
                out.stderr_line()
            )),
            Err(err) => Err(err.to_string()),
        };
        let remote_url = remote.as_ref().ok().and_then(Option::as_deref);
        let selection: Selection<'_> = self.config.select(|profile| {
            profile.matches_common_dir(&common_dir)
                || remote_url.is_some_and(|url| profile.matches_remote(url))
        });
        let patterns = DataPatterns {
            regenerable: selection
                .regenerable()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            protected: selection
                .protected()
                .into_iter()
                .map(str::to_owned)
                .collect(),
        };
        // Peeling the base to its commit reads an object, here in `dir`.
        let (base, failure) = match self.objects_unreadable(&dir) {
            Some(reason) => (None, Some(reason)),
            None => match self.base(&dir, selection.base()) {
                Ok(base) => (base, None),
                Err(message) => (None, Some(message)),
            },
        };

        let github = remote_url.and_then(|url| gh::github_repo(url, self.gh_host.as_deref()));
        let (gh_status, gh_error, open_prs) = match (&remote, &github) {
            (_, Some(repo)) => match self
                .gh(&dir, &gh::open_args(&repo.spec()))
                .and_then(|out| gh::parse_open(&out.stdout))
            {
                Ok(prs) => (GhStatus::Ok, None, Some(prs)),
                Err(message) => (GhStatus::Unknown, Some(message), None),
            },
            (Err(message), None) => (GhStatus::Unknown, Some(message.clone()), None),
            (Ok(_), None) => (GhStatus::NotApplicable, None, None),
        };

        Ok(RepoContext {
            repo: Repo {
                common_dir: display(&common_dir),
                main_worktree: display(&main_worktree),
                profiles: selection.names().into_iter().map(str::to_owned).collect(),
                github: github.as_ref().map(|r| r.name.clone()),
                base,
                gh: gh_status,
                gh_error,
                worktrees: Vec::new(),
            },
            github,
            failure,
            listed,
            patterns,
            open_prs,
        })
    }

    /// `[git] base`, else the target of `refs/remotes/origin/HEAD`, with its commit.
    ///
    /// The base is always resolved as a full ref name, never through revision syntax, which
    /// could read the index (`:path`) or reflogs (`@{…}`): a configured base must be a valid
    /// ref name and is looked up as `refs/remotes/<base>`, then `refs/heads/<base>` (or as
    /// itself when it starts with `refs/`); `origin/HEAD` must point under
    /// `refs/remotes/origin/`. `None` only when git answers with the query's documented
    /// "absent" status (exit 1 with `--quiet`); an invalid name, any other status, a signal,
    /// or output that is not a ref name or an object ID is an error, a failed step.
    fn base(&self, dir: &Path, configured: Option<&str>) -> Result<Option<BaseRef>, String> {
        let answered = |args: &[&str]| -> Result<Option<String>, String> {
            let what = format!("git {}", args.join(" "));
            let out = self.git(dir, args).map_err(|err| err.to_string())?;
            if out.code == Some(1) {
                return Ok(None);
            }
            let out = out.success(&what)?;
            let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if text.is_empty() || text.contains('\n') {
                return Err(format!("{what} printed {text:?}"));
            }
            Ok(Some(text))
        };
        let (name, candidates) = if let Some(name) = configured {
            if !git::is_ref_name(name) {
                return Err(format!("[git] base {name:?} is not a ref name"));
            }
            let candidates = if name.starts_with("refs/") {
                vec![name.to_owned()]
            } else {
                vec![format!("refs/remotes/{name}"), format!("refs/heads/{name}")]
            };
            (name.to_owned(), candidates)
        } else {
            let Some(target) = answered(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])?
            else {
                return Ok(None);
            };
            let short = target
                .strip_prefix("refs/remotes/")
                .filter(|short| short.starts_with("origin/") && git::is_ref_name(&target));
            let Some(short) = short else {
                return Err(format!(
                    "origin/HEAD points to {target:?}, not a ref under refs/remotes/origin/"
                ));
            };
            (short.to_owned(), vec![target.clone()])
        };
        for full in candidates {
            let spec = format!("{full}^{{commit}}");
            let Some(oid) = answered(&["rev-parse", "--verify", "--quiet", &spec])? else {
                continue;
            };
            if !git::is_oid(&oid) {
                return Err(format!("git rev-parse printed {oid:?} for {spec}"));
            }
            return Ok(Some(BaseRef {
                ref_name: name,
                oid,
            }));
        }
        Ok(None)
    }

    /// Observes every worktree of a repository, several at a time.
    fn observe_repo(&self, ctx: RepoContext, shared: &Shared<'_>) -> (Repo, Vec<ScopedError>) {
        let indexes: Vec<usize> = (0..ctx.listed.len())
            .filter(|&i| !ctx.listed[i].bare)
            .collect();
        let results: Mutex<Vec<(usize, WorktreeResult)>> = Mutex::new(Vec::new());
        let next = AtomicUsize::new(0);
        let workers = std::thread::available_parallelism()
            .map_or(1, std::num::NonZero::get)
            .clamp(1, 8)
            .min(indexes.len().max(1));
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let n = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&index) = indexes.get(n) else { break };
                        let result = self.observe_one(&ctx, index, shared);
                        results
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push((index, result));
                    }
                });
            }
        });
        let mut results = results
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        results.sort_by_key(|(index, _)| *index);

        let mut errors = Vec::new();
        if let Some(message) = &ctx.failure {
            errors.push(scoped(
                &format!("repo:{}", ctx.repo.main_worktree),
                message.clone(),
            ));
        }
        let mut repo = ctx.repo;
        for (_, result) in results {
            if let Some(message) = result.gh_error {
                repo.gh = GhStatus::Unknown;
                repo.gh_error.get_or_insert(message);
            }
            errors.extend(result.errors);
            repo.worktrees.push(result.worktree);
        }
        (repo, errors)
    }

    /// Observes the worktree `ctx.listed[index]`.
    fn observe_one(&self, ctx: &RepoContext, index: usize, shared: &Shared<'_>) -> WorktreeResult {
        let listed = &ctx.listed[index];
        let path = canonical(&listed.path);
        let scope = format!("worktree:{}", path.display());
        let mut errors = Vec::new();
        let mut note = |message: String| errors.push(scoped(&scope, message));
        let main = index == 0;

        let managed_by = self
            .config
            .host
            .observe
            .managed
            .iter()
            .find(|m| path.starts_with(canonical(&m.root)))
            .map(|m| m.app.clone())
            .or_else(|| listed.locked.then(|| "git-lock".to_owned()));

        // The worktree's own configuration decides, which can differ from the entry
        // directory's (`config.worktree`).
        let no_objects = self.objects_unreadable(&path);
        let IndexFacts {
            status,
            tracked,
            dirty,
            dirty_unknown,
        } = match &no_objects {
            // `status` compares the index with HEAD's tree, which reads objects.
            Some(reason) => {
                note(reason.clone());
                IndexFacts::unknown()
            }
            None => self.index_facts(&path, &ctx.patterns, &mut note),
        };
        let (head, branch, upstream) = match &status {
            Some(s) => (s.head.clone(), s.branch.clone(), s.upstream.clone()),
            None => (listed.head.clone(), listed.branch.clone(), None),
        };
        let head = head.filter(|h| git::is_oid(h));
        let has_upstream = status
            .as_ref()
            .map_or(Tri::Unknown, |s| tri(s.upstream.is_some()));

        let history = if no_objects.is_some() {
            History::unknown(ctx, branch.as_deref(), head.as_deref())
        } else {
            self.history(ctx, &path, head.as_deref(), branch.as_deref(), &mut note)
        };
        let (git_dir, last_activity_ms) = self.admin(&path, &mut note);
        let (closed_pr, content, gh_error) = self.unmerged_facts(
            ctx,
            &path,
            (branch.as_deref(), head.as_deref()),
            dirty,
            &history,
            &mut note,
        );

        let (in_use, in_use_known) = in_use(&path, shared);

        let ignored = status.as_ref().map_or_else(Vec::new, |s| {
            ignored_entries(&path, &s.ignored, &ctx.patterns, shared.sizes)
        });
        let step_failed = !errors.is_empty() || gh_error.is_some() || ctx.failure.is_some();
        let facts = Facts {
            dirty,
            dirty_unknown,
            has_upstream,
            unpushed: history.unpushed,
            open_pr: history.open_pr,
            merge_evidence: history.merge_evidence,
            merged_pr: history.merged_pr,
            content_in_base: content.value,
            content_diff: content.diff,
            closed_pr,
            in_use,
        };
        let worktree = Worktree {
            path: display(&path),
            main,
            head,
            branch,
            upstream,
            git_dir,
            last_activity_ms,
            managed_by,
            reclaim: ReclaimSummary::default(),
            allocated_bytes: shared.sizes.then(|| data::allocated_bytes(&path)).flatten(),
            facts,
            ignored,
        };
        let checks = ReclaimInputs {
            status_known: status.is_some(),
            step_failed,
            in_use_known,
            tracked,
            plain_target: plain_target(&path),
        };
        let reclaim = reclaim_summary(&worktree, &checks);
        WorktreeResult {
            worktree: Worktree {
                reclaim,
                ..worktree
            },
            errors,
            gh_error,
        }
    }

    /// `content_in_base` (section 8.2): the paths the branch changed since its merge base with
    /// `base` (`git diff` of the merge base and HEAD) against the paths where HEAD differs from
    /// `base`, renames off, tree to tree. `yes` when no path is in both.
    fn content_in_base(
        &self,
        path: &Path,
        head: &str,
        base: &BaseRef,
    ) -> Result<ContentInBase, String> {
        let out = self.git_ok(path, &["merge-base", head, &base.oid])?;
        let merge_base = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !git::is_oid(&merge_base) {
            return Err(format!("git merge-base printed {merge_base:?}"));
        }
        let names = |from: &str| -> Result<BTreeSet<Vec<u8>>, String> {
            let out = self.git_ok(
                path,
                &[
                    "diff",
                    "--name-only",
                    "-z",
                    "--no-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    // A gitlink the branch changed counts as any path does, whatever
                    // `diff.ignoreSubmodules` says.
                    "--ignore-submodules=none",
                    from,
                    head,
                    "--",
                ],
            )?;
            Ok(out
                .stdout
                .split(|&b| b == 0)
                .filter(|p| !p.is_empty())
                .map(<[u8]>::to_vec)
                .collect())
        };
        let changed = names(&merge_base)?;
        let differ = names(&base.oid)?;
        let both: Vec<String> = changed
            .intersection(&differ)
            .take(CONTENT_DIFF_PATHS)
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        Ok(ContentInBase {
            value: if both.is_empty() { Tri::Yes } else { Tri::No },
            diff: both,
        })
    }

    /// The facts of a worktree that may not have been merged: `closed_pr` for one that is
    /// dirty or has no merge evidence, and `content_in_base` for one without merge evidence
    /// (section 8.2), with the gh error of the worktree's history or of the closed pull
    /// request query. Neither is read where history could not read objects: its merge
    /// evidence and `dirty` are then `unknown`.
    fn unmerged_facts(
        &self,
        ctx: &RepoContext,
        path: &Path,
        (branch, head): (Option<&str>, Option<&str>),
        dirty: Tri,
        history: &History,
        note: &mut dyn FnMut(String),
    ) -> (Option<PrRef>, ContentInBase, Option<String>) {
        let mut gh_error = history.gh_error.clone();
        let unmerged = history.merge_evidence == MergeEvidence::None;
        let closed_pr = match (branch, head) {
            (Some(branch), Some(head)) if dirty == Tri::Yes || unmerged => self
                .closed_pr(ctx, path, branch, head, note)
                .unwrap_or_else(|message| {
                    gh_error.get_or_insert(message);
                    None
                }),
            _ => None,
        };
        let content = match (&ctx.repo.base, head) {
            (Some(base), Some(head)) if unmerged => self
                .content_in_base(path, head, base)
                .unwrap_or_else(|message| {
                    note(message);
                    ContentInBase::unknown()
                }),
            _ => ContentInBase::unknown(),
        };
        (closed_pr, content, gh_error)
    }

    /// The worktree's git dir (`git rev-parse --absolute-git-dir`) and the modification time
    /// of its index (`--git-path index`, read with `lstat`), which read no object and no
    /// index. A missing index has no time.
    fn admin(&self, path: &Path, note: &mut dyn FnMut(String)) -> (Option<String>, Option<u64>) {
        let out = match self.git_ok(
            path,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--absolute-git-dir",
                "--git-path",
                "index",
            ],
        ) {
            Ok(out) => out,
            Err(message) => {
                note(message);
                return (None, None);
            }
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        let (Some(git_dir), Some(index)) = (lines.next(), lines.next()) else {
            note(format!("git rev-parse printed {text:?}"));
            return (None, None);
        };
        let modified = std::fs::symlink_metadata(index)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        (Some(display(&canonical(Path::new(git_dir)))), modified)
    }

    /// The newest pull request of `branch` closed without merging whose head is HEAD or an
    /// ancestor of it (section 8.2); a head missing locally does not count. `Err` when gh
    /// fails or its answer is incomplete; a git step that fails is noted and leaves it null.
    fn closed_pr(
        &self,
        ctx: &RepoContext,
        path: &Path,
        branch: &str,
        head: &str,
        note: &mut dyn FnMut(String),
    ) -> Result<Option<PrRef>, String> {
        let Some(repo) = &ctx.github else {
            return Ok(None);
        };
        let prs = self
            .gh(path, &gh::closed_args(&repo.spec(), branch))
            .and_then(|out| gh::parse_closed(&out.stdout))?;
        for pr in prs {
            if !git::is_oid(&pr.head_ref_oid) {
                return Err(format!("gh printed a head {:?}", pr.head_ref_oid));
            }
            let spec = format!("{}^{{commit}}", pr.head_ref_oid);
            let answer = |args: &[&str]| match self.git(path, args) {
                Ok(out) => match out.code {
                    Some(0) => Ok(true),
                    Some(1) => Ok(false),
                    _ => Err(format!("git {} failed: {}", args[0], out.stderr_line())),
                },
                Err(err) => Err(err.to_string()),
            };
            let found = answer(&["rev-parse", "--verify", "--quiet", &spec]).and_then(|present| {
                Ok(present && answer(&["merge-base", "--is-ancestor", &pr.head_ref_oid, head])?)
            });
            match found {
                Ok(true) => {
                    return Ok(Some(PrRef {
                        number: pr.number,
                        url: pr.url,
                    }));
                }
                Ok(false) => {}
                Err(message) => {
                    note(message);
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }

    /// Merge evidence, unpushed commits, and the open pull request of a worktree at `head`.
    fn history(
        &self,
        ctx: &RepoContext,
        path: &Path,
        head: Option<&str>,
        branch: Option<&str>,
        note: &mut dyn FnMut(String),
    ) -> History {
        let base_ancestor = match (head, &ctx.repo.base) {
            (Some(head), Some(base)) => {
                match self.git(path, &["merge-base", "--is-ancestor", head, &base.oid]) {
                    Ok(out) if out.code == Some(0) => Tri::Yes,
                    Ok(out) if out.code == Some(1) => Tri::No,
                    Ok(out) => {
                        note(format!("git merge-base failed: {}", out.stderr_line()));
                        Tri::Unknown
                    }
                    Err(err) => {
                        note(err.to_string());
                        Tri::Unknown
                    }
                }
            }
            _ => Tri::Unknown,
        };

        let mut gh_error = None;
        // Looked up only without base ancestry; `None` when the answer is unknown.
        let merged: Option<Option<PrRef>> = match (&ctx.github, head) {
            _ if base_ancestor == Tri::Yes => None,
            (Some(repo), Some(head)) => match self
                .gh(path, &gh::merged_args(&repo.spec(), head))
                .and_then(|out| gh::parse_merged(&out.stdout))
                .and_then(|prs| {
                    Ok(gh::merged_pr_for(&prs, head)?.map(|pr| PrRef {
                        number: pr.number,
                        url: pr.url.clone(),
                    }))
                }) {
                Ok(found) => Some(found),
                Err(message) => {
                    gh_error = Some(message);
                    None
                }
            },
            _ => None,
        };
        let (merge_evidence, merged_pr) = match (base_ancestor, merged) {
            (Tri::Yes, _) => (MergeEvidence::BaseAncestor, None),
            (_, Some(Some(pr))) => (MergeEvidence::MergedPr, Some(pr)),
            (Tri::No, Some(None)) => (MergeEvidence::None, None),
            _ => (MergeEvidence::Unknown, None),
        };

        let unpushed = match head {
            Some(head) => match self
                .git_ok(path, &["rev-list", "--count", head, "--not", "--remotes"])
                .and_then(|out| {
                    String::from_utf8_lossy(&out.stdout)
                        .trim()
                        .parse::<u64>()
                        .map_err(|err| format!("unexpected git rev-list output: {err}"))
                }) {
                Ok(0) => Tri::No,
                // The commits of a merged pull request's head exist on GitHub.
                Ok(_) if merge_evidence == MergeEvidence::MergedPr => Tri::No,
                Ok(_) => Tri::Yes,
                Err(message) => {
                    note(message);
                    Tri::Unknown
                }
            },
            None => Tri::Unknown,
        };

        let open_pr = match &ctx.open_prs {
            Some(prs) => tri(gh::open_pr_for(prs, branch, head).is_some()),
            None => Tri::Unknown,
        };
        History {
            merge_evidence,
            merged_pr,
            unpushed,
            open_pr,
            gh_error,
        }
    }

    /// The facts that need the worktree's index: its status and its tracked files. Neither
    /// is read when the worktree uses a split index: reading one freshens its shared index
    /// file inside `.git`, even without optional locks.
    ///
    /// No filter driver runs (section 8.1): the drivers the worktree's configuration defines
    /// are listed first and each one's `clean`, `smudge`, and `process` passed as empty and
    /// `required` as false. Without its filter a filtered file is compared raw with its
    /// cleaned blob, so a worktree-side modification of a path whose `filter` attribute names
    /// a configured driver cannot be told from an unchanged file: when only such entries are
    /// present, `dirty` is `unknown` with those paths. The drivers are listed again after
    /// `status`, and a changed list fails the worktree's index facts (`filters_changed`).
    fn index_facts(
        &self,
        path: &Path,
        patterns: &DataPatterns,
        note: &mut dyn FnMut(String),
    ) -> IndexFacts {
        match self.split_index(path) {
            Ok(false) => {}
            Ok(true) => {
                note("the worktree uses a split index, which reading would modify; its status and tracked files are not read".to_owned());
                return IndexFacts::unknown();
            }
            Err(message) => {
                note(message);
                return IndexFacts::unknown();
            }
        }
        let drivers = match self.filter_drivers(path) {
            Ok(drivers) => drivers,
            Err(message) => {
                note(message);
                return IndexFacts::unknown();
            }
        };
        if let Err(err) = testpoint::hit(testpoint::OBSERVE_FILTERS_LISTED) {
            note(err.to_string());
            return IndexFacts::unknown();
        }
        let disabled = disabled_filters(&drivers);
        let status = self
            .git_ok_with(
                path,
                &[
                    "status",
                    "--porcelain=v2",
                    "-z",
                    "--branch",
                    // Never enter a submodule: its own index could be split, and a
                    // gitlink already fails `no_nested_repository`.
                    "--ignore-submodules=all",
                    "--untracked-files=all",
                    "--ignored=matching",
                ],
                &disabled,
            )
            .and_then(|out| complete_status(&out))
            .map_err(&mut *note)
            .ok();
        let (dirty, dirty_unknown) = match &status {
            None => (Tri::Unknown, None),
            Some(s) if s.other_changes => (Tri::Yes, None),
            Some(s) if s.modified.is_empty() => (Tri::No, None),
            // Without a driver no path is filtered, and no attribute is looked up.
            Some(_) if drivers.is_empty() => (Tri::Yes, None),
            Some(s) => match self.filtered(path, &s.modified, &drivers, &disabled) {
                Ok(filtered) if filtered.len() == s.modified.len() => (
                    Tri::Unknown,
                    Some(DirtyUnknown {
                        reason: DirtyUnknownReason::FilteredPaths,
                        paths: s
                            .modified
                            .iter()
                            .take(DIRTY_UNKNOWN_PATHS)
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect(),
                    }),
                ),
                Ok(_) => (Tri::Yes, None),
                Err(message) => {
                    note(message);
                    (Tri::Unknown, None)
                }
            },
        };
        let tracked = self
            .tracked(path, patterns, &disabled)
            .map_err(&mut *note)
            .ok();
        // Status ignores submodules, a staged gitlink change included, and the removal of the
        // last gitlink from the index too: whatever the index holds that HEAD does not is an
        // uncommitted change of this worktree.
        let (dirty, dirty_unknown) = match &status {
            Some(s) if dirty != Tri::Yes && s.head.is_some() => {
                match self.staged_gitlinks(path, &disabled) {
                    Ok(true) => (Tri::Yes, None),
                    Ok(false) => (dirty, dirty_unknown),
                    Err(message) => {
                        note(message);
                        (Tri::Unknown, None)
                    }
                }
            }
            _ => (dirty, dirty_unknown),
        };
        match self.filter_drivers(path) {
            Ok(after) if after == drivers => IndexFacts {
                status,
                tracked,
                dirty,
                dirty_unknown,
            },
            Ok(_) => {
                note("filters_changed: the worktree's filter drivers changed while its status was read".to_owned());
                IndexFacts::unknown()
            }
            Err(message) => {
                note(message);
                IndexFacts::unknown()
            }
        }
    }

    /// Whether the index of the worktree at `path` differs from HEAD's tree in a path status
    /// does not report: `git diff-index --cached` compares index entries with the tree, a
    /// gitlink by its recorded commit, and enters no submodule. Called only when status
    /// found the worktree otherwise clean (or could not tell) and HEAD exists, so a path it
    /// lists is a staged gitlink change, such as a gitlink added, moved, or removed.
    fn staged_gitlinks(
        &self,
        path: &Path,
        disabled: &[(OsString, OsString)],
    ) -> Result<bool, String> {
        let out = self.git_ok_with(
            path,
            &[
                "diff-index",
                "--cached",
                "--name-only",
                "-z",
                "--no-renames",
                "--ignore-submodules=none",
                "HEAD",
                "--",
            ],
            disabled,
        )?;
        Ok(out.stdout.iter().any(|&b| b != 0))
    }

    /// The filter drivers the configuration of the worktree at `path` defines. Reads
    /// configuration only.
    fn filter_drivers(&self, path: &Path) -> Result<BTreeSet<String>, String> {
        let out = self
            .git(
                path,
                &["config", "-z", "--name-only", "--get-regexp", r"^filter\."],
            )
            .map_err(|err| err.to_string())?;
        // Exit 1: no such key.
        if out.code == Some(1) {
            return Ok(BTreeSet::new());
        }
        let out = out.success("git config --get-regexp")?;
        git::parse_filter_drivers(&out.stdout)
    }

    /// Which of `paths` (worktree-relative) have a `filter` attribute naming one of
    /// `drivers`, read with `git check-attr`. `os::child` closes a child's stdin, so the paths
    /// go as arguments, in commands that fit the command line ([`check_attr_commands`]).
    fn filtered(
        &self,
        path: &Path,
        paths: &[PathBuf],
        drivers: &BTreeSet<String>,
        disabled: &[(OsString, OsString)],
    ) -> Result<BTreeSet<PathBuf>, String> {
        let program = OsStr::new(&self.config.host.observe.git);
        let mut filtered = BTreeSet::new();
        for args in check_attr_commands(program, path, paths, cfg!(windows)) {
            let out = self.git_ok_with(path, &args, disabled)?;
            filtered.extend(git::parse_filtered(&out.stdout, drivers)?);
        }
        Ok(filtered)
    }

    /// Whether the worktree at `path` uses a split index: a `sharedindex.*` file in its git
    /// dir or the common dir, or `core.splitIndex` set. Reads only files and configuration,
    /// never the index.
    fn split_index(&self, path: &Path) -> Result<bool, String> {
        let git_dir = crate::config::git_dir(path)
            .ok_or_else(|| format!("cannot read the .git entry of {}", path.display()))?;
        let mut dirs = vec![git_dir];
        dirs.extend(crate::config::git_common_dir(path));
        for dir in &dirs {
            let entries =
                std::fs::read_dir(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
            for entry in entries {
                let entry = entry.map_err(|err| format!("{}: {err}", dir.display()))?;
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("sharedindex.")
                {
                    return Ok(true);
                }
            }
        }
        let out = self
            .git(path, &["config", "--bool", "core.splitIndex"])
            .map_err(|err| err.to_string())?;
        // Exit 1: not set.
        if out.code == Some(1) {
            return Ok(false);
        }
        let out = out.success("git config core.splitIndex")?;
        Ok(String::from_utf8_lossy(&out.stdout).trim() == "true")
    }

    /// What the index of the worktree at `path` holds that `act` would refuse: a tracked path
    /// covered by a protected pattern, and a gitlink (mode 160000) whose directory has its
    /// own `.git` entry.
    fn tracked(
        &self,
        path: &Path,
        patterns: &DataPatterns,
        disabled: &[(OsString, OsString)],
    ) -> Result<Tracked, String> {
        let out = self.git_ok_with(path, &["ls-files", "-z", "--stage"], disabled)?;
        let mut tracked = Tracked::default();
        for entry in out.stdout.split(|&b| b == 0).filter(|e| !e.is_empty()) {
            let (meta, file) = git::parse_stage_entry(entry)?;
            if !tracked.protected && patterns.covers(DataList::Protected, file) {
                tracked.protected = true;
            }
            if meta.starts_with("160000 ")
                && std::fs::symlink_metadata(path.join(file).join(".git")).is_ok()
            {
                tracked.nested = true;
            }
        }
        Ok(tracked)
    }
}

/// The arguments before a git command's own: no optional locks, no filesystem monitor (a
/// configured one would start a daemon that writes inside `.git`), and `-C dir`.
fn git_prefix(dir: &Path) -> Vec<OsString> {
    vec![
        "--no-optional-locks".into(),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-C".into(),
        dir.into(),
    ]
}

/// The arguments of `git check-attr` before its paths.
const CHECK_ATTR: [&str; 4] = ["check-attr", "-z", "filter", "--"];

/// Whether the standard library starts `program` through cmd.exe on Windows: a batch file,
/// whose name ends in `.bat` or `.cmd`, in any case.
fn is_batch_file(program: &OsStr) -> bool {
    Path::new(program)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("bat") || ext.eq_ignore_ascii_case("cmd"))
}

/// The most one argument adds to a command line: a separating space, two quotes, and its
/// bytes, which are at least its UTF-16 units on Windows, with a `"` or `\` counted twice for
/// its escape. To a batch file's (`batch_file`), a `%` adds 8, as the standard library writes
/// `%%cd:~,%` for it so that cmd.exe expands no variable.
fn arg_cost(arg: &OsStr, batch_file: bool) -> usize {
    3 + arg
        .as_encoded_bytes()
        .iter()
        .map(|b| match b {
            b'"' | b'\\' => 2,
            b'%' if batch_file => 8,
            _ => 1,
        })
        .sum::<usize>()
}

/// The `git check-attr` commands `program` runs in `dir` for `paths`, on Windows if
/// `windows`: each the arguments after [`git_prefix`], [`CHECK_ATTR`] and the next of `paths`
/// that fit. On Windows the whole command line fits, within the 8,191 characters of cmd.exe
/// for a batch file ([`is_batch_file`]), else within the 32,767 of `CreateProcessW`;
/// elsewhere the paths cost at most [`CHECK_ATTR_ARGS`]. A path that alone costs more goes in
/// a command of its own, which then fails.
fn check_attr_commands(
    program: &OsStr,
    dir: &Path,
    paths: &[PathBuf],
    windows: bool,
) -> Vec<Vec<OsString>> {
    let batch_file = windows && is_batch_file(program);
    let fixed = arg_cost(program, batch_file)
        + git_prefix(dir)
            .iter()
            .chain(&CHECK_ATTR.map(OsString::from))
            .map(|a| arg_cost(a, batch_file))
            .sum::<usize>();
    let budget = match (windows, batch_file) {
        (false, _) => CHECK_ATTR_ARGS,
        (true, false) => WINDOWS_COMMAND_LINE.saturating_sub(fixed),
        (true, true) => CMD_COMMAND_LINE.saturating_sub(fixed + CMD_PREFIX.len() + 1),
    };
    let mut commands = Vec::new();
    let mut rest = paths;
    while !rest.is_empty() {
        let mut cost = 0;
        let taken = rest
            .iter()
            .take_while(|p| {
                cost += arg_cost(p.as_os_str(), batch_file);
                cost <= budget
            })
            .count()
            .max(1);
        let args = CHECK_ATTR.iter().map(OsString::from);
        commands.push(
            args.chain(rest[..taken].iter().map(OsString::from))
                .collect(),
        );
        rest = &rest[taken..];
    }
    commands
}

/// The configuration that disables every one of `drivers` for one git command:
/// `filter.<driver>.clean`, `.smudge`, and `.process` empty and `.required` false, as
/// `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>`, and `GIT_CONFIG_VALUE_<n>`, which carry any
/// driver name and take precedence over every configuration file.
fn disabled_filters(drivers: &BTreeSet<String>) -> Vec<(OsString, OsString)> {
    let entries: Vec<(String, &str)> = drivers
        .iter()
        .flat_map(|driver| {
            [
                (format!("filter.{driver}.clean"), ""),
                (format!("filter.{driver}.smudge"), ""),
                (format!("filter.{driver}.process"), ""),
                (format!("filter.{driver}.required"), "false"),
            ]
        })
        .collect();
    if entries.is_empty() {
        return Vec::new();
    }
    let mut env: Vec<(OsString, OsString)> =
        vec![("GIT_CONFIG_COUNT".into(), entries.len().to_string().into())];
    for (n, (key, value)) in entries.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{n}").into(), key.into()));
        env.push((format!("GIT_CONFIG_VALUE_{n}").into(), value.into()));
    }
    env
}

/// Tracked content `act` would refuse.
#[derive(Clone, Copy, Debug, Default)]
struct Tracked {
    protected: bool,
    nested: bool,
}

/// The ignored entries of the worktree at `root`, classified, with the sizes of those matching
/// a regenerable pattern when `sizes` is set.
fn ignored_entries(
    root: &Path,
    ignored: &[PathBuf],
    patterns: &DataPatterns,
    sizes: bool,
) -> Vec<IgnoredEntry> {
    ignored
        .iter()
        .map(|rel| {
            let class = data::classify(root, rel, patterns);
            let entry = root.join(rel);
            let regenerable_match = patterns.covers(DataList::Regenerable, rel);
            let measured = (sizes && regenerable_match)
                .then(|| data::sizes(&entry))
                .flatten();
            IgnoredEntry {
                path: display(&entry),
                class,
                regenerable_match,
                allocated_bytes: measured.map(|m| m.allocated),
                aliased_bytes: measured.and_then(|m| m.aliased),
            }
        })
        .collect()
}

/// The status `out` printed, when it is complete. Status exits 0 when it cannot read a
/// directory (`warning: could not open directory 'private/': Permission denied`) and leaves
/// out what is below it, which could hold untracked or ignored files, so any diagnostic makes
/// the whole status, the ignored list and `dirty` with it, unknown rather than a complete list.
fn complete_status(out: &Output) -> Result<git::Status, String> {
    match out.stderr_line() {
        line if line.is_empty() => git::parse_status(&out.stdout),
        line => Err(format!("git status is incomplete: {line}")),
    }
}

/// Whether a worktree is in use, and whether every source of that answer could be read: its
/// in-use lock is held, a live or recovery-needed reservation names it, or a process other
/// than this one works inside it.
fn in_use(path: &Path, shared: &Shared<'_>) -> (InUse, bool) {
    let key = worktree_key(path);
    let mut known = true;
    let lock_held = match probe(&shared.state.worktree_lock(&key)) {
        Ok(Probe::Held) => true,
        Ok(Probe::Free | Probe::Missing) => false,
        Err(_) => {
            known = false;
            false
        }
    };
    let jobs: Vec<String> = if let Some(list) = &shared.reservations {
        list.iter()
            .filter(|view| view.derived != DerivedState::Stale)
            .filter(|view| {
                view.reservation
                    .worktree
                    .as_ref()
                    .is_some_and(|w| w.key == key)
            })
            .map(|view| view.reservation.job_id.clone())
            .collect()
    } else {
        known = false;
        Vec::new()
    };
    let me = crate::os::current_pid();
    let process_inside = if let Some(table) = &shared.processes {
        table.iter().any(|p| {
            p.pid != me && !p.zombie && p.cwd.as_ref().is_some_and(|cwd| cwd.starts_with(path))
        })
    } else {
        known = false;
        false
    };
    let value = lock_held || !jobs.is_empty() || process_inside;
    (InUse { value, jobs }, known)
}

/// What the reclaim summary needs beyond the worktree's facts.
struct ReclaimInputs {
    status_known: bool,
    /// A git or gh step of this worktree or its repository failed.
    step_failed: bool,
    in_use_known: bool,
    /// `None` when the index could not be read.
    tracked: Option<Tracked>,
    plain_target: CheckResult,
}

/// The whole-worktree checks of section 9.2 as observation sees them. A main worktree is
/// never a candidate.
fn reclaim_summary(worktree: &Worktree, inputs: &ReclaimInputs) -> ReclaimSummary {
    let f = &worktree.facts;
    let from_tri = |fact: Tri, pass_when: Tri| match fact {
        Tri::Unknown => CheckResult::Unknown,
        value if value == pass_when => CheckResult::Pass,
        _ => CheckResult::Fail,
    };
    let any_class = |class: IgnoredClass| worktree.ignored.iter().any(|e| e.class == class);
    let ignored_check = |class: IgnoredClass| {
        if !inputs.status_known {
            CheckResult::Unknown
        } else if any_class(class) {
            CheckResult::Fail
        } else {
            CheckResult::Pass
        }
    };
    let observed = inputs.status_known
        && !inputs.step_failed
        && [f.dirty, f.has_upstream, f.unpushed, f.open_pr]
            .iter()
            .all(|t| *t != Tri::Unknown)
        && f.merge_evidence != MergeEvidence::Unknown;
    // A check fails on the ignored entries or the tracked ones, and is unknown when either
    // could not be read.
    let with_tracked = |ignored: CheckResult, tracked: Option<bool>| match (ignored, tracked) {
        (CheckResult::Fail, _) | (_, Some(true)) => CheckResult::Fail,
        (CheckResult::Unknown, _) | (_, None) => CheckResult::Unknown,
        _ => CheckResult::Pass,
    };
    let nested = with_tracked(
        ignored_check(IgnoredClass::NestedRepository),
        inputs.tracked.map(|t| t.nested),
    );
    let protected = with_tracked(
        ignored_check(IgnoredClass::Protected),
        inputs.tracked.map(|t| t.protected),
    );
    let checks = [
        (
            "observed",
            if observed {
                CheckResult::Pass
            } else {
                CheckResult::Fail
            },
        ),
        (
            "not_in_use",
            if f.in_use.value {
                CheckResult::Fail
            } else if inputs.in_use_known {
                CheckResult::Pass
            } else {
                CheckResult::Unknown
            },
        ),
        (
            "not_managed",
            if worktree.managed_by.is_some() {
                CheckResult::Fail
            } else {
                CheckResult::Pass
            },
        ),
        ("clean", from_tri(f.dirty, Tri::No)),
        ("no_unpushed", from_tri(f.unpushed, Tri::No)),
        ("no_open_pr", from_tri(f.open_pr, Tri::No)),
        ("has_upstream", from_tri(f.has_upstream, Tri::Yes)),
        ("plain_target", inputs.plain_target),
        ("no_protected", protected),
        (
            "no_unclassifiable",
            ignored_check(IgnoredClass::Unclassifiable),
        ),
        ("no_nested_repository", nested),
        (
            "merge_evidence",
            match f.merge_evidence {
                MergeEvidence::BaseAncestor | MergeEvidence::MergedPr => CheckResult::Pass,
                MergeEvidence::None => CheckResult::Fail,
                MergeEvidence::Unknown => CheckResult::Unknown,
            },
        ),
    ];
    let names = |want: CheckResult| -> Vec<String> {
        checks
            .iter()
            .filter(|(_, result)| *result == want)
            .map(|(name, _)| (*name).to_owned())
            .collect()
    };
    let failed = names(CheckResult::Fail);
    let unknown = names(CheckResult::Unknown);
    ReclaimSummary {
        candidate: !worktree.main && failed.is_empty() && unknown.is_empty(),
        failed,
        unknown,
    }
}

/// `plain_target` for a worktree root: not a symbolic link, not a mount point, and on the
/// same device as its parent. On Unix the device numbers of the root and its parent are
/// compared, then their mounts (`os::mount`), so a bind mount on the same device is a mount
/// point too. On Windows a root that is not a reparse point passes: another volume, a
/// junction, or a symbolic link is reachable inside a path only through a reparse point.
fn plain_target(path: &Path) -> CheckResult {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return CheckResult::Unknown;
    };
    if meta.file_type().is_symlink() {
        return CheckResult::Fail;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Some(parent) = path.parent() else {
            return CheckResult::Fail;
        };
        match std::fs::metadata(parent) {
            Ok(parent) if parent.dev() == meta.dev() => {}
            Ok(_) => return CheckResult::Fail,
            Err(_) => return CheckResult::Unknown,
        }
        match (
            crate::os::mount::of_path(parent),
            crate::os::mount::of_path(path),
        ) {
            (Ok(parent), Ok(own)) if parent == own => CheckResult::Pass,
            (Ok(_), Ok(_)) => CheckResult::Fail,
            _ => CheckResult::Unknown,
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            CheckResult::Pass
        } else {
            CheckResult::Fail
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        CheckResult::Unknown
    }
}

/// The budgets, the resources of live and recovery-needed reservations, and the reservations.
fn budget_view(
    config: &Config,
    reservations: Option<Vec<ReservationView>>,
) -> (Resources, Resources, Vec<ReservationView>) {
    let mut reserved = Resources::new();
    let list = reservations.unwrap_or_default();
    for view in list.iter().filter(|v| v.derived != DerivedState::Stale) {
        for (name, amount) in &view.reservation.resources {
            reserved
                .entry(name.clone())
                .and_modify(|sum| *sum = sum.saturating_add(amount.get()))
                .or_insert(*amount);
        }
    }
    (config.host.budget.clone(), reserved, list)
}

/// Processes of session snapshots that are still alive after their agent process ended.
fn outlived(
    state: &StateDir,
    processes: Option<&ProcessTable>,
    errors: &mut Vec<ScopedError>,
) -> Vec<Outlived> {
    let Some(table) = processes else {
        return Vec::new();
    };
    let dir = state.sessions_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) => {
            errors.push(scoped("sessions", format!("{}: {err}", dir.display())));
            return Vec::new();
        }
    };
    let mut snapshots: BTreeMap<String, SessionSnapshot> = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        match std::fs::read(&path)
            .map_err(|err| err.to_string())
            .and_then(|bytes| {
                serde_json::from_slice::<SessionSnapshot>(&bytes).map_err(|err| err.to_string())
            }) {
            Ok(snapshot) => {
                snapshots.insert(snapshot.session_id.clone(), snapshot);
            }
            // A snapshot removed meanwhile is simply gone.
            Err(_) if !path.exists() => {}
            Err(message) => errors.push(scoped(&format!("session:{}", path.display()), message)),
        }
    }
    snapshots
        .into_values()
        .filter(|s| !table.is_alive(s.agent.pid, s.agent.start_s))
        .filter_map(|s| {
            let alive: Vec<_> = s
                .processes
                .into_iter()
                .filter(|p| table.is_alive(p.pid, p.start_s))
                .collect();
            (!alive.is_empty()).then_some(Outlived {
                session_id: s.session_id,
                ended_ms: s.ended_ms,
                processes: alive,
            })
        })
        .collect()
}

fn tri(value: bool) -> Tri {
    if value { Tri::Yes } else { Tri::No }
}

fn scoped(scope: &str, message: impl Into<String>) -> ScopedError {
    ScopedError {
        scope: scope.to_owned(),
        message: message.into(),
    }
}

/// The canonical form of `path` (without `\\?\` on Windows), or `path` itself when it does
/// not exist.
fn canonical(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Why a promisor repository's objects may not be read with the git of `version` (`None`:
/// unreadable), if that is so: before 2.45.1 git fetches despite `GIT_NO_LAZY_FETCH`.
fn promisor_unreadable(version: Option<(u32, u32, u32)>) -> Option<String> {
    match version {
        Some(version) if version >= NO_LAZY_FETCH_SINCE => None,
        Some((major, minor, patch)) => Some(format!(
            "a partial clone, and git {major}.{minor}.{patch} would fetch missing objects (GIT_NO_LAZY_FETCH holds in every read from git 2.45.1): objects are not read"
        )),
        None => Some(
            "a partial clone, and the git version cannot be read: objects are not read".to_owned(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};

    use super::{CHECK_ATTR, check_attr_commands, git_prefix, plain_target, promisor_unreadable};
    use crate::contract::CheckResult;

    /// The length in UTF-16 units of the command line the standard library builds on Windows
    /// to start `program` with `args`, or more: the program quoted, each argument quoted
    /// where it has a space, with backslashes doubled before a `"` it escapes. A batch file
    /// (`.bat`, `.cmd`) runs as `cmd.exe /e:ON /v:OFF /d /c ""<script>" <args>"`, each
    /// argument quoted here and each `%` written `%%cd:~,%`.
    fn windows_command_line(program: &str, args: &[OsString]) -> usize {
        let ext = program.rsplit('.').next().unwrap_or_default();
        let batch = ext.eq_ignore_ascii_case("bat") || ext.eq_ignore_ascii_case("cmd");
        let mut line = String::new();
        if batch {
            line.push_str("cmd.exe /e:ON /v:OFF /d /c \"");
        }
        line.push('"');
        line.push_str(program);
        line.push('"');
        for arg in args {
            let arg = arg.to_str().unwrap();
            let quote = batch || arg.is_empty() || arg.contains([' ', '\t']);
            line.push(' ');
            if quote {
                line.push('"');
            }
            let mut backslashes = 0;
            for c in arg.chars() {
                match c {
                    '\\' => backslashes += 1,
                    '"' => {
                        line.push_str(&"\\".repeat(backslashes + 1));
                        backslashes = 0;
                    }
                    '%' if batch => {
                        line.push_str("%%cd:~,");
                        backslashes = 0;
                    }
                    _ => backslashes = 0,
                }
                line.push(c);
            }
            if quote {
                line.push_str(&"\\".repeat(backslashes));
                line.push('"');
            }
        }
        if batch {
            line.push('"');
        }
        line.encode_utf16().count()
    }

    /// The paths `git check-attr` gets are split into commands that fit: on Windows each
    /// whole command line stays within the 32,767 characters of `CreateProcessW`, or the
    /// 8,191 of cmd.exe for a batch file, whose `%` take 8 characters each; elsewhere the
    /// paths of a command take at most 64 KiB. Each command has every path once, in order,
    /// and all but the last fill more than half their limit; a path too long for any command
    /// goes alone.
    #[test]
    fn check_attr_commands_fit_the_command_line() {
        let paths: Vec<PathBuf> = (0..3000)
            .map(|n| PathBuf::from(format!("assets/textures/{n:05}/50% diffuse-map.bin")))
            .collect();
        let exe = r"C:\Program Files\Git\cmd\git.exe";
        let batch = r"C:\Program Files\Git\cmd\git.CMD";
        let dir = Path::new(r"C:\Users\dev\src\some repository");
        let path_bytes = |args: &[OsString]| -> usize { args.iter().map(|a| a.len() + 3).sum() };
        for (program, windows) in [(exe, true), (batch, true), (batch, false)] {
            let case = format!("{program} on Windows: {windows}");
            let commands = check_attr_commands(OsStr::new(program), dir, &paths, windows);
            assert!(commands.len() > 1, "{case}");
            let mut seen = Vec::new();
            for (n, args) in commands.iter().enumerate() {
                assert_eq!(
                    args[..CHECK_ATTR.len()],
                    CHECK_ATTR.map(OsString::from),
                    "{case}"
                );
                let given = &args[CHECK_ATTR.len()..];
                seen.extend(given.iter().map(PathBuf::from));
                let (used, limit) = if windows {
                    let mut full = git_prefix(dir);
                    full.extend(args.iter().cloned());
                    let limit = if program == batch { 8_191 } else { 32_766 };
                    (windows_command_line(program, &full), limit)
                } else {
                    (path_bytes(given), 64 * 1024)
                };
                assert!(used <= limit, "{case}: command {n} takes {used} of {limit}");
                if n + 1 < commands.len() {
                    assert!(
                        used > limit / 2,
                        "{case}: command {n} takes {used} of {limit}"
                    );
                }
            }
            assert_eq!(seen, paths, "{case}");
        }
        let long = [PathBuf::from("x".repeat(40_000)), PathBuf::from("y")];
        let commands = check_attr_commands(OsStr::new(exe), dir, &long, true);
        let given: Vec<_> = commands
            .iter()
            .map(|args| &args[CHECK_ATTR.len()..])
            .collect();
        assert_eq!(
            given,
            [[long[0].clone()], [long[1].clone()]].map(|p| p.map(OsString::from))
        );
        let none = check_attr_commands(OsStr::new(exe), dir, &[], true);
        assert_eq!(none, Vec::<Vec<OsString>>::new());
    }

    /// A promisor repository's objects are read only with a git that honors
    /// `GIT_NO_LAZY_FETCH` in every read (2.45.1 and later; 2.45.0's rename detection
    /// fetches without asking it); an older or unreadable version fails closed.
    #[test]
    fn promisor_below_2_45_1_reads_no_objects() {
        for old in [Some((2, 45, 0)), Some((2, 44, 9)), Some((1, 99, 0)), None] {
            assert!(promisor_unreadable(old).is_some(), "{old:?}");
        }
        for new in [(2, 45, 1), (2, 54, 0), (3, 0, 0)] {
            assert_eq!(promisor_unreadable(Some(new)), None, "{new:?}");
        }
    }

    #[test]
    fn plain_target_passes_a_plain_directory_and_fails_a_link_to_one() {
        let scope = cardinal_test_support::Scope::new();
        let real = scope.path("real");
        let link = scope.path("link");
        std::fs::create_dir(&real).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // A junction needs no privilege, unlike a directory symbolic link.
        #[cfg(windows)]
        {
            let output = scope
                .shell("mklink /J link real", scope.root())
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        assert_eq!(plain_target(&real), CheckResult::Pass);
        assert_eq!(plain_target(&link), CheckResult::Fail);
        assert_eq!(plain_target(&scope.path("missing")), CheckResult::Unknown);
    }
}
