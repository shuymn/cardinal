//! A reservation's derived state (`docs/architecture.md` section 4.5), computed by whoever
//! reads it and never stored.

use std::cell::OnceCell;

use crate::contract::{DerivedState, GroupIdentity, GroupMember, Reservation};
use crate::os;
use crate::state::Probe;

/// How far apart two boot times may be and still name the same boot. Windows derives the boot
/// time from the uptime, which can move by a second between reads.
pub const BOOT_TOLERANCE_S: u64 = 2;

/// The process facts liveness depends on. [`HostFacts`] reads the real system; tests give a
/// fake process table for cases a real system cannot produce on demand.
pub trait ProcessFacts {
    /// The current boot time; `None` when it cannot be read (then no reservation is released
    /// for a boot change).
    fn boot_s(&self) -> Option<u64>;
    /// Whether process group `pgid` has any member: `kill(-pgid, 0)` does not fail with
    /// `ESRCH`.
    fn group_exists(&self, pgid: u32) -> bool;
    /// The start time of `pid` if it is alive (not a zombie).
    fn start_s(&self, pid: u32) -> Option<u64>;
    /// The process group of `pid` if it is alive.
    fn pgid(&self, pid: u32) -> Option<u32>;
    /// The live members of group `pgid`.
    fn members(&self, pgid: u32) -> Vec<GroupMember>;
}

/// The derived state of one reservation and, for `recovery_needed`, whether its group is
/// verified: the recorded child is alive and still in the recorded group. Verification is
/// reported for inspection; recovery never signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Derived {
    pub state: DerivedState,
    pub verified: bool,
}

/// Derives the state of `reservation`, evaluating in order:
///
/// 1. its live lock is held by the owner: `live`;
/// 2. its boot time differs from the current one by more than 2 s: `stale`;
/// 3. its group is empty, or a live process whose PID equals the PGID has a start time other
///    than the runner's (POSIX does not reuse a PID while a group with that ID exists, so the
///    job's group emptied first): `stale`;
/// 4. otherwise `recovery_needed`.
///
/// A live lock that cannot be probed counts as held, so an unreadable state never releases a
/// reservation.
#[must_use]
pub fn derive(
    reservation: &Reservation,
    live_lock: Option<Probe>,
    facts: &dyn ProcessFacts,
) -> Derived {
    let stale = Derived {
        state: DerivedState::Stale,
        verified: false,
    };
    if matches!(live_lock, Some(Probe::Held) | None) {
        return Derived {
            state: DerivedState::Live,
            verified: false,
        };
    }
    if facts
        .boot_s()
        .is_some_and(|boot| boot.abs_diff(reservation.boot_s) > BOOT_TOLERANCE_S)
    {
        return stale;
    }
    let GroupIdentity::Pgid {
        pgid,
        leader_start_s,
    } = reservation.group;
    if !facts.group_exists(pgid) {
        return stale;
    }
    if facts
        .start_s(pgid)
        .is_some_and(|start| start != leader_start_s)
    {
        return stale;
    }
    let verified = reservation.child.is_some_and(|child| {
        facts.start_s(child.pid) == Some(child.start_s) && facts.pgid(child.pid) == Some(pgid)
    });
    Derived {
        state: DerivedState::RecoveryNeeded,
        verified,
    }
}

/// The real system. The live processes and their groups are read once, on the first call
/// that lists group members.
#[derive(Default)]
pub struct HostFacts {
    boot: OnceCell<Option<u64>>,
    grouped: OnceCell<Vec<(u32, GroupMember)>>,
}

impl HostFacts {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every live process with its group.
    #[cfg(unix)]
    fn grouped(&self) -> &[(u32, GroupMember)] {
        self.grouped.get_or_init(|| {
            let Ok(table) = os::ProcessTable::snapshot(os::Detail::default()) else {
                return Vec::new();
            };
            table
                .iter()
                .filter(|p| !p.zombie)
                .filter_map(|p| {
                    let pgid = os::unix::pgid_of(p.pid)?;
                    Some((
                        pgid,
                        GroupMember {
                            pid: p.pid,
                            start_s: p.start_s,
                            name: p.name.clone(),
                        },
                    ))
                })
                .collect()
        })
    }
}

impl ProcessFacts for HostFacts {
    fn boot_s(&self) -> Option<u64> {
        *self.boot.get_or_init(|| os::boot_time_s().ok())
    }

    #[cfg(unix)]
    fn group_exists(&self, pgid: u32) -> bool {
        os::unix::group_exists(pgid)
    }

    /// Phase 0 creates no reservation on other platforms; one found there counts as having a
    /// live group, so it is never released early.
    #[cfg(not(unix))]
    fn group_exists(&self, _pgid: u32) -> bool {
        true
    }

    fn start_s(&self, pid: u32) -> Option<u64> {
        os::start_time_s(pid)
    }

    #[cfg(unix)]
    fn pgid(&self, pid: u32) -> Option<u32> {
        os::start_time_s(pid)?;
        os::unix::pgid_of(pid)
    }

    #[cfg(not(unix))]
    fn pgid(&self, _pid: u32) -> Option<u32> {
        None
    }

    #[cfg(unix)]
    fn members(&self, pgid: u32) -> Vec<GroupMember> {
        self.grouped()
            .iter()
            .filter(|(group, _)| *group == pgid)
            .map(|(_, member)| member.clone())
            .collect()
    }

    #[cfg(not(unix))]
    fn members(&self, _pgid: u32) -> Vec<GroupMember> {
        let _ = &self.grouped;
        Vec::new()
    }
}
