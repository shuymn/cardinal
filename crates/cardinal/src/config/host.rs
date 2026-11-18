//! `host.toml` (`docs/architecture.md` section 3.2).

use std::collections::BTreeMap;
use std::num::NonZeroU64;
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
    pub disk: Disk,
    pub arbiter: Arbiter,
    pub reconcile: Reconcile,
    /// `[unit.<name>]` tables, which honor only `act`.
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

/// A volume's lines in MiB of free space (section 10.4), with
/// `critical < warning < recovery`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskLines {
    /// Below: under pressure.
    pub warning_mib: u64,
    /// Pressure ends above this.
    pub recovery_mib: u64,
    /// Below: critical.
    pub critical_mib: u64,
}

impl DiskLines {
    /// The lines in bytes.
    #[must_use]
    pub fn to_bytes(self) -> crate::contract::DiskLines {
        let bytes = |mib: u64| mib.saturating_mul(1024 * 1024);
        crate::contract::DiskLines {
            warning_bytes: bytes(self.warning_mib),
            recovery_bytes: bytes(self.recovery_mib),
            critical_bytes: bytes(self.critical_mib),
        }
    }
}

/// `[disk]`: the lines of every watched volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    /// The lines of a volume without an entry in `volumes`.
    pub lines: DiskLines,
    /// Per-mount lines, matched against the mount point a path resolves to.
    pub volumes: Vec<VolumeLines>,
}

impl Disk {
    /// The lines of the volume mounted at `mount`: its `volumes` entry, else the global
    /// lines.
    #[must_use]
    pub fn lines_for(&self, mount: &Path) -> DiskLines {
        self.volumes
            .iter()
            .find(|volume| volume.mount == mount)
            .map_or(self.lines, |volume| volume.lines)
    }
}

/// One `[disk] volumes` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeLines {
    pub mount: PathBuf,
    pub lines: DiskLines,
}

/// `[arbiter]` (section 10.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arbiter {
    /// The session's argv, never a shell string.
    pub command: Vec<String>,
    /// The launcher stops the session after this.
    pub timeout_s: u64,
    /// The least time between the starts of two passes.
    pub min_interval_s: u64,
    /// An item is due again this long after its pass ended.
    pub retry_s: u64,
}

/// `[reconcile]` (section 10.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reconcile {
    /// A full observation (git, gh) runs at most this often, unless a volume is under
    /// pressure.
    pub observe_every_s: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unit {
    /// Whether the unit's runs are recorded as `act`; `false` records them as `planned`
    /// (C16). Phase 1 executes nothing either way.
    pub act: bool,
}

/// The arbiter's prompt in the default `[arbiter] command`, as in
/// `integrations/claude-code/README.md`.
const ARBITER_PROMPT: &str = "Start the cardinal-arbiter subagent for one arbiter pass. Run no command yourself, and print its final report verbatim.";

/// The allow list of the default `[arbiter] command`: the five subcommands the skill uses.
const ARBITER_TOOLS: &str = "Bash(cardinal inventory:*),Bash(cardinal inbox:*),Bash(cardinal explain:*),Bash(cardinal plan:*),Bash(cardinal act:*)";

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

    /// Whether `act` called as `unit:<name>` is recorded as `act` rather than `planned`
    /// (`docs/architecture.md` section 9.6). Units without a table, and units with
    /// `act = false`, are not.
    #[must_use]
    pub fn unit_may_act(&self, name: &str) -> bool {
        self.units.get(name).is_some_and(|unit| unit.act)
    }
}

impl Hook {
    /// The runner path written into rewrites: `[hook] runner`, else this executable. It must be
    /// absolute and consist only of `A–Z a–z 0–9 / . _ + -`, so it never needs shell quoting,
    /// and must not be a directory, which passes the shell's `-x` test (the rewrite's `-f`
    /// test would only ever take the fallback).
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
        if Path::new(&path).is_dir() {
            return Err(ConfigError::invalid(
                "host.toml",
                format!("the runner path {path} is a directory, not the cardinal binary"),
            ));
        }
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
    disk: RawDisk,
    arbiter: RawArbiter,
    reconcile: RawReconcile,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawDisk {
    warning_mib: u64,
    recovery_mib: u64,
    critical_mib: u64,
    volumes: Vec<RawVolume>,
}

impl Default for RawDisk {
    fn default() -> Self {
        Self {
            warning_mib: 20 * 1024,
            recovery_mib: 30 * 1024,
            critical_mib: 5 * 1024,
            volumes: Vec::new(),
        }
    }
}

/// A `[disk] volumes` entry: every line is required.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVolume {
    mount: String,
    warning_mib: u64,
    recovery_mib: u64,
    critical_mib: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawArbiter {
    command: Vec<String>,
    timeout_s: u64,
    min_interval_s: u64,
    retry_s: u64,
}

impl Default for RawArbiter {
    fn default() -> Self {
        Self {
            command: vec![
                "claude".into(),
                "-p".into(),
                ARBITER_PROMPT.into(),
                "--allowedTools".into(),
                ARBITER_TOOLS.into(),
            ],
            timeout_s: 1200,
            min_interval_s: 1800,
            retry_s: 7200,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawReconcile {
    observe_every_s: u64,
}

impl Default for RawReconcile {
    fn default() -> Self {
        Self {
            observe_every_s: 3600,
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

        let cpus = NonZeroU64::new(cpus).unwrap_or(NonZeroU64::MIN);
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
            None => [(
                "cpu".to_owned(),
                NonZeroU64::new(cpus.get() / 2).unwrap_or(NonZeroU64::MIN),
            )]
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

        let disk = self.disk.resolve(home).map_err(invalid)?;
        let arbiter = self.arbiter.resolve().map_err(invalid)?;

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
            disk,
            arbiter,
            reconcile: Reconcile {
                observe_every_s: self.reconcile.observe_every_s,
            },
            units: self
                .unit
                .into_iter()
                .map(|(name, unit)| (name, Unit { act: unit.act }))
                .collect(),
        })
    }
}

impl RawDisk {
    /// The lines, each set checked for `critical < warning < recovery`, and the volume
    /// mounts made absolute.
    fn resolve(self, home: Option<&Path>) -> Result<Disk, String> {
        let lines = checked_lines(
            "disk",
            DiskLines {
                warning_mib: self.warning_mib,
                recovery_mib: self.recovery_mib,
                critical_mib: self.critical_mib,
            },
        )?;
        let mut volumes: Vec<VolumeLines> = Vec::new();
        for volume in self.volumes {
            let mount =
                absolute_path(&volume.mount, home).map_err(|m| format!("disk.volumes: {m}"))?;
            if volumes.iter().any(|v| v.mount == mount) {
                return Err(format!("disk.volumes: {} is listed twice", mount.display()));
            }
            let lines = checked_lines(
                &format!("disk.volumes {}", mount.display()),
                DiskLines {
                    warning_mib: volume.warning_mib,
                    recovery_mib: volume.recovery_mib,
                    critical_mib: volume.critical_mib,
                },
            )?;
            volumes.push(VolumeLines { mount, lines });
        }
        Ok(Disk { lines, volumes })
    }
}

impl RawArbiter {
    /// The arbiter settings, with a command that names a program and a positive timeout.
    fn resolve(self) -> Result<Arbiter, String> {
        if self.command.first().is_none_or(String::is_empty) {
            return Err("arbiter.command must name a program as its first word".into());
        }
        if self.timeout_s == 0 {
            return Err("arbiter.timeout_s must be positive".into());
        }
        Ok(Arbiter {
            command: self.command,
            timeout_s: self.timeout_s,
            min_interval_s: self.min_interval_s,
            retry_s: self.retry_s,
        })
    }
}

/// `lines` if `critical < warning < recovery`.
fn checked_lines(what: &str, lines: DiskLines) -> Result<DiskLines, String> {
    if lines.critical_mib < lines.warning_mib && lines.warning_mib < lines.recovery_mib {
        Ok(lines)
    } else {
        Err(format!(
            "{what}: the lines must satisfy critical_mib < warning_mib < recovery_mib, not {} < {} < {}",
            lines.critical_mib, lines.warning_mib, lines.recovery_mib
        ))
    }
}

fn check_resource_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        Err("a resource name is empty".into())
    } else {
        Ok(())
    }
}

/// Checks resource names; amounts are positive by type.
pub(super) fn check_resources(resources: &Resources) -> Result<(), String> {
    resources
        .keys()
        .try_for_each(|name| check_resource_name(name))
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
    use std::num::NonZeroU64;
    use std::path::{Path, PathBuf};

    use super::{DiskLines, HostConfig, validate_runner};

    /// An absolute home directory on every OS.
    fn home() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\h" } else { "/h" })
    }

    fn n(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).unwrap()
    }

    fn parse(text: &str) -> Result<HostConfig, super::ConfigError> {
        HostConfig::parse(Path::new("host.toml"), text, 8, Some(&home()))
    }

    #[test]
    fn defaults_follow_the_architecture() {
        let host = parse("").unwrap();
        assert_eq!(host.budget.get("cpu"), Some(&n(8)));
        assert_eq!(host.budget.len(), 1, "memory_mb has no default budget");
        assert_eq!(host.admission.default_resources.get("cpu"), Some(&n(4)));
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
        assert_eq!(
            host.disk.lines,
            DiskLines {
                warning_mib: 20_480,
                recovery_mib: 30_720,
                critical_mib: 5120,
            }
        );
        assert_eq!(host.disk.volumes, []);
        assert_eq!(host.arbiter.command[..2], ["claude", "-p"]);
        assert_eq!(host.arbiter.command[3], "--allowedTools");
        assert_eq!(host.arbiter.timeout_s, 1200);
        assert_eq!(host.arbiter.min_interval_s, 1800);
        assert_eq!(host.arbiter.retry_s, 7200);
        assert_eq!(host.reconcile.observe_every_s, 3600);
        assert!(!host.unit_may_act("anything"));
    }

    /// The default arbiter command is the launch the integration's README documents.
    #[test]
    fn default_arbiter_command_is_the_documented_launch() {
        let readme = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../integrations/claude-code/README.md"),
        )
        .unwrap();
        let host = parse("").unwrap();
        let [_, _, prompt, _, tools] = &host.arbiter.command[..] else {
            panic!("{:?}", host.arbiter.command)
        };
        assert!(
            readme.contains(&format!("claude -p '{prompt}'")),
            "{prompt}"
        );
        assert!(
            readme.contains(&format!("--allowedTools '{tools}'")),
            "{tools}"
        );
    }

    #[test]
    fn disk_lines_per_volume() {
        let host = parse(
            r#"
            [disk]
            warning_mib = 300
            recovery_mib = 400
            critical_mib = 100
            volumes = [{ mount = "~/Scratch", warning_mib = 600, recovery_mib = 900, critical_mib = 200 }]
            [arbiter]
            command = ["/bin/arbiter"]
            timeout_s = 5
            min_interval_s = 0
            retry_s = 0
            [reconcile]
            observe_every_s = 60
            "#,
        )
        .unwrap();
        let scratch = DiskLines {
            warning_mib: 600,
            recovery_mib: 900,
            critical_mib: 200,
        };
        assert_eq!(host.disk.lines_for(&home().join("Scratch")), scratch);
        assert_eq!(host.disk.lines_for(&home()).warning_mib, 300);
        assert_eq!(scratch.to_bytes().critical_bytes, 200 * 1024 * 1024);
        assert_eq!(host.arbiter.command, ["/bin/arbiter"]);
        assert_eq!(host.reconcile.observe_every_s, 60);
    }

    /// Each invalid case fails its own check, which the message names. Mounts start at `~`,
    /// which is absolute on every OS once expanded, so only the relative one fails for that.
    #[test]
    fn disk_and_arbiter_validation() {
        let order = "critical_mib < warning_mib < recovery_mib";
        for (text, expected) in [
            // The global lines out of order, and equal lines.
            (
                "[disk]\nwarning_mib = 10\nrecovery_mib = 5\ncritical_mib = 1",
                order,
            ),
            (
                "[disk]\nwarning_mib = 10\nrecovery_mib = 20\ncritical_mib = 10",
                order,
            ),
            (
                "[disk]\nwarning_mib = 30\nrecovery_mib = 40\ncritical_mib = 35",
                order,
            ),
            // A volume's lines out of order, a volume without every line, a relative mount,
            // a mount listed twice, an unknown key, a negative line.
            (
                "[disk]\nvolumes = [{ mount = \"~/v\", warning_mib = 9, recovery_mib = 8, critical_mib = 1 }]",
                order,
            ),
            (
                "[disk]\nvolumes = [{ mount = \"~/v\", warning_mib = 9 }]",
                "missing field",
            ),
            (
                "[disk]\nvolumes = [{ mount = \"v\", warning_mib = 2, recovery_mib = 3, critical_mib = 1 }]",
                "is not an absolute path",
            ),
            (
                "[disk]\nvolumes = [{ mount = \"~/v\", warning_mib = 2, recovery_mib = 3, critical_mib = 1 }, { mount = \"~/v\", warning_mib = 2, recovery_mib = 3, critical_mib = 1 }]",
                "is listed twice",
            ),
            ("[disk]\nwarning = 1", "unknown field"),
            ("[disk]\nwarning_mib = -1", "invalid value"),
            // An empty command, an empty program, a shell string, a zero timeout, unknown
            // keys.
            ("[arbiter]\ncommand = []", "first word"),
            ("[arbiter]\ncommand = [\"\", \"-p\"]", "first word"),
            ("[arbiter]\ncommand = \"claude -p x\"", "invalid type"),
            ("[arbiter]\ntimeout_s = 0", "must be positive"),
            ("[arbiter]\ninterval = 1", "unknown field"),
            ("[reconcile]\nobserve_every = 1", "unknown field"),
        ] {
            let message = match parse(text) {
                Ok(_) => panic!("accepted: {text}"),
                Err(err) => err.to_string(),
            };
            assert!(message.contains(expected), "{text}: {message}");
        }
    }

    #[test]
    fn single_cpu_default_resources_stay_positive() {
        let host = HostConfig::parse(Path::new("host.toml"), "", 1, None).unwrap();
        assert_eq!(host.admission.default_resources.get("cpu"), Some(&n(1)));
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
            managed = [{ app = "t3code", root = "~/w" }]
            [unit.disk-reclaim]
            act = true
            [unit.quiet]
            "#,
        )
        .unwrap();
        assert_eq!(host.budget.get("cpu"), Some(&n(6)));
        assert_eq!(host.budget.get("gpu"), Some(&n(1)));
        assert_eq!(host.admission.default_resources.get("cpu"), Some(&n(2)));
        assert_eq!(host.observe.repos, vec![home().join("src").join("other")]);
        assert_eq!(host.observe.managed[0].app, "t3code");
        assert_eq!(host.observe.managed[0].root, home().join("w"));
        assert!(host.unit_may_act("disk-reclaim"));
        assert!(!host.unit_may_act("quiet"));
    }

    #[test]
    fn unknown_keys_and_bad_values_are_rejected() {
        for text in [
            "[budget]\ncpu = -1",
            "[budget]\ngpu = 0",
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
