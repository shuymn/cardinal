//! The `cardinal` binary with its own state and config directories
//! (`docs/architecture.md` section 13.5).

use std::path::{Path, PathBuf};

use cardinal_test_support::{Command, Scope};

/// A state and a config directory inside a scope of their own.
pub struct Dirs {
    scope: Scope,
    state: PathBuf,
    config: PathBuf,
}

impl Dirs {
    pub fn new() -> Self {
        let scope = Scope::new();
        let state = scope.path("state");
        let config = scope.path("config");
        std::fs::create_dir_all(&config).expect("create the config dir");
        Self {
            scope,
            state,
            config,
        }
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn state(&self) -> &Path {
        &self.state
    }

    pub fn config(&self) -> &Path {
        &self.config
    }

    /// `cardinal` in the scope root, with `CARDINAL_STATE_DIR` and `CARDINAL_CONFIG_DIR` set
    /// to these directories.
    pub fn cmd(&self) -> Command {
        let mut command = self
            .scope
            .command(env!("CARGO_BIN_EXE_cardinal"), self.scope.root());
        command
            .env("CARDINAL_STATE_DIR", &self.state)
            .env("CARDINAL_CONFIG_DIR", &self.config);
        command
    }
}
