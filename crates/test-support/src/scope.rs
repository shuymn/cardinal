//! A scope: a root directory the test owns, and the one way to start a process.
//!
//! Every child starts with `env_clear()` and the scope's constructed environment, in a working
//! directory inside the root, in a process group of its own (Unix) or a job object of its own
//! (Windows). Its cleanup slot is registered before it starts. When the child exits, is
//! killed, or its [`Child`] or the [`Scope`] is dropped, whatever is left of its group or job
//! is killed. A group is signalled only while its leader is unreaped, so its ID cannot have
//! been reused; a job holds only processes started inside it.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{ChildStderr, ChildStdin, ChildStdout, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

use tempfile::TempDir;

/// The variable the test harness sets to a unique value for each test executable it runs. A
/// scope passes it to every child, so the harness can find what a test left running.
pub const RUN_MARKER: &str = "CARDINAL_TEST_RUN";

/// Fixed identity and dates for every commit made inside a scope.
const IDENTITY: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "Fixture Author"),
    ("GIT_AUTHOR_EMAIL", "author@fixture.invalid"),
    ("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z"),
    ("GIT_COMMITTER_NAME", "Fixture Committer"),
    ("GIT_COMMITTER_EMAIL", "committer@fixture.invalid"),
    ("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z"),
];

/// Non-git variables Windows programs need to start at all.
const WINDOWS_PASSTHROUGH: &[&str] = &["SystemRoot", "SystemDrive", "windir", "PATHEXT", "ComSpec"];

/// Symbolic link chains longer than this are not followed further.
const MAX_LINKS: u32 = 40;

pub struct Scope {
    registry: Arc<Registry>,
    root: PathBuf,
    // Removed after the registry's children are gone (see `Drop`).
    _dir: TempDir,
}

impl Scope {
    /// A new root in the OS temp dir, outside any repository, with `home/` (and a git config
    /// that disables signing, hooks, maintenance, and every transport) and `tmp/`.
    #[must_use]
    pub fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("cardinal-scope-")
            .tempdir()
            .expect("create the scope root");
        let root = dunce::canonicalize(dir.path()).expect("canonical scope root");
        if let Some(repo) = root
            .ancestors()
            .skip(1)
            .find(|dir| std::fs::symlink_metadata(dir.join(".git")).is_ok())
        {
            panic!(
                "scopes must live outside any repository, but the temp dir {} is inside {}",
                root.display(),
                repo.display()
            );
        }
        for dir in [
            "home/.config",
            "home/.cache",
            "home/.local/state",
            "home/.local/share",
            "home/AppData/Roaming",
            "home/AppData/Local",
            "hooks",
            "tmp",
        ] {
            std::fs::create_dir_all(root.join(dir)).expect("create scope dirs");
        }
        let hooks = root.join("hooks").to_string_lossy().replace('\\', "/");
        std::fs::write(
            root.join("home/.gitconfig"),
            format!(
                "[core]\n\thooksPath = \"{hooks}\"\n\tautocrlf = false\n\tfsmonitor = false\n\
                 [commit]\n\tgpgsign = false\n[tag]\n\tgpgsign = false\n\
                 [init]\n\tdefaultBranch = main\n[gc]\n\tauto = 0\n[maintenance]\n\tauto = false\n\
                 [protocol]\n\tallow = never\n[credential]\n\thelper =\n"
            ),
        )
        .expect("write the scope git config");
        Self {
            registry: Arc::default(),
            root,
            _dir: dir,
        }
    }

    /// The canonical root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `root/rel`; panics if `rel` leaves the root.
    #[must_use]
    pub fn path(&self, rel: &str) -> PathBuf {
        let path = normalize(&self.root.join(rel));
        assert!(path.starts_with(&self.root), "{rel} leaves the scope root");
        path
    }

    /// The environment every child gets, built from scratch: `HOME`, the XDG directories,
    /// the temp dir, and git's global config inside the root, no system config, a ceiling at
    /// the root's parent, no terminal prompts, a fixed locale and commit identity; `PATH`, the
    /// harness's run marker, and on Windows the variables programs need to start come from
    /// `inherited`. Nothing else passes.
    #[must_use]
    pub fn env_from(&self, inherited: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
        let home = self.root.join("home");
        let tmp = self.root.join("tmp");
        let ceiling = self.root.parent().unwrap_or(&self.root).to_path_buf();
        let mut env: Vec<(OsString, OsString)> = vec![
            ("HOME".into(), home.clone().into()),
            ("XDG_CONFIG_HOME".into(), home.join(".config").into()),
            ("XDG_CACHE_HOME".into(), home.join(".cache").into()),
            ("XDG_STATE_HOME".into(), home.join(".local/state").into()),
            ("XDG_DATA_HOME".into(), home.join(".local/share").into()),
            ("GIT_CONFIG_GLOBAL".into(), home.join(".gitconfig").into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CEILING_DIRECTORIES".into(), ceiling.into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("LC_ALL".into(), "C".into()),
        ];
        env.extend(IDENTITY.iter().map(|(k, v)| ((*k).into(), (*v).into())));
        if cfg!(windows) {
            env.extend([
                ("USERPROFILE".into(), home.clone().into()),
                ("APPDATA".into(), home.join("AppData/Roaming").into()),
                ("LOCALAPPDATA".into(), home.join("AppData/Local").into()),
                ("TEMP".into(), tmp.clone().into()),
                ("TMP".into(), tmp.into()),
            ]);
        } else {
            env.push(("TMPDIR".into(), tmp.into()));
        }
        let pass = |name: &OsString| {
            let name = name.to_string_lossy();
            name.eq_ignore_ascii_case("PATH")
                || name == RUN_MARKER
                || (cfg!(windows)
                    && WINDOWS_PASSTHROUGH
                        .iter()
                        .any(|keep| name.eq_ignore_ascii_case(keep)))
        };
        env.extend(inherited.iter().filter(|(k, _)| pass(k)).cloned());
        env
    }

    /// [`Scope::env_from`] with this process's environment.
    #[must_use]
    pub fn env(&self) -> Vec<(OsString, OsString)> {
        self.env_from(&std::env::vars_os().collect::<Vec<_>>())
    }

    /// A command for `program` that runs in `cwd` (inside the root) with the constructed
    /// environment. Variables a test adds with [`Command::env`] are explicit, never inherited.
    pub fn command(&self, program: impl AsRef<OsStr>, cwd: &Path) -> Command {
        self.assert_inside(cwd, "working directory");
        #[expect(
            clippy::disallowed_methods,
            reason = "the spawn API: the one place tests construct a process"
        )]
        let mut inner = std::process::Command::new(program);
        inner.env_clear().envs(self.env()).current_dir(cwd);
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut inner, 0);
        Command {
            inner,
            registry: Arc::clone(&self.registry),
            stdin_set: false,
            stdout_set: false,
            stderr_set: false,
        }
    }

    /// `script` run by the platform shell in `cwd`, without startup files: `sh -c` on Unix
    /// (a non-interactive `sh` reads none), `cmd /D /C` on Windows (`/D` skips `AutoRun`).
    #[must_use]
    pub fn shell(&self, script: &str, cwd: &Path) -> Command {
        let mut command;
        if cfg!(windows) {
            command = self.command("cmd", cwd);
            command.args(["/D", "/C", script]);
        } else {
            command = self.command("sh", cwd);
            command.args(["-c", script]);
        }
        command
    }

    /// Panics unless `path` resolves inside the root.
    pub fn assert_inside(&self, path: &Path, what: &str) {
        let resolved = resolve(path);
        assert!(
            resolved.starts_with(&self.root),
            "refusing: {what} {} is outside the scope root {}",
            path.display(),
            self.root.display()
        );
    }

    /// Whether `path` resolves inside the root.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        resolve(path).starts_with(&self.root)
    }
}

impl Default for Scope {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        self.registry.cleanup_all();
    }
}

/// A child process being built. Unlike [`std::process::Command`], its environment and working
/// directory are fixed by the scope.
pub struct Command {
    inner: std::process::Command,
    registry: Arc<Registry>,
    stdin_set: bool,
    stdout_set: bool,
    stderr_set: bool,
}

impl Command {
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    /// Sets one variable explicitly, on top of the constructed environment.
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env(key, value);
        self
    }

    /// Removes one variable of the constructed environment.
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env_remove(key);
        self
    }

    pub fn stdin(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.inner.stdin(cfg);
        self.stdin_set = true;
        self
    }

    pub fn stdout(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.inner.stdout(cfg);
        self.stdout_set = true;
        self
    }

    pub fn stderr(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.inner.stderr(cfg);
        self.stderr_set = true;
        self
    }

    /// Starts the child. Its cleanup slot exists before it starts.
    pub fn spawn(&mut self) -> io::Result<Child> {
        self.check_shell();
        let slot = self.registry.register();
        let (process, stdin, stdout, stderr) = self.start()?;
        let pid = process.child.id();
        *lock(&slot) = Slot::Running(process);
        Ok(Child {
            pid,
            slot,
            stdin,
            stdout,
            stderr,
        })
    }

    #[cfg(unix)]
    fn start(&mut self) -> io::Result<Started> {
        #[expect(
            clippy::disallowed_methods,
            reason = "the spawn API: the one place tests start a process"
        )]
        let mut child = self.inner.spawn()?;
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        Ok((Process { child }, pipes.0, pipes.1, pipes.2))
    }

    #[cfg(windows)]
    fn start(&mut self) -> io::Result<Started> {
        let mut wrap =
            process_wrap::std::CommandWrap::from(std::mem::replace(&mut self.inner, placeholder()));
        wrap.wrap(process_wrap::std::JobObject);
        let result = wrap.spawn();
        self.inner = wrap.into_command();
        let mut child = result?;
        let pipes = (
            child.stdin().take(),
            child.stdout().take(),
            child.stderr().take(),
        );
        Ok((Process { child }, pipes.0, pipes.1, pipes.2))
    }

    /// Runs the child to completion with stdin closed and stdout and stderr captured, unless
    /// set otherwise.
    pub fn output(&mut self) -> io::Result<Output> {
        if !self.stdin_set {
            self.inner.stdin(Stdio::null());
        }
        if !self.stdout_set {
            self.inner.stdout(Stdio::piped());
        }
        if !self.stderr_set {
            self.inner.stderr(Stdio::piped());
        }
        self.spawn()?.wait_with_output()
    }

    /// Runs the child to completion and returns its status.
    pub fn status(&mut self) -> io::Result<ExitStatus> {
        self.spawn()?.wait()
    }

    /// [`Command::output`] for `assert_cmd` assertions; panics if the child cannot start.
    pub fn assert(&mut self) -> assert_cmd::assert::Assert {
        let output = self.output().expect("start the child");
        assert_cmd::assert::OutputAssertExt::assert(output)
    }

    /// `cmd` runs `AutoRun` commands from the registry unless `/D` comes first.
    fn check_shell(&self) {
        let program = Path::new(self.inner.get_program());
        let is_cmd = program
            .file_stem()
            .is_some_and(|stem| stem.eq_ignore_ascii_case("cmd"));
        if is_cmd {
            let first = self.inner.get_args().next();
            assert!(
                first.is_some_and(|arg| arg.eq_ignore_ascii_case("/D")),
                "refusing: cmd must start with /D so AutoRun does not run (use Scope::shell)"
            );
        }
    }
}

type Started = (
    Process,
    Option<ChildStdin>,
    Option<ChildStdout>,
    Option<ChildStderr>,
);

/// A command that is never run, standing in while the real one is lent to `process-wrap`.
#[cfg(windows)]
fn placeholder() -> std::process::Command {
    #[expect(
        clippy::disallowed_methods,
        reason = "a placeholder that is never spawned"
    )]
    std::process::Command::new("")
}

/// A started child. Dropping it kills what is left of its process group or job.
pub struct Child {
    pid: u32,
    slot: Arc<Mutex<Slot>>,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

impl Child {
    #[must_use]
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Waits for the child to exit, kills what is left of its group or job, and reaps it.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        drop(self.stdin.take());
        if let Slot::Done(status) = &*lock(&self.slot) {
            return Ok(*status);
        }
        #[cfg(unix)]
        if let Err(err) = unix::wait_exited(self.pid, true) {
            // Killed and reaped by another thread meanwhile.
            if let Slot::Done(status) = &*lock(&self.slot) {
                return Ok(*status);
            }
            return Err(err);
        }
        let mut slot = lock(&self.slot);
        match std::mem::replace(&mut *slot, Slot::Starting) {
            Slot::Running(process) => {
                let status = process.finish()?;
                *slot = Slot::Done(status);
                Ok(status)
            }
            Slot::Done(status) => {
                *slot = Slot::Done(status);
                Ok(status)
            }
            Slot::Starting => unreachable!("a started child has a process"),
        }
    }

    /// The exit status if the child has exited (then cleaned up as in [`Child::wait`]).
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let mut slot = lock(&self.slot);
        match &mut *slot {
            Slot::Done(status) => Ok(Some(*status)),
            Slot::Starting => unreachable!("a started child has a process"),
            Slot::Running(process) => {
                if !process.has_exited()? {
                    return Ok(None);
                }
                let Slot::Running(process) = std::mem::replace(&mut *slot, Slot::Starting) else {
                    unreachable!()
                };
                let status = process.finish()?;
                *slot = Slot::Done(status);
                Ok(Some(status))
            }
        }
    }

    /// Kills the child with its group or job (SIGKILL on Unix, `TerminateJobObject` on
    /// Windows) and reaps it.
    pub fn kill(&mut self) {
        lock(&self.slot).kill();
    }

    /// Waits as [`Child::wait`] and collects the piped stdout and stderr.
    pub fn wait_with_output(mut self) -> io::Result<Output> {
        drop(self.stdin.take());
        let read = |pipe: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut pipe) = pipe {
                    pipe.read_to_end(&mut buf).map(|_| buf)
                } else {
                    Ok(buf)
                }
            })
        };
        let stdout = read(
            self.stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        );
        let stderr = read(
            self.stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        );
        let status = self.wait()?;
        let join = |handle: std::thread::JoinHandle<io::Result<Vec<u8>>>| {
            handle.join().expect("pipe reader panicked")
        };
        Ok(Output {
            status,
            stdout: join(stdout)?,
            stderr: join(stderr)?,
        })
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        lock(&self.slot).kill();
    }
}

#[derive(Default)]
struct Registry {
    slots: Mutex<Vec<Arc<Mutex<Slot>>>>,
}

impl Registry {
    fn register(&self) -> Arc<Mutex<Slot>> {
        let slot = Arc::new(Mutex::new(Slot::Starting));
        let mut slots = lock(&self.slots);
        slots.retain(|slot| !matches!(*lock(slot), Slot::Done(_)));
        slots.push(Arc::clone(&slot));
        slot
    }

    fn cleanup_all(&self) {
        for slot in lock(&self.slots).iter() {
            lock(slot).kill();
        }
    }
}

enum Slot {
    /// Registered, not started yet (or the start failed).
    Starting,
    Running(Process),
    Done(ExitStatus),
}

impl Slot {
    fn kill(&mut self) {
        if let Slot::Running(process) = std::mem::replace(self, Slot::Starting) {
            // A failed reap leaves nothing this slot could still clean up.
            if let Ok(status) = process.kill() {
                *self = Slot::Done(status);
            }
        }
    }
}

struct Process {
    #[cfg(unix)]
    child: std::process::Child,
    /// The child in a job object it joined before it ran (`process-wrap` starts it suspended,
    /// assigns it, then resumes it).
    #[cfg(windows)]
    child: Box<dyn process_wrap::std::ChildWrapper>,
}

impl Process {
    fn has_exited(&mut self) -> io::Result<bool> {
        #[cfg(unix)]
        {
            unix::wait_exited(self.child.id(), false)
        }
        #[cfg(windows)]
        {
            // The handle std holds keeps the process unreaped; `try_wait` only reads.
            Ok(windows::leader(&mut *self.child).try_wait()?.is_some())
        }
    }

    /// The leader has exited: kill what is left of its group or job, then reap it.
    fn finish(mut self) -> io::Result<ExitStatus> {
        #[cfg(unix)]
        {
            unix::kill_group(self.child.id());
            self.child.wait()
        }
        #[cfg(windows)]
        {
            let status = windows::leader(&mut *self.child).wait();
            let _ = self.child.start_kill();
            status
        }
    }

    fn kill(mut self) -> io::Result<ExitStatus> {
        #[cfg(unix)]
        {
            unix::kill_group(self.child.id());
            let _ = self.child.kill();
            self.child.wait()
        }
        #[cfg(windows)]
        {
            let _ = self.child.start_kill();
            windows::leader(&mut *self.child).wait()
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::any::Any;

    use process_wrap::std::ChildWrapper;

    /// The `std` child at the core of the wrappers. Its own `wait` returns when the leader
    /// exits; the job's `wait` would also wait for every process left in the job.
    pub(super) fn leader(child: &mut dyn ChildWrapper) -> &mut std::process::Child {
        if (child as &dyn Any).is::<std::process::Child>() {
            return (child as &mut dyn Any)
                .downcast_mut()
                .expect("checked above");
        }
        leader(child.inner_mut())
    }
}

#[cfg(unix)]
mod unix {
    use std::io;
    use std::time::Duration;

    use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, WaitIdStatus};

    fn pid(raw: u32) -> Pid {
        Pid::from_raw(i32::try_from(raw).expect("PIDs fit in i32")).expect("PIDs are positive")
    }

    /// Whether a `waitid` report is the end of the child. macOS also reports a stopped child
    /// to a wait for `WEXITED` alone.
    fn ended(status: &WaitIdStatus) -> bool {
        status.exited() || status.killed() || status.dumped()
    }

    /// Whether child `raw` has exited, leaving it unreaped so its PID and group ID stay
    /// reserved; with `block`, waits until it has. A stopped child has not exited.
    pub(super) fn wait_exited(raw: u32, block: bool) -> io::Result<bool> {
        let mut options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
        if !block {
            options |= WaitIdOptions::NOHANG;
        }
        loop {
            match rustix::process::waitid(WaitId::Pid(pid(raw)), options) {
                Ok(Some(status)) if ended(&status) => return Ok(true),
                Ok(_) if !block => return Ok(false),
                // A stop report stays until the child changes again; look again later
                // instead of spinning on it.
                Ok(_) => std::thread::sleep(Duration::from_millis(50)),
                Err(rustix::io::Errno::INTR) => {}
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// SIGKILL to the process group led by unreaped child `raw`. The group may be empty.
    pub(super) fn kill_group(raw: u32) {
        let _ = rustix::process::kill_process_group(pid(raw), Signal::KILL);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Removes `.` and `..` lexically.
pub(crate) fn normalize(path: &Path) -> PathBuf {
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
pub(crate) fn resolve(path: &Path) -> PathBuf {
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
