//! `hook claude subagent-stop` and `session-end` (`docs/architecture.md` section 6.6). Both
//! record the end and never stop processes (C9); `session-end` also snapshots the agent
//! process and its descendants for the outlived-process sweep.

use std::collections::BTreeSet;
use std::time::Instant;

use crate::contract::{SessionProcess, SessionSnapshot, SessionSnapshotSchema, SnapshotAgent};
use crate::os::ProcessTable;

/// How many bytes of each command line a snapshot keeps.
const COMMAND_BYTES: usize = 256;

/// Process names that are shells: the agent is the nearest ancestor that is not one. A
/// leading `-` (login shell) and a `.exe` suffix are ignored.
pub(super) fn is_shell(name: &str) -> bool {
    let name = name.strip_prefix('-').unwrap_or(name).to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    matches!(
        name,
        "sh" | "bash" | "zsh" | "dash" | "cmd" | "powershell" | "pwsh"
    )
}

/// The snapshot for a session that ends now: the agent process (the nearest ancestor of
/// `hook_pid` that is not a shell) and every live descendant of it except the hook's own
/// chain (the hook, its descendants, and the shells between it and the agent). `None` when
/// no such ancestor exists, or when `deadline` passes during the walk.
pub(super) fn snapshot(
    table: &ProcessTable,
    hook_pid: u32,
    session_id: &str,
    reason: &str,
    ended_ms: u64,
    deadline: Instant,
) -> Option<SessionSnapshot> {
    let ancestors = table.ancestors(hook_pid);
    let position = ancestors
        .iter()
        .position(|pid| table.get(*pid).is_some_and(|p| !is_shell(&p.name)))?;
    let agent = table.get(ancestors[position])?;
    let mut chain: BTreeSet<u32> = ancestors[..position].iter().copied().collect();
    chain.insert(hook_pid);
    chain.extend(table.descendants_until(hook_pid, Some(deadline))?);
    let mut processes: Vec<SessionProcess> = table
        .descendants_until(agent.pid, Some(deadline))?
        .into_iter()
        .filter(|pid| !chain.contains(pid))
        .filter_map(|pid| table.get(pid))
        .filter(|p| !p.zombie)
        .map(|p| SessionProcess {
            pid: p.pid,
            start_s: p.start_s,
            name: p.name.clone(),
            command: p.command_line(COMMAND_BYTES),
        })
        .collect();
    processes.sort_by_key(|p| p.pid);
    Some(SessionSnapshot {
        schema: SessionSnapshotSchema,
        session_id: session_id.to_owned(),
        reason: reason.to_owned(),
        ended_ms,
        agent: SnapshotAgent {
            pid: agent.pid,
            start_s: agent.start_s,
            name: agent.name.clone(),
        },
        processes,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{is_shell, snapshot};
    use crate::os::{ProcessInfo, ProcessTable};

    fn proc(pid: u32, parent: Option<u32>, name: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent,
            start_s: 1000 + u64::from(pid),
            name: name.into(),
            cmd: vec![name.into(), "x".repeat(300)],
            cwd: None,
            zombie: false,
        }
    }

    #[test]
    fn shells() {
        for name in [
            "sh", "-zsh", "bash", "BASH.EXE", "cmd.exe", "pwsh.exe", "dash",
        ] {
            assert!(is_shell(name), "{name}");
        }
        for name in ["claude", "node", "shx", "fish-like", "cardinal"] {
            assert!(!is_shell(name), "{name}");
        }
    }

    #[test]
    fn agent_is_the_nearest_non_shell_ancestor() {
        // terminal(1) → claude(10) → { sh(11) → bash(12) → hook(13) → child(14),
        //                             dev server(20) → worker(21), zombie(22), node(30) }
        let table = ProcessTable::from_processes([
            proc(1, None, "zsh"),
            proc(10, Some(1), "claude"),
            proc(11, Some(10), "sh"),
            proc(12, Some(11), "bash"),
            proc(13, Some(12), "cardinal"),
            proc(14, Some(13), "git"),
            proc(20, Some(10), "zsh"),
            proc(21, Some(20), "vite"),
            ProcessInfo {
                zombie: true,
                ..proc(22, Some(10), "old")
            },
            proc(30, Some(10), "node"),
        ]);
        let snap = snapshot(&table, 13, "s1", "clear", 5, later()).unwrap();
        assert_eq!((snap.agent.pid, snap.agent.name.as_str()), (10, "claude"));
        assert_eq!(snap.agent.start_s, 1010);
        let pids: Vec<u32> = snap.processes.iter().map(|p| p.pid).collect();
        assert_eq!(pids, [20, 21, 30]);
        assert!(snap.processes.iter().all(|p| p.command.len() <= 256));
        assert_eq!(
            (
                snap.session_id.as_str(),
                snap.reason.as_str(),
                snap.ended_ms
            ),
            ("s1", "clear", 5)
        );
    }

    #[test]
    fn no_agent_without_a_non_shell_ancestor() {
        let table =
            ProcessTable::from_processes([proc(1, None, "sh"), proc(2, Some(1), "cardinal")]);
        assert_eq!(snapshot(&table, 2, "s", "other", 0, later()), None);
        assert_eq!(
            snapshot(&ProcessTable::default(), 2, "s", "other", 0, later()),
            None
        );
        // Past its deadline the walk stops and makes none, where it made one above.
        let table = ProcessTable::from_processes([
            proc(10, Some(1), "claude"),
            proc(13, Some(10), "cardinal"),
        ]);
        assert!(snapshot(&table, 13, "s", "other", 0, later()).is_some());
        assert_eq!(snapshot(&table, 13, "s", "other", 0, Instant::now()), None);
    }

    fn later() -> Instant {
        Instant::now() + std::time::Duration::from_secs(60)
    }
}
