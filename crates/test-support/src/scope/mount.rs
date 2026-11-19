//! Bind mounts in a mount namespace of a child's own (Linux), for tests of how a program tells
//! mount points (`docs/architecture.md` section 13.3).
//!
//! The child enters a new user namespace and a mount namespace owned by it before its program
//! runs, so it can bind-mount without privileges, and its mounts end with the namespace. A
//! mount made there never propagates back to the parent's namespace. No ID map is written,
//! because the confinement's `/proc` is read-only: inside the namespace the child's IDs and
//! the owner of every file show as the overflow IDs, while the kernel still checks file access
//! against the child's real IDs.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;

use rustix::thread::{LinkNameSpaceType, UnshareFlags};

use super::{Child, Command, Scope, Slot, lock, resolve};

impl Scope {
    /// Makes `command` start its child in a user and mount namespace of its own, with each
    /// `(source, target)` of `binds` bind-mounted there before the program runs. Both paths of
    /// a bind must resolve inside the root. The child fails to start if the host forbids the
    /// namespace or a mount.
    pub fn mount_namespace(&self, command: &mut Command, binds: &[(&Path, &Path)]) {
        let binds = self.binds(binds);
        let enter = move || {
            let flags = UnshareFlags::NEWUSER | UnshareFlags::NEWNS;
            // SAFETY: `NEWUSER | NEWNS` leaves the descriptor table shared, so no thread can
            // see a descriptor from an unshared table; this child has only one thread.
            unsafe { rustix::thread::unshare_unsafe(flags) }?;
            for (source, target) in &binds {
                rustix::mount::mount_bind(source.as_c_str(), target.as_c_str())?;
            }
            Ok(())
        };
        // SAFETY: the closure runs in the child between fork and exec, where only
        // async-signal-safe work is sound. It makes system calls on paths made before the
        // fork, and allocates nothing and takes no lock.
        unsafe { command.inner.pre_exec(enter) };
    }

    /// Bind-mounts `source` over `target` in the mount namespace of `child`, started with
    /// [`Scope::mount_namespace`], from a short-lived process that joins its namespaces. Both
    /// paths must resolve inside the root. Fails if `child` has exited or the mount fails.
    pub fn bind_mount(&self, child: &Child, source: &Path, target: &Path) -> io::Result<()> {
        let mut binds = self.binds(&[(source, target)]);
        let (source, target) = binds.pop().expect("one bind");
        // The child is unreaped while its slot runs, so its PID names it while the
        // namespaces are opened.
        let (user, mount) = {
            let slot = lock(&child.slot);
            if !matches!(*slot, Slot::Running(_)) {
                return Err(io::Error::other("the child has exited"));
            }
            let ns = |name: &str| File::open(format!("/proc/{}/ns/{name}", child.id()));
            (ns("user")?, ns("mnt")?)
        };
        let join = move || {
            let enter = rustix::thread::move_into_link_name_space;
            enter(user.as_fd(), Some(LinkNameSpaceType::User))?;
            enter(mount.as_fd(), Some(LinkNameSpaceType::Mount))?;
            rustix::mount::mount_bind(source.as_c_str(), target.as_c_str())?;
            Ok(())
        };
        let mut command = self.command("true", self.root());
        // SAFETY: as in `mount_namespace`: system calls on descriptors and paths made before
        // the fork, no allocation, no lock.
        unsafe { command.inner.pre_exec(join) };
        let status = command.status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "the bind mount process: {status}"
            )))
        }
    }

    /// Makes `command` start its child in a user namespace of its own with its root directory
    /// changed to `root`, a directory inside the scope's root, right before the program runs.
    /// The working directory stays where it was, so a relative program path resolves from it,
    /// outside `root`, while the program's interpreter and libraries resolve inside `root`. The
    /// child fails to start if the host forbids the namespace. The child gets the absolute path
    /// `root` resolves to here, the one checked, never a spelling it would resolve again from
    /// its own working directory.
    pub fn chroot(&self, command: &mut Command, root: &Path) {
        let root = resolve(&std::path::absolute(root).expect("an absolute root directory"));
        self.assert_inside(&root, "root directory");
        let root = CString::new(root.as_os_str().as_bytes()).expect("a path without NUL");
        let enter = move || {
            // SAFETY: as in `mount_namespace`: this child has only one thread.
            unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWUSER) }?;
            rustix::process::chroot(root.as_c_str())?;
            Ok(())
        };
        // SAFETY: as in `mount_namespace`: system calls on a path made before the fork, no
        // allocation, no lock.
        unsafe { command.inner.pre_exec(enter) };
    }

    /// `binds` as C strings, each path checked to resolve inside the root.
    fn binds(&self, binds: &[(&Path, &Path)]) -> Vec<(CString, CString)> {
        let c_path = |path: &Path, what: &str| {
            self.assert_inside(path, what);
            CString::new(path.as_os_str().as_bytes()).expect("a path without NUL")
        };
        binds
            .iter()
            .map(|(source, target)| (c_path(source, "bind source"), c_path(target, "bind target")))
            .collect()
    }
}
