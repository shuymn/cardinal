//! Configuration: locations, `host.toml`, and `repos/*.toml` profiles
//! (`docs/architecture.md` section 3).
//!
//! Unknown keys are rejected so that typos surface as `config_invalid`. The core ships no
//! default rules, wrappers, or data patterns.

mod host;
mod locations;
mod profile;

use std::path::{Path, PathBuf};

pub use host::{
    Act, Admission, Arbiter, Disk, DiskLines, Hook, HostConfig, MAX_ITEMS, Managed, Observe,
    Reconcile, Records, Sessions, Unit, VolumeLines, logical_cpus, validate_runner,
};
pub use locations::{Base, config_dir, resolve, state_dir};
pub use profile::{
    DataList, Matcher, Pattern, Profile, Rule, Selection, data_pattern_covers, data_patterns_cover,
    git_common_dir, git_dir, normalize_remote, without_userinfo,
};

use crate::contract::{Error, ErrorCode};

/// A configuration error. Every variant maps to `config_invalid`.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0}")]
    Location(String),
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
    #[error("{}: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
}

impl ConfigError {
    pub(crate) fn invalid(path: impl AsRef<Path>, message: impl Into<String>) -> Self {
        Self::Invalid {
            path: path.as_ref().to_path_buf(),
            message: message.into(),
        }
    }
}

impl From<ConfigError> for Error {
    fn from(err: ConfigError) -> Self {
        Error::new(ErrorCode::ConfigInvalid, err.to_string())
    }
}

/// `host.toml` and every profile of one config directory.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub dir: PathBuf,
    pub host: HostConfig,
    /// In file-name order.
    pub profiles: Vec<Profile>,
}

impl Config {
    /// Loads `<dir>/host.toml` (optional) and `<dir>/repos/*.toml` (in file-name order).
    /// A missing directory is an empty configuration.
    pub fn load(dir: &Path) -> Result<Self, ConfigError> {
        Self::load_with(dir, logical_cpus(), locations::home_dir().as_deref())
    }

    /// [`Config::load`] with the CPU count and the home directory for `~` given explicitly.
    pub fn load_with(dir: &Path, cpus: u64, home: Option<&Path>) -> Result<Self, ConfigError> {
        let host_path = dir.join("host.toml");
        let host = match read_optional(&host_path)? {
            Some(text) => HostConfig::parse(&host_path, &text, cpus, home)?,
            None => HostConfig::parse(&host_path, "", cpus, home)?,
        };

        let repos = dir.join("repos");
        let mut files = Vec::new();
        match std::fs::read_dir(&repos) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|source| ConfigError::Read {
                        path: repos.clone(),
                        source,
                    })?;
                    let path = entry.path();
                    if names_profile(&path) && is_profile(&path)? {
                        files.push(path);
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ConfigError::Read {
                    path: repos,
                    source,
                });
            }
        }
        files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

        let profiles = files
            .iter()
            .map(|path| {
                let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                    path: path.clone(),
                    source,
                })?;
                Profile::parse(path, &text, home)
            })
            .collect::<Result<_, _>>()?;

        Ok(Self {
            dir: dir.to_path_buf(),
            host,
            profiles,
        })
    }

    /// The profiles that apply to a repository; `matches` decides each non-generic profile
    /// (path matching first, then remote matching only if needed, as section 3.3 says).
    pub fn select(&self, matches: impl FnMut(&Profile) -> bool) -> Selection<'_> {
        Selection::new(&self.profiles, matches)
    }
}

impl HostConfig {
    /// Loads `<dir>/host.toml` (optional) and nothing else: the runner needs no profile, so a
    /// broken profile does not affect it.
    pub fn load(dir: &Path) -> Result<Self, ConfigError> {
        let path = dir.join("host.toml");
        let text = read_optional(&path)?.unwrap_or_default();
        Self::parse(
            &path,
            &text,
            logical_cpus(),
            locations::home_dir().as_deref(),
        )
    }

    /// [`Self::load`] for a reader whose fallback to defaults would change what it removes,
    /// the session hooks' lease pruning (section 9.8): only a `host.toml` with no directory
    /// entry is absent. An entry that is there but cannot be read is an error, `ENOENT` through
    /// a symbolic link whose target is missing included, as during an update that replaces
    /// the target.
    pub fn load_present(dir: &Path) -> Result<Self, ConfigError> {
        let path = dir.join("host.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err)
                if err.kind() == std::io::ErrorKind::NotFound
                    && std::fs::symlink_metadata(&path)
                        .is_err_and(|meta| meta.kind() == std::io::ErrorKind::NotFound) =>
            {
                String::new()
            }
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        Self::parse(
            &path,
            &text,
            logical_cpus(),
            locations::home_dir().as_deref(),
        )
    }
}

/// Whether `path`, an entry of `repos/`, is named as a profile: `*.toml`. [`Config::load`]
/// reads each such entry that is a file, through a final symbolic link, and nothing else
/// there.
pub(crate) fn names_profile(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "toml")
}

/// Whether the `*.toml` entry `path` of `repos/` is a profile: a regular file, a symbolic link
/// to one included. An entry whose `stat` fails is an error, never an absent profile, which
/// would drop its `protected` list (section 3.1) for this load; that includes an entry removed
/// since it was listed, as an editor that deletes a file and writes it again leaves it for a
/// moment, which the next load reads again.
fn is_profile(path: &Path) -> Result<bool, ConfigError> {
    std::fs::metadata(path)
        .map(|meta| meta.is_file())
        .map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })
}

fn read_optional(path: &Path) -> Result<Option<String>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::Config;

    #[test]
    fn missing_directory_is_an_empty_configuration() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config::load_with(&tmp.path().join("absent"), 4, None).unwrap();
        assert_eq!(config.profiles, []);
        assert_eq!(config.host.budget.get("cpu").map(|n| n.get()), Some(4));
    }

    #[test]
    fn profiles_load_in_file_name_order_and_errors_name_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repos = tmp.path().join("repos");
        std::fs::create_dir_all(&repos).unwrap();
        std::fs::write(repos.join("b.toml"), "").unwrap();
        std::fs::write(repos.join("a.toml"), "").unwrap();
        std::fs::write(repos.join("notes.txt"), "ignored").unwrap();
        let config = Config::load_with(tmp.path(), 4, None).unwrap();
        let names: Vec<_> = config.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);

        std::fs::write(repos.join("c.toml"), "[rule]\nbad = 1").unwrap();
        let err = Config::load_with(tmp.path(), 4, None).unwrap_err();
        assert!(err.to_string().contains("c.toml"), "{err}");
    }

    /// A profile that cannot be read is an error, never an absent profile: one whose `stat`
    /// fails in a `repos/` without search permission (which `Path::is_file` reads as no file),
    /// one whose content cannot be read, and a symbolic link to nothing. Its `protected` list
    /// would otherwise be dropped.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_profile_is_an_error() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let repos = tmp.path().join("repos");
        std::fs::create_dir_all(&repos).unwrap();
        let profile = repos.join("a.toml");
        std::fs::write(&profile, "[data]\nprotected = [\"fuzz/corpus\"]\n").unwrap();
        let loaded = Config::load_with(tmp.path(), 4, None).unwrap();
        assert_eq!(loaded.profiles[0].protected, ["fuzz/corpus"]);

        let mode = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        mode(&repos, 0o644);
        let unsearchable = std::fs::metadata(&profile).is_err();
        let result = Config::load_with(tmp.path(), 4, None);
        mode(&repos, 0o755);
        if unsearchable {
            let err = result.unwrap_err();
            assert!(err.to_string().contains("a.toml"), "{err}");
        } else {
            // Root reads past permissions, and so does the load.
            assert_eq!(result.unwrap().profiles.len(), 1);
        }

        mode(&profile, 0o000);
        let unreadable = std::fs::read(&profile).is_err();
        let result = Config::load_with(tmp.path(), 4, None);
        mode(&profile, 0o644);
        if unreadable {
            assert!(result.is_err(), "{result:?}");
        }

        std::os::unix::fs::symlink(repos.join("missing"), repos.join("b.toml")).unwrap();
        let err = Config::load_with(tmp.path(), 4, None).unwrap_err();
        assert!(err.to_string().contains("b.toml"), "{err}");
    }

    /// A `*.toml` entry removed between the listing of `repos/` and its `stat` is an error too:
    /// the load is read again later, while skipping it would drop its `protected` list.
    #[test]
    fn a_profile_removed_since_it_was_listed_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = super::is_profile(&tmp.path().join("gone.toml")).unwrap_err();
        assert!(err.to_string().contains("gone.toml"), "{err}");
    }

    #[test]
    fn shipped_examples_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/config");
        let config = Config::load_with(&dir, 8, Some(Path::new("/h"))).unwrap();
        assert!(config.profiles.iter().any(super::Profile::is_generic));
    }
}
