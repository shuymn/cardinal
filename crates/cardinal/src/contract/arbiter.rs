use std::fmt;

use serde::{Deserialize, Serialize};

use super::ArbiterPassSchema;

/// How an arbiter pass ended (`docs/architecture.md` section 10.5): `completed`, `timed_out`,
/// `abandoned`, or `failed:<status>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PassOutcome {
    Completed,
    TimedOut,
    Abandoned,
    /// The session exited unsuccessfully; the status as the launcher saw it.
    Failed(String),
}

impl fmt::Display for PassOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completed => f.write_str("completed"),
            Self::TimedOut => f.write_str("timed_out"),
            Self::Abandoned => f.write_str("abandoned"),
            Self::Failed(status) => write!(f, "failed:{status}"),
        }
    }
}

impl Serialize for PassOutcome {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PassOutcome {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        match text.as_str() {
            "completed" => Ok(Self::Completed),
            "timed_out" => Ok(Self::TimedOut),
            "abandoned" => Ok(Self::Abandoned),
            _ => match text.strip_prefix("failed:") {
                Some(status) if !status.is_empty() => Ok(Self::Failed(status.to_owned())),
                _ => Err(serde::de::Error::custom(format!(
                    "unknown arbiter pass outcome {text:?}"
                ))),
            },
        }
    }
}

/// `cardinal.arbiter-pass.v1` (`arbiter/last.json`, the stdout of `arbiter run`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArbiterPass {
    pub schema: ArbiterPassSchema,
    pub id: String,
    pub started_ms: u64,
    pub ended_ms: Option<u64>,
    /// Null while the pass runs.
    pub outcome: Option<PassOutcome>,
    /// The keys of the items assigned to the pass.
    pub items: Vec<String>,
    /// The path of the pass's log.
    pub log: String,
    pub failures_in_a_row: u64,
}

#[cfg(test)]
mod tests {
    use super::PassOutcome;

    #[test]
    fn outcomes_round_trip() {
        for (outcome, text) in [
            (PassOutcome::Completed, "completed"),
            (PassOutcome::TimedOut, "timed_out"),
            (PassOutcome::Abandoned, "abandoned"),
            (PassOutcome::Failed("exit 2".into()), "failed:exit 2"),
        ] {
            let json = serde_json::to_value(&outcome).unwrap();
            assert_eq!(json, text);
            assert_eq!(
                serde_json::from_value::<PassOutcome>(json).unwrap(),
                outcome
            );
        }
        for bad in ["failed:", "failed", "done", ""] {
            assert!(
                serde_json::from_value::<PassOutcome>(bad.into()).is_err(),
                "{bad}"
            );
        }
    }
}
