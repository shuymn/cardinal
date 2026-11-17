//! Pseudo-terminals, for code that behaves differently when its stdin is a terminal.

use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::Stdio;

use rustix::fs::{Mode, OFlags};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

/// A pseudo-terminal whose slave end can be a child's stdin. Neither end becomes anyone's
/// controlling terminal (`O_NOCTTY`). Keep it until the child has exited: dropping it closes
/// the master, and the slave then reports a hangup.
pub struct Pty {
    _master: OwnedFd,
    slave: std::fs::File,
}

impl Pty {
    /// Opens a new pseudo-terminal.
    #[must_use]
    pub fn open() -> Self {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("open a pty");
        grantpt(&master).expect("grantpt");
        unlockpt(&master).expect("unlockpt");
        let name = ptsname(&master, Vec::new()).expect("ptsname");
        let slave = rustix::fs::open(
            Path::new(name.to_str().expect("UTF-8 pty name")),
            OFlags::RDWR | OFlags::NOCTTY,
            Mode::empty(),
        )
        .expect("open the pty slave");
        Self {
            _master: master,
            slave: std::fs::File::from(slave),
        }
    }

    /// The slave end, for [`crate::Command::stdin`].
    #[must_use]
    pub fn stdin(&self) -> Stdio {
        Stdio::from(self.slave.try_clone().expect("clone the pty slave"))
    }
}
