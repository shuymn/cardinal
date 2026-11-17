//! A scope: a root directory the test owns, and the one way to start a process.
//!
//! Every child starts with `env_clear()` and the scope's constructed environment, in a working
//! directory inside the root, in a process group of its own (Unix) or a job object of its own
//! (Windows). On Unix it holds no descriptor of the test process but the standard streams it is
//! given and the scope's marker (`crate::marker`), which the guards of `crate::process` go by.
//! Its cleanup slot is registered before it starts. When the child exits, is killed, or its
//! [`Child`] or the [`Scope`] is dropped, whatever is left of its group or job is killed. A group is signalled only while its leader is unreaped, so its ID cannot have
//! been reused; a job holds only processes started inside it. On Unix a child can instead
//! lead a new session ([`Command::new_session`]), whose group it leads too, or join the group
//! another child leads ([`Command::join_group`]); a child that joined a group is cleaned up
//! alone, and its group goes with its leader.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{ChildStderr, ChildStdin, ChildStdout, ExitStatus, Output, Stdio};
#[cfg(unix)]
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use tempfile::TempDir;

#[cfg(unix)]
use crate::marker::Marker;

/// The variable the test harness sets to a value unique to each harness run. A scope passes
/// it to every child, so the harness can find what a test left running.
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
    #[cfg(unix)]
    marker: Arc<Marker>,
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
            #[cfg(unix)]
            marker: Arc::new(Marker::create(&root)),
            root,
            _dir: dir,
        }
    }

    /// The canonical root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file every child of this scope holds open (see [`Marker`]). A guard keeps it open
    /// for as long as it may signal, so the file's inode cannot be given to another file while
    /// the guard compares against it, even after the scope is gone.
    #[cfg(unix)]
    pub(crate) fn marker(&self) -> Arc<Marker> {
        Arc::clone(&self.marker)
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
        let highest = Arc::new(AtomicI32::new(2));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            let highest = Arc::clone(&highest);
            let marker = Arc::clone(&self.marker);
            // SAFETY: the closure runs in the child between fork and exec. It reads integers
            // and makes system calls that only read and set descriptor flags, all
            // async-signal-safe, and allocates nothing.
            unsafe {
                inner.pre_exec(move || {
                    unix::close_others_on_exec(highest.load(Ordering::Relaxed), marker.keep());
                    unix::inherit(marker.raw());
                    Ok(())
                });
            }
        }
        Command {
            inner,
            registry: Arc::clone(&self.registry),
            #[cfg(unix)]
            group: Group::New,
            #[cfg(unix)]
            highest,
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
    #[cfg(unix)]
    group: Group,
    /// The highest descriptor this process held when the child last started, for the child's
    /// `pre_exec` (see `unix::close_others_on_exec`).
    #[cfg(unix)]
    highest: Arc<AtomicI32>,
    stdin_set: bool,
    stdout_set: bool,
    stderr_set: bool,
}

/// The process group a Unix child starts in.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq)]
enum Group {
    /// A new group the child leads.
    New,
    /// A new session, whose group the child leads.
    Session,
    /// The group of the given unreaped child, which leads it.
    Join(i32),
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

    /// Sets every variable `other` sets and removes every one it removes, as [`Command::env`]
    /// and [`Command::env_remove`] would: for a program, such as a shell, that starts the
    /// program `other` was built for and passes its environment on. Arguments, working
    /// directory, and stdio stay this command's.
    pub fn copy_env(&mut self, other: &Command) -> &mut Self {
        for (key, value) in other.get_envs() {
            match value {
                Some(value) => self.inner.env(key, value),
                None => self.inner.env_remove(key),
            };
        }
        self
    }

    /// Starts the child as the leader of a new session (`setsid`) instead of a new group in
    /// the test's session; it leads the session's first group, which is cleaned up as any
    /// child's group. For a test that needs a session of its own, such as one in which a
    /// job-control shell gives a job its own group, independent of the session the test runs
    /// in (a terminal's, launchd's on macOS, a container's).
    #[cfg(unix)]
    pub fn new_session(&mut self) -> &mut Self {
        assert!(
            self.group != Group::Session,
            "refusing: new_session was already called"
        );
        assert!(
            self.group == Group::New,
            "refusing: a child that joins a group cannot lead a session"
        );
        self.group = Group::Session;
        // SAFETY: the closure runs between `fork` and `exec` and calls only `setsid`, which
        // is async-signal-safe; it allocates nothing.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut self.inner, || {
                rustix::process::setsid()?;
                Ok(())
            });
        }
        self
    }

    /// Starts the child in the process group `leader` leads instead of a new one, as a shell
    /// without job control runs a command in its own group. `leader` must be unreaped and lead
    /// its group, so the group's ID is still its own when the child starts. The group
    /// is `leader`'s: cleaning up this child kills and reaps it alone, and the group is killed
    /// with its leader. A group this child makes later (`setpgid(0, 0)`) is not cleaned up
    /// here; a [`crate::process::GroupGuard`] or the harness's sweep covers it.
    #[cfg(unix)]
    pub fn join_group(&mut self, leader: &Child) -> &mut Self {
        assert!(
            self.group == Group::New,
            "refusing: a child joins one group and does not lead a session"
        );
        let leads = matches!(&*lock(&leader.slot), Slot::Running(process) if process.leads_group);
        assert!(
            leads,
            "refusing: join the group of child {}, which was reaped or does not lead a group",
            leader.pid
        );
        self.group = Group::Join(i32::try_from(leader.pid).expect("PIDs fit in i32"));
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

    /// The program, as given to [`Scope::command`].
    #[must_use]
    pub fn get_program(&self) -> &OsStr {
        self.inner.get_program()
    }

    /// The arguments, without the program.
    pub fn get_args(&self) -> std::process::CommandArgs<'_> {
        self.inner.get_args()
    }

    /// The whole environment the child gets: the scope's constructed environment with the
    /// changes made through [`Command::env`] and [`Command::env_remove`] (a removed variable
    /// has `None` as its value).
    pub fn get_envs(&self) -> std::process::CommandEnvs<'_> {
        self.inner.get_envs()
    }

    /// The working directory inside the scope root.
    #[must_use]
    pub fn get_current_dir(&self) -> Option<&Path> {
        self.inner.get_current_dir()
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
        use std::os::unix::process::CommandExt as _;
        // A session leader's `setsid` runs in `pre_exec` (see `new_session`) and would fail
        // in a process that already leads a group.
        match self.group {
            Group::New => {
                self.inner.process_group(0);
            }
            Group::Join(pgid) => {
                self.inner.process_group(pgid);
            }
            Group::Session => {}
        }
        self.highest
            .store(unix::highest_descriptor(), Ordering::Relaxed);
        #[expect(
            clippy::disallowed_methods,
            reason = "the spawn API: the one place tests start a process"
        )]
        let mut child = self.inner.spawn()?;
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let process = Process {
            child,
            leads_group: !matches!(self.group, Group::Join(_)),
        };
        Ok((process, pipes.0, pipes.1, pipes.2))
    }

    #[cfg(windows)]
    fn start(&mut self) -> io::Result<Started> {
        let mut wrap =
            process_wrap::std::CommandWrap::from(std::mem::replace(&mut self.inner, placeholder()));
        wrap.wrap(process_wrap::std::JobObject);
        #[expect(
            clippy::disallowed_methods,
            reason = "the spawn API: the one place tests start a process"
        )]
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
        self.capture();
        self.spawn()?.wait_with_output()
    }

    /// [`Command::output`] with one deadline, `limit` from now, for the child to exit and for
    /// its output to end. A child still running then is killed as [`Child::kill`] kills it;
    /// either way the result is a [`io::ErrorKind::TimedOut`] error. Output that a process
    /// outside the child's group or job holds open is left to the process that holds it.
    pub fn output_within(&mut self, limit: Duration) -> io::Result<Output> {
        let deadline = Instant::now() + limit;
        self.capture();
        let mut child = self.spawn()?;
        drop(child.stdin.take());
        let readers = Readers::start(child.stdout.take(), child.stderr.take());
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill();
                return Err(timed_out(&format!(
                    "the child did not exit within {limit:?}"
                )));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let (stdout, stderr) = readers.collect(Some(deadline))?.ok_or_else(|| {
            timed_out(&format!("the child's output did not end within {limit:?}"))
        })?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// Stdin closed and stdout and stderr piped, unless set otherwise.
    fn capture(&mut self) {
        if !self.stdin_set {
            self.inner.stdin(Stdio::null());
        }
        if !self.stdout_set {
            self.inner.stdout(Stdio::piped());
        }
        if !self.stderr_set {
            self.inner.stderr(Stdio::piped());
        }
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

/// A started child. Dropping it kills what is left of its process group or job (a child that
/// joined another child's group: the child alone).
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

    /// Waits for the child to exit, kills what is left of its group or job (unless it joined
    /// another child's group), and reaps it.
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
    /// Windows) and reaps it. A child that joined another child's group is killed alone.
    pub fn kill(&mut self) {
        lock(&self.slot).kill();
    }

    /// Kills the child alone (SIGKILL on Unix, `TerminateProcess` on Windows) and waits until
    /// it has exited, leaving the rest of its group or job running and the child unreaped. On
    /// Windows a job a descendant joined is nested in the child's job, so [`Child::kill`]
    /// would end it too. What is left is killed when the child is waited for or dropped.
    pub fn kill_leader(&mut self) {
        if let Slot::Running(process) = &mut *lock(&self.slot) {
            process.kill_leader();
        }
    }

    /// Waits as [`Child::wait`] and collects the piped stdout and stderr.
    pub fn wait_with_output(mut self) -> io::Result<Output> {
        drop(self.stdin.take());
        let readers = Readers::start(self.stdout.take(), self.stderr.take());
        let status = self.wait()?;
        let (stdout, stderr) = readers
            .collect(None)?
            .expect("without a deadline the readers finish");
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

/// A child's piped stdout and stderr, each read to its end on a thread of its own.
struct Readers {
    stdout: mpsc::Receiver<io::Result<Vec<u8>>>,
    stderr: mpsc::Receiver<io::Result<Vec<u8>>>,
}

impl Readers {
    fn start(stdout: Option<ChildStdout>, stderr: Option<ChildStderr>) -> Self {
        Self {
            stdout: read_to_end(stdout),
            stderr: read_to_end(stderr),
        }
    }

    /// Both outputs once they end, or `None` if one has not ended by `deadline`. A reader left
    /// waiting finishes when its pipe closes.
    fn collect(self, deadline: Option<Instant>) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        let receive = |rx: &mpsc::Receiver<io::Result<Vec<u8>>>| {
            let result = match deadline {
                None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                Some(deadline) => {
                    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                }
            };
            match result {
                Ok(read) => read.map(Some),
                Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err(io::Error::other("a pipe reader ended without its result"))
                }
            }
        };
        let (Some(stdout), Some(stderr)) = (receive(&self.stdout)?, receive(&self.stderr)?) else {
            return Ok(None);
        };
        Ok(Some((stdout, stderr)))
    }
}

/// Reads `pipe` to its end on a new thread; no pipe reads as empty.
fn read_to_end(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<io::Result<Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let read = match pipe {
            Some(mut pipe) => pipe.read_to_end(&mut buf).map(|_| buf),
            None => Ok(buf),
        };
        let _ = tx.send(read);
    });
    rx
}

fn timed_out(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, what)
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
    /// Kills and reaps a running child; a slot that is not running keeps its state, so a
    /// child killed twice still reports the status of the first kill.
    fn kill(&mut self) {
        if !matches!(self, Slot::Running(_)) {
            return;
        }
        let Slot::Running(process) = std::mem::replace(self, Slot::Starting) else {
            unreachable!("checked above")
        };
        // A failed reap leaves nothing this slot could still clean up.
        if let Ok(status) = process.kill() {
            *self = Slot::Done(status);
        }
    }
}

struct Process {
    #[cfg(unix)]
    child: std::process::Child,
    /// Whether the child leads its group; one that joined another child's group is cleaned up
    /// alone.
    #[cfg(unix)]
    leads_group: bool,
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
            if self.leads_group {
                unix::kill_group(self.child.id());
            }
            self.child.wait()
        }
        #[cfg(windows)]
        {
            let status = windows::leader(&mut *self.child).wait();
            let _ = self.child.start_kill();
            status
        }
    }

    fn kill_leader(&mut self) {
        #[cfg(unix)]
        {
            unix::kill_leader(self.child.id());
            let _ = unix::wait_exited(self.child.id(), true);
        }
        #[cfg(windows)]
        {
            let leader = windows::leader(&mut *self.child);
            let _ = leader.kill();
            // The handle std holds keeps the exited process listed; `wait` only waits.
            let _ = leader.wait();
        }
    }

    fn kill(mut self) -> io::Result<ExitStatus> {
        #[cfg(unix)]
        {
            if self.leads_group {
                unix::kill_group(self.child.id());
            }
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

    /// SIGKILL to unreaped child `raw` alone.
    pub(super) fn kill_leader(raw: u32) {
        let _ = rustix::process::kill_process(pid(raw), Signal::KILL);
    }

    /// SIGKILL to the process group led by unreaped child `raw`. The group may be empty.
    pub(super) fn kill_group(raw: u32) {
        let _ = rustix::process::kill_process_group(pid(raw), Signal::KILL);
    }

    /// The highest descriptor this process holds, listed from `/dev/fd`; no flag changes.
    pub(super) fn highest_descriptor() -> i32 {
        std::fs::read_dir("/dev/fd")
            .expect("list this process's descriptors")
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .max()
            .unwrap_or(2)
    }

    /// Run in a child between fork and exec: marks every descriptor from 3 up but those in
    /// `keep` (sorted) close-on-exec, so the child keeps only the standard streams set up for
    /// it and the scope's markers, never a descriptor that whatever started the test run left
    /// open. The test process's own flags never change. With `close_range` (Linux 5.11 and
    /// later) one call covers each run between kept descriptors; otherwise each descriptor up to
    /// `highest`, the highest the test process held when the child started: the descriptors it
    /// inherited are among those, and the ones it opened since are close-on-exec already, as the
    /// standard library opens them. Async-signal-safe: system calls only, nothing allocated.
    pub(super) fn close_others_on_exec(highest: i32, keep: &[i32]) {
        #[cfg(target_os = "linux")]
        {
            let mut first = 3_u32;
            let mut set = true;
            for &kept in keep {
                let kept = kept.cast_unsigned();
                if kept > first {
                    set &= cloexec_range(first, kept - 1);
                }
                first = first.max(kept + 1);
            }
            if set && cloexec_range(first, u32::MAX) {
                return;
            }
        }
        for raw in 3..=highest {
            if keep.contains(&raw) {
                continue;
            }
            // SAFETY: only the descriptor flags of `raw` are read and set; a number that is
            // not open fails with EBADF.
            let fd = unsafe { rustix::fd::BorrowedFd::borrow_raw(raw) };
            if let Ok(flags) = rustix::io::fcntl_getfd(fd) {
                let _ = rustix::io::fcntl_setfd(fd, flags | rustix::io::FdFlags::CLOEXEC);
            }
        }
    }

    /// `close_range(first, last, CLOSE_RANGE_CLOEXEC)`: whether it set the flag.
    #[cfg(target_os = "linux")]
    fn cloexec_range(first: u32, last: u32) -> bool {
        // SAFETY: with `CLOSE_RANGE_CLOEXEC` the call closes nothing; it only sets the
        // close-on-exec flag of the descriptors in the range.
        let set = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                first,
                last,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        };
        set == 0
    }

    /// Run in a child between fork and exec: clears close-on-exec on `raw`, the scope's marker,
    /// so the program the child runs holds it. Async-signal-safe.
    pub(super) fn inherit(raw: i32) {
        // SAFETY: only the descriptor flags of `raw` are read and set; the marker is open in
        // the test process, and so in the child.
        let fd = unsafe { rustix::fd::BorrowedFd::borrow_raw(raw) };
        if let Ok(flags) = rustix::io::fcntl_getfd(fd) {
            let _ = rustix::io::fcntl_setfd(fd, flags - rustix::io::FdFlags::CLOEXEC);
        }
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::io;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    use super::Scope;

    /// A child does not get a descriptor of this process that is not close-on-exec, and
    /// starting it leaves that descriptor's flags as they were.
    #[cfg(unix)]
    #[test]
    fn a_child_starts_without_the_parents_descriptors_and_leaves_their_flags() {
        use std::os::fd::{AsFd as _, AsRawFd as _};

        use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};

        let scope = Scope::new();
        let file = std::fs::File::open(scope.root()).expect("open the root");
        let fd = file.as_fd();
        fcntl_setfd(fd, fcntl_getfd(fd).unwrap() - FdFlags::CLOEXEC).unwrap();
        let raw = fd.as_raw_fd();
        let status = scope
            .shell(&format!("[ ! -e /dev/fd/{raw} ]"), scope.root())
            .status()
            .expect("run the shell");
        assert!(status.success(), "the child holds descriptor {raw}");
        assert!(
            !fcntl_getfd(fd).unwrap().contains(FdFlags::CLOEXEC),
            "the parent's descriptor flags changed"
        );
    }

    /// A child that has not exited by the deadline is killed, and the result says so.
    #[cfg(unix)]
    #[test]
    fn output_within_kills_a_child_that_runs_on() {
        let scope = Scope::new();
        let mut command = scope.command("sleep", scope.root());
        command.arg("30");
        let started = Instant::now();
        let err = command
            .output_within(Duration::from_millis(300))
            .expect_err("the child runs on");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// Output that a process outside the child's cleanup holds open ends the wait at the
    /// deadline: the joined shell is cleaned up alone, and its background `sleep` keeps stdout.
    #[cfg(unix)]
    #[test]
    fn output_within_stops_at_output_held_by_another_process() {
        let scope = Scope::new();
        let mut leader = scope.command("sleep", scope.root());
        leader.arg("30");
        let leader = leader.spawn().expect("start the group's leader");
        let mut command = scope.shell("sleep 30 & echo started", scope.root());
        command.join_group(&leader);
        let started = Instant::now();
        let err = command
            .output_within(Duration::from_millis(500))
            .expect_err("the output stays open");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// A second kill leaves the reaped child's status in place for `wait`.
    #[test]
    fn repeated_kill_keeps_the_status() {
        let scope = Scope::new();
        let mut command = if cfg!(windows) {
            let mut command = scope.command("ping", scope.root());
            command.args(["-n", "30", "127.0.0.1"]);
            command
        } else {
            let mut command = scope.command("sleep", scope.root());
            command.arg("30");
            command
        };
        let mut child = command.spawn().expect("start the child");
        child.kill();
        child.kill();
        let status = child.wait().expect("wait after two kills");
        assert!(!status.success(), "{status}");
    }
}
