use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::{ActResultSchema, Check};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActStatus {
    Executed,
    Partial,
    DryRun,
    Rejected,
    Expired,
    AlreadyExecuted,
}

impl ActStatus {
    /// Whether `act` did not execute (exit code 3).
    #[must_use]
    pub fn is_refusal(self) -> bool {
        matches!(self, Self::Rejected | Self::Expired | Self::AlreadyExecuted)
    }
}

/// `cardinal.act-result.v1`: the `act` output. `act` prints it and records it in an `act` or
/// `planned` record; it writes no result file (`docs/architecture.md` section 9.5). Phase 1
/// reports only `dry_run`, `rejected`, and `expired`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActResult {
    pub schema: ActResultSchema,
    pub plan_id: String,
    pub caller: String,
    pub status: ActStatus,
    /// Failing check names, `"owner_approval_required"`, `"identity_changed:<field>"`, and
    /// `"execution_not_designed"` in every `dry_run` of phase 1.
    pub reasons: Vec<String>,
    pub checks: Vec<Check>,
    pub freed_bytes: Option<u64>,
    /// Entries changed or created since the snapshot; empty in phase 1.
    pub skipped_changed: Vec<String>,
    pub error: Option<String>,
    pub finished_ms: u64,
}

/// Who calls `act`: `arbiter` (the default), `owner`, or `unit:<name>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Caller {
    Arbiter,
    Owner,
    Unit(String),
}

/// A `--caller` value that is not `arbiter`, `owner`, or `unit:<name>`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("caller must be `arbiter`, `owner`, or `unit:<name>`, not `{0}`")]
pub struct CallerParseError(String);

impl FromStr for Caller {
    type Err = CallerParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "arbiter" => Ok(Self::Arbiter),
            "owner" => Ok(Self::Owner),
            _ => match value.strip_prefix("unit:") {
                Some(name)
                    if !name.is_empty()
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) =>
                {
                    Ok(Self::Unit(name.to_owned()))
                }
                _ => Err(CallerParseError(value.to_owned())),
            },
        }
    }
}

impl fmt::Display for Caller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arbiter => f.write_str("arbiter"),
            Self::Owner => f.write_str("owner"),
            Self::Unit(name) => write!(f, "unit:{name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ActStatus, Caller};

    #[test]
    fn caller_round_trips() {
        for text in ["arbiter", "owner", "unit:disk-reclaim"] {
            assert_eq!(text.parse::<Caller>().unwrap().to_string(), text);
        }
        for bad in ["", "unit:", "unit:a b", "Owner", "unit:../x"] {
            assert!(bad.parse::<Caller>().is_err(), "{bad}");
        }
    }

    #[test]
    fn refusals_are_the_exit_3_statuses() {
        assert!(ActStatus::Rejected.is_refusal());
        assert!(ActStatus::Expired.is_refusal());
        assert!(ActStatus::AlreadyExecuted.is_refusal());
        assert!(!ActStatus::DryRun.is_refusal());
        assert!(!ActStatus::Partial.is_refusal());
        assert_eq!(
            serde_json::to_value(ActStatus::AlreadyExecuted).unwrap(),
            "already_executed"
        );
    }
}
