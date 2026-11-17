use serde::{Deserialize, Serialize};

use super::{GroupIdentity, ReservationSchema, Resources};

/// A process identified by PID and start time (Unix seconds, as the OS reports it), so a
/// reused PID is never mistaken for the original process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_s: u64,
}

/// The stored lifecycle state of a reservation (`docs/architecture.md` section 4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationState {
    Reserved,
    Starting,
    Running,
}

/// Priority classes. Phase 0 has only `normal`; phase 2 adds the others.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    #[default]
    Normal,
}

/// How admission placed the job: inside the budgets, or over them after the wait cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionResult {
    WithinBudget,
    Overflow,
}

/// The worktree a job holds the in-use lock for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservationWorktree {
    pub key: String,
    pub path: String,
    /// `false` when the runner could not take the in-use lock (`in_use_unprotected`).
    pub protected: bool,
}

/// The agent session and tool call a job came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservationSession {
    pub id: String,
    pub agent_id: Option<String>,
    pub tool_use_id: Option<String>,
    pub stage_index: u32,
}

/// `cardinal.reservation.v1` (`reservations/<job>.json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub schema: ReservationSchema,
    pub job_id: String,
    pub state: ReservationState,
    pub boot_s: u64,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub runner: ProcessIdentity,
    pub group: GroupIdentity,
    /// Set at `running`.
    pub child: Option<ProcessIdentity>,
    /// The last SIGTERM sent by the recovery pass.
    pub stop_signal_ms: Option<u64>,
    pub priority: Priority,
    pub resources: Resources,
    pub admission: AdmissionResult,
    pub class: Option<String>,
    pub rule: Option<String>,
    pub worktree: Option<ReservationWorktree>,
    pub session: Option<ReservationSession>,
    pub cwd: String,
    /// Truncated to 16 KiB in total.
    pub argv: Vec<String>,
}

/// A reservation's derived state, computed by whoever reads it and never stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivedState {
    Live,
    RecoveryNeeded,
    Stale,
}

/// A member of a reservation's process group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMember {
    pub pid: u32,
    pub start_s: u64,
    pub name: String,
}

/// A reservation as shown by `inventory` and inbox evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservationView {
    #[serde(flatten)]
    pub reservation: Reservation,
    pub derived: DerivedState,
    pub members: Vec<GroupMember>,
    pub verified: bool,
}

#[cfg(test)]
mod tests {
    use super::{
        AdmissionResult, DerivedState, Priority, ProcessIdentity, Reservation, ReservationSchema,
        ReservationState, ReservationView,
    };
    use crate::contract::GroupIdentity;

    fn reservation() -> Reservation {
        Reservation {
            schema: ReservationSchema,
            job_id: "0123456789abcdef".into(),
            state: ReservationState::Running,
            boot_s: 1,
            created_ms: 2,
            updated_ms: 3,
            runner: ProcessIdentity {
                pid: 10,
                start_s: 4,
            },
            group: GroupIdentity::Pgid {
                pgid: 10,
                leader_start_s: 4,
            },
            child: Some(ProcessIdentity {
                pid: 11,
                start_s: 5,
            }),
            stop_signal_ms: None,
            priority: Priority::Normal,
            resources: [("cpu".to_owned(), std::num::NonZeroU64::new(2).unwrap())]
                .into_iter()
                .collect(),
            admission: AdmissionResult::WithinBudget,
            class: None,
            rule: None,
            worktree: None,
            session: None,
            cwd: "/w".into(),
            argv: vec!["cargo".into(), "test".into()],
        }
    }

    #[test]
    fn view_flattens_the_reservation() {
        let view = ReservationView {
            reservation: reservation(),
            derived: DerivedState::RecoveryNeeded,
            members: vec![],
            verified: true,
        };
        let value = serde_json::to_value(&view).unwrap();
        assert_eq!(value["schema"], "cardinal.reservation.v1");
        assert_eq!(value["state"], "running");
        assert_eq!(value["derived"], "recovery_needed");
        assert_eq!(value["admission"], "within_budget");
        assert_eq!(value["stop_signal_ms"], serde_json::Value::Null);
        let back: ReservationView = serde_json::from_value(value).unwrap();
        assert_eq!(back, view);
    }
}
