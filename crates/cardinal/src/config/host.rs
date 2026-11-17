//! `host.toml` (`docs/architecture.md` section 3.2).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::ConfigError;
use super::locations::expand_tilde;
use crate::contract::Resources;

/// `host.toml` with every default applied.
#[derive(Clone, Debug, PartialEq)]
pub struct HostConfig {
    /// Admission budgets; a resource without a budget is unlimited.
    pub budget: Resources,
    pub admission: Admission,
    pub hook: Hook,
    pub observe: Observe,
    pub records: Records,
    /// `[unit.<name>]` tables. Phase 0 honors only `act`.
    pub units: BTreeMap<String, Unit>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Admission {
    /// Resources of a stage whose rule has none, and of `cardinal run` without a spec.
    pub default_resources: Resources,
    pub lock_timeout_ms: u64,
    pub max_wait_ms: u64,
    /// Share of the Bash timeout the synchronous wait may use.
    pub wait_fraction: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    pub enabled: bool,
    /// The configured runner path, unvalidated; see [`Hook::runner_path`].
    pub runner: Option<String>,
    pub deadline_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observe {
    pub git: String,
    pub gh: String,
    pub git_timeout_ms: u64,
    pub gh_timeout_ms: u64,
    /// Extra main worktrees to inventory, with `~` expanded.
    pub repos: Vec<PathBuf>,
    /// Roots of worktrees managed by other apps; those worktrees are only reported.
    pub managed: Vec<Managed>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Managed {
    pub app: String,
    pub root: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Records {
    pub max_bytes: u64,
    /// Number of record files kept, the current one included.
    pub keep: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unit {
    /// Whether `act` may execute for this unit; `false` makes it a dry run.
    pub act: bool,
}

impl HostConfig {
    /// The configuration of a host without `host.toml`.
    #[must_use]
    pub fn defaults() -> Self {
        let raw = RawHost::default();
        raw.resolve(Path::new("host.toml"), logical_cpus(), None)
            .expect("defaults are valid")
    }

    pub(super) fn parse(
        path: &Path,
        text: &str,
        cpus: u64,
        home: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        let raw: RawHost = toml::from_str(text).map_err(|err| ConfigError::Parse {
            path: path.to_path_buf(),
            message: err.to_string(),
        })?;
        raw.resolve(path, cpus, home)
    }

    /// Whether `act` called as `unit:<name>` may execute (`docs/architecture.md` section 9.6).
    /// Units without a table, and units with `act = false`, may not.
    #[must_use]
    pub fn unit_may_act(&self, name: &str) -> bool {
        self.units.get(name).is_some_and(|unit| unit.act)
    }
}

impl Hook {
    /// The runner path written into rewrites: `[hook] runner`, else this executable. It must be
    /// absolute and consist only of `A–Z a–z 0–9 / . _ + -`, so it never needs shell quoting.
    pub fn runner_path(&self) -> Result<String, ConfigError> {
        let path = if let Some(path) = &self.runner {
            path.clone()
        } else {
            let exe = std::env::current_exe().map_err(|err| {
                ConfigError::invalid("host.toml", format!("cannot find the runner: {err}"))
            })?;
            exe.to_str()
                .ok_or_else(|| ConfigError::invalid("host.toml", "the runner path is not UTF-8"))?
                .to_owned()
        };
        validate_runner(&path)?;
        Ok(path)
    }
}

/// Checks the `[hook] runner` rule (`docs/architecture.md` section 3.2).
pub fn validate_runner(path: &str) -> Result<(), ConfigError> {
    let allowed = |b: u8| b.is_ascii_alphanumeric() || b"/._+-".contains(&b);
    if !path.starts_with('/') {
        return Err(ConfigError::invalid(
            "host.toml",
            format!("hook.runner `{path}` is not an absolute path"),
        ));
    }
    if !path.bytes().all(allowed) {
        return Err(ConfigError::invalid(
            "host.toml",
            format!("hook.runner `{path}` contains characters other than A-Z a-z 0-9 / . _ + -"),
        ));
    }
    Ok(())
}

/// Logical CPUs, or 1 when the OS does not say.
#[must_use]
pub fn logical_cpus() -> u64 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u64)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawHost {
    budget: Resources,
    admission: RawAdmission,
    hook: RawHook,
    observe: RawObserve,
    records: RawRecords,
    unit: BTreeMap<String, RawUnit>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawAdmission {
    default_resources: Option<Resources>,
    lock_timeout_ms: u64,
    max_wait_ms: u64,
    wait_fraction: f64,
}

impl Default for RawAdmission {
    fn default() -> Self {
        Self {
            default_resources: None,
            lock_timeout_ms: 2000,
            max_wait_ms: 30_000,
            wait_fraction: 0.25,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawHook {
    enabled: bool,
    runner: Option<String>,
    deadline_ms: u64,
}

impl Default for RawHook {
    fn default() -> Self {
        Self {
            enabled: true,
            runner: None,
            deadline_ms: 500,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawObserve {
    git: String,
    gh: String,
    git_timeout_ms: u64,
    gh_timeout_ms: u64,
    repos: Vec<String>,
    managed: Vec<RawManaged>,
}

impl Default for RawObserve {
    fn default() -> Self {
        Self {
            git: "git".into(),
            gh: "gh".into(),
            git_timeout_ms: 10_000,
            gh_timeout_ms: 20_000,
            repos: Vec::new(),
            managed: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManaged {
    app: String,
    root: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawRecords {
    max_bytes: u64,
    keep: u32,
}

impl Default for RawRecords {
    fn default() -> Self {
        Self {
            max_bytes: 8 * 1024 * 1024,
            keep: 4,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawUnit {
    act: bool,
}

impl RawHost {
    fn resolve(
        self,
        path: &Path,
        cpus: u64,
        home: Option<&Path>,
    ) -> Result<HostConfig, ConfigError> {
        let invalid = |message: String| ConfigError::invalid(path, message);

        let mut budget = self.budget;
        budget.entry("cpu".to_owned()).or_insert(cpus);
        for name in budget.keys() {
            check_resource_name(name).map_err(|m| invalid(format!("budget: {m}")))?;
        }

        let default_resources = match self.admission.default_resources {
            Some(resources) => {
                check_resources(&resources)
                    .map_err(|m| invalid(format!("admission.default_resources: {m}")))?;
                resources
            }
            None => [("cpu".to_owned(), (cpus / 2).max(1))]
                .into_iter()
                .collect(),
        };
        let wait_fraction = self.admission.wait_fraction;
        if !(0.0..=1.0).contains(&wait_fraction) {
            return Err(invalid(format!(
                "admission.wait_fraction {wait_fraction} is not between 0 and 1"
            )));
        }

        let observe = self.observe;
        let repos = observe
            .repos
            .iter()
            .map(|repo| {
                absolute_path(repo, home).map_err(|m| invalid(format!("observe.repos: {m}")))
            })
            .collect::<Result<_, _>>()?;
        let managed = observe
            .managed
            .into_iter()
            .map(|managed| {
                if managed.app.is_empty() {
                    return Err(invalid("observe.managed: app is empty".into()));
                }
                let root = absolute_path(&managed.root, home)
                    .map_err(|m| invalid(format!("observe.managed: {m}")))?;
                Ok(Managed {
                    app: managed.app,
                    root,
                })
            })
            .collect::<Result<_, _>>()?;
        if observe.git.is_empty() || observe.gh.is_empty() {
            return Err(invalid(
                "observe.git and observe.gh must not be empty".into(),
            ));
        }

        if self.records.keep == 0 || self.records.max_bytes == 0 {
            return Err(invalid(
                "records.keep and records.max_bytes must be positive".into(),
            ));
        }

        Ok(HostConfig {
            budget,
            admission: Admission {
                default_resources,
                lock_timeout_ms: self.admission.lock_timeout_ms,
                max_wait_ms: self.admission.max_wait_ms,
                wait_fraction,
            },
            hook: Hook {
                enabled: self.hook.enabled,
                runner: self.hook.runner,
                deadline_ms: self.hook.deadline_ms,
            },
            observe: Observe {
                git: observe.git,
                gh: observe.gh,
                git_timeout_ms: observe.git_timeout_ms,
                gh_timeout_ms: observe.gh_timeout_ms,
                repos,
                managed,
            },
            records: Records {
                max_bytes: self.records.max_bytes,
                keep: self.records.keep,
            },
            units: self
                .unit
                .into_iter()
                .map(|(name, unit)| (name, Unit { act: unit.act }))
                .collect(),
        })
    }
}

fn check_resource_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        Err("a resource name is empty".into())
    } else {
        Ok(())
    }
}

/// Checks that every amount is a positive integer.
pub(super) fn check_resources(resources: &Resources) -> Result<(), String> {
    for (name, amount) in resources {
        check_resource_name(name)?;
        if *amount == 0 {
            return Err(format!("{name} must be a positive integer"));
        }
    }
    Ok(())
}

/// Expands `~` and requires an absolute result.
pub(super) fn absolute_path(value: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let path = expand_tilde(value, home)
        .ok_or_else(|| format!("`{value}` needs a home directory, which is not set"))?;
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!("`{value}` is not an absolute path"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{HostConfig, validate_runner};

    fn parse(text: &str) -> Result<HostConfig, super::ConfigError> {
        HostConfig::parse(Path::new("host.toml"), text, 8, Some(Path::new("/h")))
    }

    #[test]
    fn defaults_follow_the_architecture() {
        let host = parse("").unwrap();
        assert_eq!(host.budget.get("cpu"), Some(&8));
        assert_eq!(host.budget.len(), 1, "memory_mb has no default budget");
        assert_eq!(host.admission.default_resources.get("cpu"), Some(&4));
        assert_eq!(host.admission.lock_timeout_ms, 2000);
        assert_eq!(host.admission.max_wait_ms, 30_000);
        assert!((host.admission.wait_fraction - 0.25).abs() < f64::EPSILON);
        assert!(host.hook.enabled);
        assert_eq!(host.hook.runner, None);
        assert_eq!(host.hook.deadline_ms, 500);
        assert_eq!(host.observe.git, "git");
        assert_eq!(host.observe.gh, "gh");
        assert_eq!(host.observe.git_timeout_ms, 10_000);
        assert_eq!(host.observe.gh_timeout_ms, 20_000);
        assert_eq!(host.records.max_bytes, 8_388_608);
        assert_eq!(host.records.keep, 4);
        assert!(!host.unit_may_act("anything"));
    }

    #[test]
    fn single_cpu_default_resources_stay_positive() {
        let host = HostConfig::parse(Path::new("host.toml"), "", 1, None).unwrap();
        assert_eq!(host.admission.default_resources.get("cpu"), Some(&1));
    }

    #[test]
    fn explicit_values_and_extra_resources() {
        let host = parse(
            r#"
            [budget]
            cpu = 6
            memory_mb = 24000
            gpu = 1
            [admission]
            default_resources = { cpu = 2 }
            [observe]
            repos = ["~/src/other"]
            managed = [{ app = "t3code", root = "/w" }]
            [unit.disk-reclaim]
            act = true
            [unit.quiet]
            "#,
        )
        .unwrap();
        assert_eq!(host.budget.get("cpu"), Some(&6));
        assert_eq!(host.budget.get("gpu"), Some(&1));
        assert_eq!(host.admission.default_resources.get("cpu"), Some(&2));
        assert_eq!(host.observe.repos, vec![PathBuf::from("/h/src/other")]);
        assert_eq!(host.observe.managed[0].app, "t3code");
        assert!(host.unit_may_act("disk-reclaim"));
        assert!(!host.unit_may_act("quiet"));
    }

    #[test]
    fn unknown_keys_and_bad_values_are_rejected() {
        for text in [
            "[budget]\ncpu = -1",
            "[admission]\nlock_timeout = 5",
            "[hook]\nrunner = 5",
            "typo = 1",
            "[unit.x]\nkind = \"reconciler\"",
            "[admission]\nwait_fraction = 1.5",
            "[admission]\ndefault_resources = { cpu = 0 }",
            "[observe]\nrepos = [\"relative\"]",
            "[records]\nkeep = 0",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn runner_path_rule() {
        assert!(validate_runner("/usr/local/bin/cardinal").is_ok());
        assert!(validate_runner("/opt/c++/bin/cardinal-0.1_x").is_ok());
        for bad in [
            "cardinal",
            "./cardinal",
            "/path with space/cardinal",
            "/a/$HOME",
            "/a'b",
            r"C:\cardinal.exe",
        ] {
            assert!(validate_runner(bad).is_err(), "{bad}");
        }
    }
}
