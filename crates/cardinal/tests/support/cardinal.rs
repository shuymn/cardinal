//! The `cardinal` binary with its own state and config directories
//! (`docs/architecture.md` section 13.5).

use std::path::{Path, PathBuf};

use tempfile::TempDir;

/// A state and a config directory in a fresh temp dir.
pub struct Dirs {
    _dir: TempDir,
    state: PathBuf,
    config: PathBuf,
}

impl Dirs {
    pub fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("cardinal-dirs-")
            .tempdir()
            .expect("create temp dirs");
        let root = dunce::canonicalize(dir.path()).expect("canonical temp dir");
        let state = root.join("state");
        let config = root.join("config");
        std::fs::create_dir_all(&config).expect("create the config dir");
        Self {
            _dir: dir,
            state,
            config,
        }
    }

    pub fn state(&self) -> &Path {
        &self.state
    }

    pub fn config(&self) -> &Path {
        &self.config
    }

    /// `cardinal` with `CARDINAL_STATE_DIR` and `CARDINAL_CONFIG_DIR` set to these directories
    /// and no test-point variables inherited.
    pub fn cmd(&self) -> assert_cmd::Command {
        let mut command = assert_cmd::Command::new(env!("CARGO_BIN_EXE_cardinal"));
        command
            .env("CARDINAL_STATE_DIR", &self.state)
            .env("CARDINAL_CONFIG_DIR", &self.config)
            .env_remove("CARDINAL_TEST_DIR")
            .env_remove("CARDINAL_TEST_PAUSE")
            .env_remove("CARDINAL_TEST_FAIL");
        command
    }
}
