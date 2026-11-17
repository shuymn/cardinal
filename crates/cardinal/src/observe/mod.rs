//! Observation: git and gh with constructed environments and deadlines, repository and
//! worktree facts, merge evidence, ignored data, disk usage, and outlived processes
//! (`docs/architecture.md` section 8).
//!
//! Every fact is read fresh. A git or gh call that fails to start, exits with a status that
//! carries no meaning for the fact, prints output that does not parse, or misses its
//! deadline makes the fact `unknown`, never "safe". Observation writes nothing: git runs
//! with `--no-optional-locks` and `GIT_OPTIONAL_LOCKS=0`, never `fetch`, and `inventory`
//! never deletes reservations or signals processes.

mod data;
mod gh;
mod git;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub use crate::os::child::{ChildError, Output};

use crate::admission;
use crate::config::{Config, DataList, Selection};
use crate::contract::{
    BaseRef, CheckResult, DerivedState, Error, ErrorCode, Facts, GhStatus, HostInfo, IgnoredClass,
    IgnoredEntry, InUse, Inventory, InventorySchema, MergeEvidence, Os, Outlived, PrRef,
    ReclaimSummary, Repo, ReservationView, Resources, ScopedError, SessionSnapshot, Tri, Worktree,
};
use crate::os::child;
use crate::os::{Detail, ProcessTable};
use crate::state::{Probe, StateDir, find_worktree_root, now_ms, probe, worktree_key};

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

/// Observes the worktree containing `path` from scratch; `plan` and `act` use it.
pub fn observe_worktree(
    config: &Config,
    state: &StateDir,
    path: &Path,
) -> Result<WorktreeObservation, Error> {
    Observer::new(config).observe_worktree(state, path)
}

/// Runs `git --no-optional-locks -C <dir> <args>` with the constructed environment of
/// section 8.1 and `[observe] git_timeout_ms`. This is the one way cardinal runs git.
pub fn git<S: AsRef<OsStr>>(config: &Config, dir: &Path, args: &[S]) -> Result<Output, ChildError> {
    Observer::new(config).git(dir, args)
}

/// Where observation reads reservations: `admission` in the product, a fake in tests of
/// observation alone.
pub trait Reservations: Sync {
    /// Every reservation with its derived state. Read-only.
    fn list(&self, state: &StateDir) -> Result<Vec<ReservationView>, Error>;
}

/// [`Reservations`] from `admission::list_reservations`.
#[derive(Clone, Copy, Debug, Default)]
pub struct AdmissionReservations;

impl Reservations for AdmissionReservations {
    fn list(&self, state: &StateDir) -> Result<Vec<ReservationView>, Error> {
        admission::list_reservations(state)
    }
}

/// Observation with its inputs: the configuration, the environment git and gh children are
/// built from, and the reservation source.
pub struct Observer<'a> {
    config: &'a Config,
    /// The environment of git children.
    env: Vec<(OsString, OsString)>,
    /// The environment of gh children.
    gh_env: Vec<(OsString, OsString)>,
    /// The caller's `GH_HOST`: a host besides `github.com` whose remotes are GitHub.
    gh_host: Option<String>,
    reservations: &'a dyn Reservations,
}

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

/// What one worktree observation found, besides the worktree itself.
struct WorktreeResult {
    worktree: Worktree,
    errors: Vec<ScopedError>,
    gh_error: Option<String>,
}

impl<'a> Observer<'a> {
    /// Observation with this process's environment and the admission reservations.
    #[must_use]
    pub fn new(config: &'a Config) -> Self {
        Self {
            config,
            env: Vec::new(),
            gh_env: Vec::new(),
            gh_host: None,
            reservations: &AdmissionReservations,
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

    /// Reads reservations from `reservations`.
    #[must_use]
    pub fn with_reservations(mut self, reservations: &'a dyn Reservations) -> Self {
        self.reservations = reservations;
        self
    }

    /// See [`git`](fn@git).
    pub fn git<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Result<Output, ChildError> {
        let observe = &self.config.host.observe;
        // No optional locks, and no filesystem monitor: a configured fsmonitor would start a
        // daemon that writes inside `.git`.
        let mut command: Vec<OsString> = vec![
            "--no-optional-locks".into(),
            "-c".into(),
            "core.fsmonitor=false".into(),
            "-C".into(),
            dir.into(),
        ];
        command.extend(args.iter().map(|a| a.as_ref().to_os_string()));
        child::run(
            &observe.git,
            &command,
            &self.env,
            dir,
            Instant::now() + Duration::from_millis(observe.git_timeout_ms),
        )
    }

    /// git that must exit 0; anything else is an error message.
    fn git_ok<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Result<Output, String> {
        let what = args.first().map_or("git".to_owned(), |a| {
            format!("git {}", a.as_ref().to_string_lossy())
        });
        self.git(dir, args)
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
            Instant::now() + Duration::from_millis(observe.gh_timeout_ms),
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
        Ok(Inventory {
            schema: InventorySchema,
            observed_ms: now_ms(),
            host,
            budget,
            reserved,
            reservations,
            repos,
            outlived,
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
        let reservations = self
            .reservations
            .list(state)
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
        let (base, failure) = match self.base(&dir, selection.base()) {
            Ok(base) => (base, None),
            Err(message) => (None, Some(message)),
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

        let (status, tracked) = self.index_facts(&path, &ctx.patterns, &mut note);
        let (head, branch, upstream) = match &status {
            Some(s) => (s.head.clone(), s.branch.clone(), s.upstream.clone()),
            None => (listed.head.clone(), listed.branch.clone(), None),
        };
        let head = head.filter(|h| git::is_oid(h));
        let dirty = status.as_ref().map_or(Tri::Unknown, |s| tri(s.dirty));
        let has_upstream = status
            .as_ref()
            .map_or(Tri::Unknown, |s| tri(s.upstream.is_some()));

        let history = self.history(ctx, &path, head.as_deref(), branch.as_deref(), &mut note);

        let (in_use, in_use_known) = in_use(&path, shared);

        let ignored = status.as_ref().map_or_else(Vec::new, |s| {
            ignored_entries(&path, &s.ignored, &ctx.patterns, shared.sizes)
        });
        let step_failed = !errors.is_empty() || history.gh_error.is_some() || ctx.failure.is_some();

        let facts = Facts {
            dirty,
            has_upstream,
            unpushed: history.unpushed,
            open_pr: history.open_pr,
            merge_evidence: history.merge_evidence,
            merged_pr: history.merged_pr,
            in_use,
        };
        let worktree = Worktree {
            path: display(&path),
            main,
            head,
            branch,
            upstream,
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
            gh_error: history.gh_error,
        }
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
    fn index_facts(
        &self,
        path: &Path,
        patterns: &DataPatterns,
        note: &mut dyn FnMut(String),
    ) -> (Option<git::Status>, Option<Tracked>) {
        match self.split_index(path) {
            Ok(false) => {}
            Ok(true) => {
                note("the worktree uses a split index, which reading would modify; its status and tracked files are not read".to_owned());
                return (None, None);
            }
            Err(message) => {
                note(message);
                return (None, None);
            }
        }
        let status = self
            .git_ok(
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
            )
            .and_then(|out| git::parse_status(&out.stdout))
            .map_err(&mut *note)
            .ok();
        let tracked = self.tracked(path, patterns).map_err(note).ok();
        (status, tracked)
    }

    /// Whether the worktree at `path` uses a split index: a `sharedindex.*` file in its git
    /// dir or the common dir, or `core.splitIndex` set. Reads only files and configuration,
    /// never the index.
    fn split_index(&self, path: &Path) -> Result<bool, String> {
        let git_dir = worktree_git_dir(path)
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
    fn tracked(&self, path: &Path, patterns: &DataPatterns) -> Result<Tracked, String> {
        let out = self.git_ok(path, &["ls-files", "-z", "--stage"])?;
        let mut tracked = Tracked::default();
        for entry in out.stdout.split(|&b| b == 0).filter(|e| !e.is_empty()) {
            let (meta, file) = git::parse_stage_entry(entry)?;
            if !tracked.protected && patterns.covers(DataList::Protected, &file) {
                tracked.protected = true;
            }
            if meta.starts_with("160000 ")
                && std::fs::symlink_metadata(path.join(&file).join(".git")).is_ok()
            {
                tracked.nested = true;
            }
        }
        Ok(tracked)
    }
}

/// Tracked content `act` would refuse.
#[derive(Clone, Copy, Debug, Default)]
struct Tracked {
    protected: bool,
    nested: bool,
}

/// The git dir of the worktree at `path`, read from its `.git` entry without running git:
/// the `.git` directory itself, or the `gitdir:` a `.git` file names.
fn worktree_git_dir(path: &Path) -> Option<PathBuf> {
    let dot_git = path.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = text.strip_prefix("gitdir:")?.trim();
    Some(path.join(gitdir))
}

/// The ignored entries of the worktree at `root`, classified, with sizes of regenerable ones
/// when `sizes` is set.
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
            let allocated_bytes = (sizes && class == IgnoredClass::Regenerable)
                .then(|| data::allocated_bytes(&entry))
                .flatten();
            IgnoredEntry {
                path: display(&entry),
                class,
                allocated_bytes,
            }
        })
        .collect()
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
/// compared. On Windows a root that is not a reparse point passes: another volume, a
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
            Ok(parent) if parent.dev() == meta.dev() => CheckResult::Pass,
            Ok(_) => CheckResult::Fail,
            Err(_) => CheckResult::Unknown,
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

#[cfg(test)]
mod tests {
    use super::plain_target;
    use crate::contract::CheckResult;

    #[test]
    fn plain_target_passes_a_plain_directory_and_fails_a_link_to_one() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let link = tmp.path().join("link");
        std::fs::create_dir(&real).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // A junction needs no privilege, unlike a directory symbolic link.
        #[cfg(windows)]
        assert!(
            std::process::Command::new("cmd")
                .env_clear()
                .envs(std::env::vars_os().filter(|(k, _)| {
                    ["PATH", "SystemRoot", "ComSpec", "PATHEXT"]
                        .iter()
                        .any(|keep| k.eq_ignore_ascii_case(keep))
                }))
                .current_dir(tmp.path())
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(&real)
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(plain_target(&real), CheckResult::Pass);
        assert_eq!(plain_target(&link), CheckResult::Fail);
        assert_eq!(
            plain_target(&tmp.path().join("missing")),
            CheckResult::Unknown
        );
    }
}
