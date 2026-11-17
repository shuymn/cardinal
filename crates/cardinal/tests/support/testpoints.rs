//! Driving the pause points of `cardinal::testpoint` (`docs/architecture.md` section 13.1).
//! A contended test waits for the second actor's `.reached` file before releasing the first.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use cardinal::testpoint::{reached_file, release_file};
use tempfile::TempDir;

use super::helper::wait_for;

/// A test-point directory.
pub struct TestPoints {
    dir: TempDir,
}

impl TestPoints {
    pub fn new() -> Self {
        Self {
            dir: tempfile::Builder::new()
                .prefix("cardinal-points-")
                .tempdir()
                .expect("create the test-point dir"),
        }
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Sets the test-point variables on `command`: pause at `pause`, fail at `fail`.
    pub fn apply(&self, command: &mut Command, pause: &[&str], fail: Option<&str>) {
        command
            .env("CARDINAL_TEST_DIR", self.dir())
            .env("CARDINAL_TEST_PAUSE", pause.join(","))
            .env("CARDINAL_TEST_FAIL", fail.unwrap_or_default());
    }

    /// Waits until process `pid` reached `point`.
    pub fn wait_reached(&self, point: &str, pid: u32, timeout: Duration) {
        let file = reached_file(self.dir(), point, pid);
        wait_for(&format!("{point} reached by {pid}"), timeout, || {
            file.exists().then_some(())
        });
    }

    /// Whether process `pid` reached `point`.
    pub fn reached(&self, point: &str, pid: u32) -> bool {
        reached_file(self.dir(), point, pid).exists()
    }

    /// Releases every process paused at `point`.
    pub fn release(&self, point: &str) {
        std::fs::write(release_file(self.dir(), point), b"").expect("write the release file");
    }
}
