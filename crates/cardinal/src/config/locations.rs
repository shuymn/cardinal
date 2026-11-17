//! Config and state directory locations (`docs/architecture.md` section 3.1).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::ConfigError;

/// Which base directory to resolve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    Config,
    State,
}

impl Base {
    fn override_var(self) -> &'static str {
        match self {
            Self::Config => "CARDINAL_CONFIG_DIR",
            Self::State => "CARDINAL_STATE_DIR",
        }
    }
}

/// The config directory from the process environment.
pub fn config_dir() -> Result<PathBuf, ConfigError> {
    resolve(Base::Config, cfg!(windows), |name| std::env::var_os(name))
}

/// The state directory from the process environment.
pub fn state_dir() -> Result<PathBuf, ConfigError> {
    resolve(Base::State, cfg!(windows), |name| std::env::var_os(name))
}

/// Resolves a base directory from `env`. An empty variable counts as unset; a relative result
/// or a missing base is an error.
pub fn resolve(
    base: Base,
    windows: bool,
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, ConfigError> {
    let var = |name: &str| env(name).filter(|value| !value.is_empty());
    if let Some(dir) = var(base.override_var()) {
        return absolute(base.override_var(), PathBuf::from(dir));
    }
    if windows {
        let local = var("LOCALAPPDATA").ok_or_else(|| missing(base, "LOCALAPPDATA"))?;
        let leaf = match base {
            Base::Config => "config",
            Base::State => "state",
        };
        return absolute(
            "LOCALAPPDATA",
            Path::new(&local).join("cardinal").join(leaf),
        );
    }
    let (xdg, fallback) = match base {
        Base::Config => ("XDG_CONFIG_HOME", ".config"),
        Base::State => ("XDG_STATE_HOME", ".local/state"),
    };
    if let Some(dir) = var(xdg) {
        return absolute(xdg, PathBuf::from(dir).join("cardinal"));
    }
    let home = var("HOME").ok_or_else(|| missing(base, "HOME"))?;
    absolute("HOME", Path::new(&home).join(fallback).join("cardinal"))
}

fn absolute(source: &str, path: PathBuf) -> Result<PathBuf, ConfigError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(ConfigError::Location(format!(
            "{source} gives the relative path {}",
            path.display()
        )))
    }
}

fn missing(base: Base, var: &str) -> ConfigError {
    ConfigError::Location(format!(
        "{var} is not set and {} is not set",
        base.override_var()
    ))
}

/// Expands a leading `~` or `~/` with `home`.
pub(crate) fn expand_tilde(value: &str, home: Option<&Path>) -> Option<PathBuf> {
    if value == "~" {
        return home.map(Path::to_path_buf);
    }
    match value.strip_prefix("~/") {
        Some(rest) => home.map(|home| home.join(rest)),
        None => Some(PathBuf::from(value)),
    }
}

/// The home directory used for `~` in configuration values.
pub(crate) fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::{Base, expand_tilde, resolve};

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[cfg(unix)]
    #[test]
    fn unix_precedence() {
        let all = env(&[
            ("CARDINAL_STATE_DIR", "/override"),
            ("XDG_STATE_HOME", "/xdg"),
            ("HOME", "/home/u"),
        ]);
        assert_eq!(
            resolve(Base::State, false, all).unwrap(),
            PathBuf::from("/override")
        );
        let xdg = env(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/home/u")]);
        assert_eq!(
            resolve(Base::Config, false, xdg).unwrap(),
            PathBuf::from("/xdg/cardinal")
        );
        let home = env(&[("XDG_STATE_HOME", ""), ("HOME", "/home/u")]);
        assert_eq!(
            resolve(Base::State, false, home).unwrap(),
            PathBuf::from("/home/u/.local/state/cardinal")
        );
        let home = env(&[("HOME", "/home/u")]);
        assert_eq!(
            resolve(Base::Config, false, home).unwrap(),
            PathBuf::from("/home/u/.config/cardinal")
        );
    }

    #[cfg(unix)]
    #[test]
    fn relative_or_missing_base_is_an_error() {
        assert!(resolve(Base::State, false, env(&[("CARDINAL_STATE_DIR", "rel")])).is_err());
        assert!(resolve(Base::State, false, env(&[("XDG_STATE_HOME", "rel")])).is_err());
        assert!(resolve(Base::Config, false, env(&[])).is_err());
    }

    #[test]
    fn windows_uses_local_app_data() {
        let local = if cfg!(windows) {
            r"C:\Users\u\AppData\Local"
        } else {
            "/local"
        };
        let state = resolve(Base::State, true, env(&[("LOCALAPPDATA", local)])).unwrap();
        assert_eq!(state, Path::new(local).join("cardinal").join("state"));
        assert!(resolve(Base::Config, true, env(&[("HOME", "/home/u")])).is_err());
    }

    #[test]
    fn tilde_expansion() {
        let home = Path::new("/h");
        assert_eq!(
            expand_tilde("~/src", Some(home)),
            Some(PathBuf::from("/h/src"))
        );
        assert_eq!(expand_tilde("~", Some(home)), Some(PathBuf::from("/h")));
        assert_eq!(expand_tilde("~/src", None), None);
        assert_eq!(expand_tilde("/abs", None), Some(PathBuf::from("/abs")));
        assert_eq!(
            expand_tilde("~other/x", Some(home)),
            Some(PathBuf::from("~other/x"))
        );
    }
}
