# cardinal phase 0 architecture

This document fixes the contracts that the phase 0 pieces build against: the crate and module layout, configuration, the state directory, the runner, the Claude Code hook, classification, observation, `plan` and `act`, the CLI and its JSON, libraries, tests, and the work split. [design.md](design.md) is the source for principles, non-goals, invariants, purpose-layer conditions G1–G14, and plan-layer claims C1–C21; this document refers to its sections as "design §N" and does not restate them. Where the design turned out to be wrong or incomplete, [section 17](#17-issues-with-the-design) says so and gives the resolution used here.

## Contents

1. [Phase 0 scope](#1-phase-0-scope)
2. [Crate and module layout](#2-crate-and-module-layout)
3. [Configuration](#3-configuration)
4. [State directory](#4-state-directory)
5. [Runner](#5-runner)
6. [Claude Code hook contract](#6-claude-code-hook-contract)
7. [Classification](#7-classification)
8. [Observation](#8-observation)
9. [plan, act, and invariants](#9-plan-act-and-invariants)
10. [Inbox and explain](#10-inbox-and-explain)
11. [CLI and JSON contracts](#11-cli-and-json-contracts)
12. [Libraries](#12-libraries)
13. [Test strategy](#13-test-strategy)
14. [Work split](#14-work-split)
15. [Purpose layer for phase 0](#15-purpose-layer-for-phase-0)
16. [Plan layer for phase 0](#16-plan-layer-for-phase-0)
17. [Issues with the design](#17-issues-with-the-design)

## 1. Phase 0 scope

Phase 0 builds the core (hook adapter, runner, admission and reservations, observation, records, `plan`/`act`/invariants, the inbox, the CLI for the arbiter), the Claude Code adapter, and the first arbiter skill. It is accepted by G4, G5, G10, and G14 (design §12).

**Phase 0 admission decides with exactly this:** the resources of every reservation that is live or recovery-needed, plus the new job's resources, compared per resource against the budgets in `host.toml`; a job gets all of its resources or none. There are no sensors, no priority classes, no queue order, and no measured weights. Resource amounts come from the matching classification rule. This is the design's "sensor read failed, decide from reservations alone" case (design §7), made permanent until phase 2.

Phase 0 leaves seams for phase 2 and later but implements none of them:

| Seam | Phase 0 shape | Filled by |
| --- | --- | --- |
| Sensors | `sensor::Sensors` trait; the only implementation returns `Unavailable` | Phase 2 (memory), phase 1 (disk) |
| Priority classes | `Priority` enum with one variant, `Normal`; stored in reservations | Phase 2 |
| Accepted jobs, `wait`, `cancel`, `logs` | Command names and `jobs/` path reserved; lock handoff fixed in [section 5.7](#57-executor-contract-phase-2-seam); not registered in the CLI | Phase 2 |
| Queue tickets | `tickets/` path reserved | Phase 2 |
| Job spec, placement, executor | `exec::Invocation` (local form), `exec::Placement::Local`, and the `Executor` contract of section 5.7 | Phase 2 |
| Measurements | Records carry `measurements: [{kind, …}]`; phase 0 writes only `wall_time` | Phase 2 (C21) |
| Parallelism injection | Not implemented; classification rules reject an `env` key | Phase 2 |
| Classifier and reconciler units | Built-in classifier implements the unit-shaped `Classifier` trait; JSON schemas fixed in [section 7.6](#76-classifier-unit-seam); `act` already honors per-unit `act = false` ([section 9.6](#96-dry-run-and-unit-gating)) | Phase 1+ |
| Disk reserve file | Path reserved ([section 4.12](#412-disk-reserve-file)) | Phase 1 (C11) |
| OS timer, reconcile runner, starting the arbiter | None; `cardinal inbox` runs the recovery pass (section 10.1) | Phase 1 |

## 2. Crate and module layout

### 2.1 One crate

Everything lives in the single crate `crates/cardinal` (library plus the `cardinal` binary). The library target exists so integration tests can call modules directly; it is not a published API.

| Option | Verdict |
| --- | --- |
| One crate with modules, layering checked by a test | **Chosen.** One compile unit, one binary, no version coordination. The dependency direction below is enforced by `tests/layering.rs`, which fails when a module imports one it must not |
| Workspace of `cardinal-core`, `cardinal-os`, `cardinal-claude`, `cardinal-cli` | Rejected for now. The compiler would enforce layering, but every contract change would touch several manifests, and no second consumer of the core exists yet. Split when one appears (a unit SDK, a control-plane connector) |

### 2.2 Modules

| Module | Owns | May use |
| --- | --- | --- |
| `contract` | serde types for every JSON document in [section 11](#11-cli-and-json-contracts) and the record, reservation, and run-spec files | std, serde |
| `config` | `host.toml` and `repos/*.toml`: locations, parsing, defaults | `contract` |
| `state` | State paths, atomic writes, lock helpers with deadlines, IDs and keys, JSONL append and read | `contract` |
| `record` | Record appending, rotation, reading | `state`, `contract` |
| `testpoint` | Pause points and failure injection for tests ([section 13.1](#131-deterministic-concurrency)) | std |
| `os` | Platform backends behind one API: process table, start times, parent PID, process working directories, boot time (all OSes); process-group setup and signals (Unix); Job Objects (Windows) | std, rustix, signal-hook, windows-sys, sysinfo |
| `sensor` | Phase 2 seam: `Sensors` trait, `Unavailable` implementation | `contract` |
| `exec` | `Invocation` (argv; cwd and env by inheritance), `Placement::Local`, `Executor` trait, local executor | `os`, `contract` |
| `admission` | Budget math (pure), reservation store, liveness and the recovery pass, worktree in-use query | `state`, `config`, `os`, `sensor`, `record`, `contract` |
| `run` | The runner: start boundary, fail-open, wait loop, child supervision, release | `admission`, `exec`, `record`, `state`, `config`, `os`, `testpoint` |
| `classify` | Shell tokenizer, stage finder, normalizer, rule matcher, `Classifier` trait, built-in classifier | `config`, `contract` |
| `adapter::claude` | Hook input and output, stage rewriting, session-end snapshot | `classify`, `config`, `record`, `state`, `os` |
| `observe` | git and gh invocation with constructed environments and deadlines, repository and worktree facts, merge evidence, ignored-data classification, disk usage, outlived processes | `config`, `state`, `admission` (read-only queries), `os`, `contract` |
| `plan` | Plan generation, identity binding | `observe`, `admission`, `state`, `record`, `contract` |
| `act` | Plan execution: move to trash, inspection, deletion | `plan`, `observe`, `admission`, `state`, `record`, `contract`, `testpoint` |
| `inbox` | Inbox store, dedup, refresh (recovery pass and built-in raisers) | `admission`, `observe`, `state`, `record`, `contract` |
| `explain` | Explanation documents | `classify`, `record`, `plan`, `inbox`, `state`, `contract` |
| `cli` | clap definitions and thin handlers that print JSON | everything |

```mermaid
flowchart TD
  cli --> adapter_claude[adapter::claude]
  cli --> run
  cli --> plan
  cli --> act
  cli --> inbox
  cli --> explain
  cli --> observe
  adapter_claude --> classify
  run --> admission
  run --> exec
  act --> plan
  plan --> observe
  inbox --> observe
  observe --> admission
  admission --> os
  exec --> os
  admission --> state
  observe --> state
  classify --> config
  state --> contract
```

Two rules matter most and are part of the layering test:

- `admission`, `run`, and `exec` never import `observe`, `inbox`, `plan`, or `act`. gh lives only in `observe`, and nothing on the admission path raises inbox items (C17, C18). Section 13.5 checks the same at run time.
- `contract`, `state`, `config`, `testpoint`, and `os` import nothing above them, so every work unit can depend on them from day one.

## 3. Configuration

### 3.1 Locations

| | Unix (macOS, Linux) | Windows |
| --- | --- | --- |
| Config dir | `$CARDINAL_CONFIG_DIR`, else `$XDG_CONFIG_HOME/cardinal`, else `$HOME/.config/cardinal` | `$CARDINAL_CONFIG_DIR`, else `%LOCALAPPDATA%\cardinal\config` |
| State dir | `$CARDINAL_STATE_DIR`, else `$XDG_STATE_HOME/cardinal`, else `$HOME/.local/state/cardinal` | `$CARDINAL_STATE_DIR`, else `%LOCALAPPDATA%\cardinal\state` |

macOS uses the XDG locations too, as the design says (design §9) and as most developer CLIs do. Windows uses `%LOCALAPPDATA%` (not roaming) for both because the configuration describes one host. The `CARDINAL_*` overrides exist for tests and for running two independent instances. A relative or missing base directory is an error; before a stage starts, every error fails open.

Files:

```text
<config>/
  host.toml          # one per host; optional
  repos/*.toml       # per-repository profiles; read in file-name order
```

Unknown keys are rejected (`deny_unknown_fields`) so typos surface as `config_invalid` instead of silently changing behavior. A config error never blocks a command: the hook does not rewrite, and the runner fails open.

### 3.2 `host.toml`

```toml
[budget]                 # admission budgets; a resource without a budget is unlimited
cpu = 8                  # default: logical CPUs (std::thread::available_parallelism)
memory_mb = 24000        # default: unset
# any other name is a resource too, e.g. gpu = 1

[admission]
default_resources = { cpu = 4 }  # for rules without `resources`; default: cpu = max(1, logical CPUs / 2)
lock_timeout_ms = 2000           # admission lock deadline
max_wait_ms = 30000              # upper bound of the synchronous wait
wait_fraction = 0.25             # share of the Bash timeout the wait may use

[hook]
enabled = true
runner = "/usr/local/bin/cardinal"  # absolute path written into rewrites; default: the hook's own executable
deadline_ms = 500                   # internal classification deadline

[observe]
git = "git"
gh = "gh"
git_timeout_ms = 10000
gh_timeout_ms = 20000
repos = ["~/src/other"]             # extra main worktrees to inventory (profiles add theirs)
managed = [{ app = "t3code", root = "/path/to/t3code/worktrees" }]  # report-only worktrees

[records]
max_bytes = 8388608
keep = 4

# [unit.<name>] tables (design §5) are parsed from phase 1. In phase 0 the only
# honored key is `act` for callers that identify as a unit (section 9.6).
```

`[hook] runner` must be absolute and consist only of `A–Z a–z 0–9 / . _ + -` (plus a drive prefix such as `C:` on Windows); otherwise the hook does not rewrite and records `config_error`. The path is the only configuration value that appears in shell text, and this rule means it never needs quoting.

### 3.3 `repos/*.toml` profiles

```toml
[match]                                  # omit [match] for a generic profile that applies to every repository
paths = ["~/src/app"]                    # main worktree paths; matched through the git common dir
remotes = ["github.com/owner/app"]       # origin URL, normalized to host/owner/name

[git]
base = "origin/main"                     # default: the target of refs/remotes/origin/HEAD

[classify]
wrappers = [["npx"], ["pnpm", "exec"], ["pnpm", "dlx"], ["npm", "exec"], ["bunx"], ["mise", "exec", "--"], ["mise", "x", "--"]]
skip = []                                # command words never rewritten in matching repositories

[[rule]]
id = "test"
commands = ["pnpm test", "pnpm -r test", "vitest", "cargo test", "cargo nextest run"]
except = ["--watch", "-w"]               # any of these words disqualifies a match
class = "test"                           # label for records (phase 2 measurement key)
resources = { cpu = 4, memory_mb = 9000 }

[data]
regenerable = ["node_modules", "target", "fuzz/target", ".DS_Store"]
protected = ["fuzz/corpus", "fuzz/artifacts", "proptest-regressions"]
```

- **Repository identity** is the canonical git common dir. A profile matches when one of `match.paths` has that common dir, or when `git remote get-url origin` normalizes to one of `match.remotes`. Path matching needs no child process and is tried first; remote matching runs git (section 8.1) and only when a rule could match a stage at all.
- **Precedence:** matching profiles in file-name order, then generic profiles in file-name order. The first rule whose pattern matches wins. `[data]` lists and `[classify] skip` are the union of the matching and generic profiles. `[git] base` and `[classify] wrappers` come from the first profile that sets them.
- **Data patterns:** an entry without `/` matches a file or directory with that name at any depth; an entry with `/` matches that worktree-relative path. No globs in phase 0.
- **No tool knowledge in code.** The core ships no default rules, wrappers, or data patterns (design §4). The repository ships `examples/config/` with a starting `host.toml` and a generic profile; the owner copies what they want.

## 4. State directory

### 4.1 Layout

```text
<state>/
  VERSION                     # layout version, "1"
  host-id                     # 16 hex chars, created once
  admission.lock              # empty; exclusive lock = the admission critical section
  live/<job>.lock             # empty; held exclusively by the owning runner for its lifetime
  reservations/<job>.json     # one per reserved job
  worktrees/<key>.lock        # empty; in-use lock (shared = in use, exclusive = being reclaimed)
  sessions/<session>.json     # SessionEnd snapshots awaiting the outlived-process sweep
  inbox/.lock                 # empty; serializes inbox upserts
  inbox/<key-hash>.json       # one per inbox item
  plans/<plan>/plan.json      # immutable after creation
  plans/<plan>/claim          # created once by the act that executes the plan; progress appended
  plans/<plan>/result.json    # act result
  records/records.jsonl       # current record file
  records/records.<n>.jsonl   # rotated files, n = 1..keep-1
  records/.rotate.lock        # empty; serializes rotation
  tmp/                        # temporary files for atomic writes (same file system)
  jobs/                       # reserved for phase 2 accepted jobs
  tickets/                    # reserved for phase 2 queue tickets
  reserve                     # reserved for the phase 1 disk reserve file
```

| File | Format | Written by | Read by |
| --- | --- | --- | --- |
| `VERSION`, `host-id` | text | first cardinal process that finds them missing: a fully written temp file is hard-linked into place, which fails if another process won, and the loser reads the winner's file | all |
| `reservations/<job>.json` | `cardinal.reservation.v1` | owning runner; the recovery pass deletes others' reservations, only under the admission lock | admission, observe, inbox, plan, act |
| `sessions/<session>.json` | `cardinal.session-snapshot.v1` | `hook claude session-end` | inbox refresh |
| `inbox/<key-hash>.json` | `cardinal.inbox-item.v1` | inbox raise, refresh, resolve | `inbox`, `explain` |
| `plans/<plan>/plan.json` | `cardinal.plan.v1` | `plan` | `act`, `explain` |
| `plans/<plan>/claim` | JSON lines: `{"pid","claimed_ms","trash"}`, then `{"step":"renamed"\|"inspected"\|"restored"}` | `act` (create-new, then append) | `act`, `plan`, `explain` |
| `plans/<plan>/result.json` | `cardinal.act-result.v1` | `act` | `act`, `explain` |
| `records/*.jsonl` | one `cardinal.record.v1` object per line | every command | `explain`, `inventory` |

### 4.2 Write discipline

- **Atomic replace:** write to `tmp/<name>.<pid>.<n>`, flush, rename over the target. Readers see the old or the new file, never a mix. Leftover temp files older than one hour are removed by `plan` and `inbox` runs.
- **fsync policy:** reservations, live locks, and records are not fsynced; they are meaningless after a power loss (reservations from an earlier boot are released, section 4.5). Plan claims, act results, and inbox items are fsynced (file, then parent directory on Unix), because losing them could run a plan twice, lose track of a target moved to trash, or bring back a resolved item. On Windows, `FlushFileBuffers` on the file only.
- **Exclusive creation for uniqueness:** a job ID is claimed by creating `live/<job>.lock` with `create_new`, a plan ID by creating `plans/<plan>/` with `create_dir`; a collision retries with a new ID.
- **IDs:** 16 lowercase hex characters from a randomly keyed SipHash (`std::hash::RandomState`) of time, PID, and a counter. No RNG crate. The CLI accepts any unique prefix of at least 4 characters.
- **Keys:** worktree keys and inbox key hashes are FNV-1a 64 of the canonical path or key, as 16 hex characters (hand-written, stable across Rust releases unlike `DefaultHasher`). A worktree-key collision only makes two worktrees share an in-use lock (more conservative); inbox files store the full key and probe `-1`, `-2`, … on collision.
- **Times** are integers in milliseconds since the Unix epoch (`*_ms`). Process start times are seconds, as the OS reports them (`*_s`).

### 4.3 Locks

Locks use `std::fs::File::{lock, lock_shared, try_lock, try_lock_shared}` (stable since Rust 1.89): `flock` on Linux and macOS, `LockFileEx` on Windows. The OS drops them when the holder dies, which is what makes liveness detection work without TTLs.

- Lock files are always empty and separate from data files, because Windows locks are mandatory: a locked byte range cannot be read by other processes.
- std opens files close-on-exec and non-inheritable, so a child never inherits the runner's locks. A surviving child therefore cannot keep a dead runner looking alive.
- Waiting with a deadline is a `try_lock` loop with backoff (5 ms doubling to 50 ms). Every acquisition has a deadline; there is no unbounded `lock()`.
- Lock order is always: worktree in-use lock, then admission lock, then inbox or rotate lock. Nothing takes them in another order.
- `admission.lock`, `worktrees/*.lock`, `inbox/.lock`, and `records/.rotate.lock` are never deleted, so no process can lock a file another process has already unlinked. `worktrees/*.lock` grows by one empty file per worktree ever used; phase 1 may prune them under the admission lock.

### 4.4 Admission protocol

The critical section only reads small files and writes one; process probes are a few system calls.

1. Take `admission.lock` exclusively (deadline `lock_timeout_ms`; on timeout, fail open).
2. On the runner's first pass only: create `live/<job>.lock` with `create_new` and take it exclusively. The runner holds it until it exits.
3. Run the **recovery pass**: read every `reservations/*.json` (a file that vanished meanwhile is skipped) and evaluate each one (section 4.5). Delete `stale` ones and their live lock files, and delete live lock files that are free and have no reservation. Send SIGTERM to each verified `recovery_needed` group on Unix, and SIGKILL when a later pass finds it still alive at least 2 s after the SIGTERM recorded in the reservation.
4. Sum the resources of `live` and `recovery_needed` reservations. If, for every resource the new job needs, `sum + need ≤ budget` (resources without a budget always fit), write `reservations/<job>.json` (atomic) with `state: "reserved"` and the runner's group identity (section 5.5).
5. Release `admission.lock`.

The liveness lock is held before the reservation becomes visible, so another admitter can never see the new reservation with a free liveness lock. Live lock files are created and garbage-collected only inside the admission lock, so collection can never delete a file between its creation and its locking. A missing live lock file counts as free. `cardinal inbox` runs the same recovery pass (section 10.1).

### 4.5 Reservation lifecycle and recovery-needed

```text
reserved ──(written immediately before spawn)──> starting ──(after spawn)──> running ──> deleted at release
```

The group identity is fixed before the reservation exists: on Unix the runner makes itself a process-group leader, so the PGID is the runner's PID; on Windows the runner creates and joins a named Job Object (section 5.5). A reservation's derived state is computed by whoever reads it; it is never stored:

| Condition (evaluated in order) | Derived state |
| --- | --- |
| `try_lock` on `live/<job>.lock` fails (the owner holds it) | `live` |
| `boot_s` differs from the current boot time by more than 2 s | `stale`: no process survives a reboot |
| The group is empty. Unix: `kill(-pgid, 0)` fails with `ESRCH`, or a process whose PID equals the PGID has a start time other than the runner's (POSIX does not reuse a PID while a process group with that ID exists, so the job's group must have emptied first). Windows: the named Job Object cannot be opened or reports no active processes | `stale` |
| Otherwise | `recovery_needed` |

A `recovery_needed` group is **verified** when the recorded child (PID and start time) is alive and still has the recorded PGID. The recovery pass signals only verified groups. An unverified group (the runner died at `starting`, the child exited while descendants remain, or the PGID was reused by an unrelated group) is never signalled: the reservation keeps counting until the group is empty or the boot changes, and an inbox item shows its members. PGID reuse by an unrelated group therefore only delays release, and the delay clears itself when that group ends. Nobody, including the owner, can release a non-empty group's reservation (G14).

Recovery-needed reservations count as reserved for admission and as in use for their worktree. On Windows the runner's death closes the Job Object, whose kill-on-close stops the group, so recovery normally becomes `stale` at the next pass.

### 4.6 Worktree in-use lock

- The worktree of a job is found by walking up from the runner's cwd to the nearest `.git` entry (file or directory); the key is the FNV-1a of that directory's canonical path. A job outside any worktree takes no in-use lock.
- The runner takes `worktrees/<key>.lock` **shared** at acceptance, before admission and before any wait, and holds it until the child has exited and the reservation is deleted (C15). Several jobs can share a worktree.
- **Fail-open never skips the in-use lock.** If the lock is held exclusively past the wait cap (an `act` is reclaiming the worktree), the runner exits 125 without running the stage (section 5.6). After acquiring it, the runner checks that the worktree root still exists with the identity it had before the wait; if not, it also exits 125 without running. Only when the lock file cannot be opened at all (the state dir is broken) does the runner fail open without it: `act` needs the same file, so no reclamation can run in that state.
- `act` takes the lock **exclusively** with `try_lock` (no waiting) and keeps it until the target has been moved to trash and inspected (section 9.5). A worktree is in use when that `try_lock` fails, or when a `live` or `recovery_needed` reservation names the worktree key (checked under the admission lock while holding the exclusive lock). The second check covers a crashed runner, whose shared lock the OS has already dropped.

### 4.7 Records

- One JSON object per line, appended with a single `write` on a file opened in append mode (`O_APPEND`; `FILE_APPEND_DATA` on Windows), so concurrent appenders do not interleave.
- When `records.jsonl` exceeds `max_bytes`, the appender that notices takes `.rotate.lock` with `try_lock` (skips rotation if busy), renames `records.<keep-2>` → `records.<keep-1>` … `records.jsonl` → `records.1.jsonl`. An appender that still holds the old file writes into the rotated file; nothing is lost.
- Readers skip lines that do not parse. A crash mid-write costs at most the torn line and the line appended after it.
- A record failure is never surfaced to a command and never causes a rerun (C5, C11).

### 4.8 Inbox store

`inbox/<key-hash>.json` holds one item (section 10.1). Upserts take `inbox/.lock` exclusively: an existing item with the same key is updated (`seen` incremented, `evidence` replaced) and is never reopened once `resolved` or `dismissed`.

### 4.9 Plans store

`plans/<plan>/plan.json` is written once. `act` claims a plan by creating `plans/<plan>/claim` with `create_new`, including the trash path it will use, and fsyncs it before touching the target, so a plan runs at most once even across a crash or power loss. Each step (`renamed`, `inspected`, `restored`) is appended and fsynced. A claim without `result.json` means the run was interrupted; the next `plan` or `act` run finishes it: an entry still at the trash path is deleted if the claim records `inspected`, otherwise moved back to its original path. If that path is occupied, the entry stays in trash and an `interrupted_act` inbox item reports it. The plan itself is never retried. Plan directories older than 30 days are removed by `plan` runs.

### 4.10 Sessions

`hook claude session-end` writes `sessions/<session>.json` with the agent process and its descendants (section 6.6). Inbox refresh deletes it once none of the listed processes is alive.

### 4.11 Crash consistency

| Runner dies … | Left behind | Next reader does |
| --- | --- | --- |
| before its first admission pass | Nothing; its in-use shared lock is dropped by the OS | — |
| while waiting between passes | `live/<job>.lock`, free | The recovery pass deletes it |
| inside the admission lock | Temp file in `tmp/`; lock dropped by the OS | Temp GC |
| at `reserved` | Reservation; its group was the runner alone and is now empty | `stale` |
| at `starting` | Reservation; a child may or may not exist in the group | Empty group: `stale`. Otherwise `recovery_needed`, unverified, until the group is empty |
| at `running` | Reservation with the child's identity | Windows: job killed, `stale`. Unix: `recovery_needed`, verified; stopped, then `stale` |
| after the child exits, before release | Reservation; group empty | `stale`; records lack `job_finished` |
| during release | Nothing or a free live lock | Recovery pass |

`act` dying leaves its target in place or at the recorded trash path (section 4.9). Power loss: reservations, live locks, and in-use locks are reset by the reboot (`boot_s` check, OS locks). Plans and the inbox survive through fsync.

### 4.12 Disk reserve file

Phase 1 creates `<state>/reserve` with really allocated blocks (written zeros, not `set_len`, which leaves a sparse file), 64 MiB by default, and deletes it when the state volume is full so minimal reclamation can proceed (C11). Phase 0 only reserves the path.

## 5. Runner

### 5.1 Invocation

```text
cardinal run [--spec-b64 SPEC] -- COMMAND [ARG]...
```

- **The stage** arrives as argv after `--`: the stage's own words after the calling shell has expanded them (variables, including ones that are not exported, globs, `~`, quoting), with the stage's assignment prefixes and redirections already applied to the runner process by that shell (section 6.4). The runner executes the argv directly, without a shell, so expansion, cwd, environment, and redirections are exactly those of the calling shell.
- **`--spec-b64`** is unpadded base64url of a `cardinal.run-spec.v1` JSON document (section 11.3): class, rule, resources, Bash timeout, background flag, session, agent, and tool-use IDs, stage index, and the stage's source text. Everything the runner needs from the hook travels here, so no configuration value is ever spliced into shell text, and base64url contains nothing that any shell or Git Bash's path conversion treats specially. Without a spec (people, tests), `admission.default_resources` applies.

### 5.2 Steps

1. If stdin is a terminal: **TTY passthrough.** Append `tty_passthrough` (best effort) and run the stage as the fail-open path of step 9 does: no process group, no admission, no in-use lock.
2. Decode the spec, resolve the state dir, load `host.toml`.
3. Take the worktree in-use lock shared (section 4.6).
4. Set up the group (section 5.5).
5. Admission (section 4.4). If the budget is full, release the admission lock, sleep (250 ms doubling to 2 s), and retry until the wait cap (section 5.4).
6. Write `starting` (the start boundary, section 5.3), spawn the stage, write `running` with the child's PID and start time, append `job_started`.
7. Wait for the child, forwarding signals and watching the parent (section 5.5).
8. Release: delete the reservation, drop the in-use lock, append `job_finished` (exit status, `measurements: [{kind: "wall_time", ms}]`), drop and delete the live lock, print the wait note if there was a wait, exit with the child's status (section 5.6).
9. **Fail open** (section 5.3): append `job_fail_open` if records still work, release whatever was taken, restore the original process group (Unix) or clear kill-on-close (Windows), and run the stage without a reservation: `exec` on Unix, so the runner process becomes the stage; spawn and wait on Windows. Nothing is printed. If the stage itself cannot be executed, print `<command>: <OS error>` and exit 127 (not found) or 126, as shells do.

### 5.3 Start boundary

- **The stage has started once `spawn()` has returned a child.** Before that, the runner itself runs the stage on every failure it can observe: an error in steps 2–6 other than a busy in-use lock, a failed `spawn()` (the OS guarantees that no child ran), the admission-lock deadline, and a panic, which a top-level `catch_unwind` turns into fail-open. Every wait before the spawn has a deadline: the admission lock (`lock_timeout_ms`, then fail open), the budget (the wait cap, then the overflow start of section 5.4), and the in-use lock (the wait cap, then exit 125 without running, section 4.6).
- **After `spawn()` has returned, the runner never runs the stage again.** A later error or panic ends in waiting for the child and returning its status when possible, and otherwise in exit 125.
- **The `starting` record**, written immediately before `spawn()`, is the boundary as seen from the state dir: a reservation at `starting` whose runner died may or may not have a child, and recovery treats it as possibly running.
- **Zero-run cases that remain:** the runner killed (for example by SIGKILL) between its start and the spawn, and a runner path that is executable but cannot be loaded (`[ -x R ]` passes, the exec fails with a loader error). Both end the Bash call with a visible non-zero status (137, or 126 with the shell's message), nothing runs twice, and nothing reports success. A shell-side fallback on the runner's exit status could not tell these cases from the stage's own failure and would run the stage twice.

### 5.4 Wait cap and phase 0 overflow

- Wait cap: `max_wait_ms` if the spec says background; otherwise `min(max_wait_ms, timeout_ms × wait_fraction)`. Phase 2 subtracts the expected run time once measurements exist.
- **When the wait reaches the cap, phase 0 starts the stage anyway, over budget** (admission result `overflow`), records it, and prints `[cardinal] Ran over budget after waiting 30 s (budget was full).` at the end. This is a phase 0 interim rule; phase 2 replaces it with an accepted job (C4). Alternatives rejected for phase 0: returning an error makes agents retry and adds load; waiting until the Bash timeout gets the command moved to the background by Claude Code with the wait still in front of it.
- After a wait of at least 1 s that ended in normal admission, the runner prints `[cardinal] Ran after waiting 40 s` at the end. Both notes go to the runner's stderr, which is the stage's stderr including its redirections.

### 5.5 Child execution

- **Execution:** argv is resolved through `PATH` as `env` does, with inherited stdio, cwd, and environment. The runner never proxies output, so terminal detection, buffering, and pipe behavior are unchanged, and the runner's death cannot break the child's output.
- **Unix group:** in step 4 the runner calls `setpgid(0, 0)` and records `pgid` as its own PID. `EPERM` is accepted when `getpgid(0)` already equals the runner's PID (it already leads its group, for example as a session leader); any other result is a pre-start failure. The child inherits the group. The runner leads the group, not the child, so that the PGID is known before the reservation is written and a crash at any point leaves a group that can be checked (section 4.5).
- **Unix signals:** the runner forwards SIGINT, SIGTERM, SIGHUP, and SIGQUIT to the rest of the group and keeps waiting. If its parent process exits while the child runs (the Bash call was killed with its shell's process group, which no longer contains the stage), the runner sends SIGTERM to the group and SIGKILL 5 s later. When the direct child exits, the runner releases; group members left behind are not killed (the same as running the stage without cardinal) and show up in `inventory`. Processes that leave the group (`setsid`) are not tracked.
- **Windows group:** in step 4 the runner creates a Job Object named `Local\cardinal-<host-id>-<job>` with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and assigns itself to it; the child inherits membership, so no grandchild escapes. On normal completion the runner clears the kill-on-close limit before exiting, so leftovers behave as on Unix; only the runner's abnormal death kills the group.

### 5.6 Exit status and infrastructure failure

| Situation | Exit status | stderr |
| --- | --- | --- |
| Child exited with code N | N | the child's output only (plus a wait note) |
| Child killed by signal N (Unix) | 128 + N | the child's output only |
| Pre-start failure, TTY passthrough | whatever the stage returns | nothing added |
| In-use lock held past the wait cap, or the worktree changed while waiting | 125 | `[cardinal] The worktree is being reclaimed; the command was not run.` |
| Runner error after the child started, child status unknown | 125 | `[cardinal] Infrastructure failure after the command started: <reason>. The command was not rerun.` |
| Runner killed after the child started | set by the killer (137 for SIGKILL) | nothing; the record has `job_started` without `job_finished`, and `explain job` reports `runner_died` |
| Release or record failure after the child exited | the child's status | nothing |

125 follows the convention of `env`, `nice`, and `timeout` for "the wrapper itself failed". It can collide with a child's own 125, so the stderr line and the records are the distinguishing signal, not the code alone.

### 5.7 Executor contract (phase 2 seam)

```rust
pub trait Executor {
    /// Sets up the process group or Job Object; runs before the reservation is written.
    fn prepare(&self) -> Result<GroupIdentity, NotStarted>;
    /// Starts the invocation. `Err` guarantees that nothing started, so the caller may run the stage itself.
    fn start(&self, inv: &Invocation) -> Result<Box<dyn Running>, NotStarted>;
}
pub trait Running {
    fn child(&self) -> ChildIdentity;          // written as `running`
    fn wait(self: Box<Self>) -> ExecOutcome;    // blocks; forwards cancellation
}
pub enum ExecOutcome { Exited(i32), Signaled(i32), InfraFailure(String) }
```

Phase 0 has one implementation, the local executor. The start boundary is the same for every executor: `starting` is written before `start`, and an `Ok` from `start` means the stage may have run. For phase 2's accepted jobs, the accepting runner starts a detached runner, which takes its own live lock and a shared in-use lock and rewrites the reservation's owner to itself before the accepting runner drops its locks and prints the job ID; both hold the locks for that moment, so the job is never unowned.

## 6. Claude Code hook contract

### 6.1 Facts

| Question | Answer | Source |
| --- | --- | --- |
| Is there a tool call ID in `PreToolUse`? | Yes: `tool_use_id`, next to `tool_name` and `tool_input` | [Hooks reference](https://code.claude.com/docs/en/hooks): "PreToolUse hooks receive `tool_name`, `tool_input`, and `tool_use_id`" |
| Session, cwd, mode | Common fields: `session_id`, `transcript_path`, `cwd`, `permission_mode`, `hook_event_name`; in a subagent also `agent_id` and `agent_type`. `permission_mode` takes `default`, `acceptEdits`, `dontAsk`, `auto`, `bypassPermissions` | Hooks reference; values checked against Claude Code 2.1.322 |
| Bash `tool_input` fields | `command`, `description`, `timeout` (ms), `run_in_background` | Hooks reference example input |
| Bash timeout default and maximum | `BASH_DEFAULT_TIMEOUT_MS` default 120000; `BASH_MAX_TIMEOUT_MS` default 600000 (the ceiling is the larger of the two) | [Environment variables](https://code.claude.com/docs/en/env-vars) |
| What happens at the Bash timeout? | A foreground command that reaches its timeout is moved to the background, not stopped, unless it starts with `sleep` or `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1` | [Tools reference](https://code.claude.com/docs/en/tools-reference) |
| `updatedInput` semantics | "Replaces the entire input object, so include unchanged fields alongside modified ones." Permission rules and auto-background eligibility are evaluated against the returned input. Combine with `allow` to auto-approve or `ask` to prompt; without `permissionDecision` the normal permission flow applies | Hooks reference |
| How is `if … then … else … fi` matched against permission rules? | Each branch's commands are matched separately, in one-line and multi-line form and after a leading `cd … &&`. Every other fallback shape (`&&`, `\|\|`, or `;` chains, `{ …; } \|\| …`, `test -x … && exec …`) prompts even when every part is allowed. `"ask"` prompts even when every part is allowed; `"allow"` runs the command when the original's rules would not. In `auto` mode, a rewritten command whose original stage is not allowed by a rule was denied by the auto-mode classifier while the original alone was allowed | Checked against Claude Code 2.1.322 |
| Several hooks returning `updatedInput` | Not documented; hooks run in parallel | Hooks reference |
| Hook timeout | `timeout` in seconds; default 600 for command hooks; a timed-out `PreToolUse` hook is discarded and the call continues through the normal permission flow | Hooks reference |
| Exit codes and output | Exit 2 blocks the tool call; other non-zero exits are non-blocking; exit 0 with empty stdout is success; exit 0 with invalid JSON is a non-blocking error | Hooks reference |
| `SubagentStop` input | `agent_id`, `agent_type`, `agent_transcript_path`, `stop_hook_active`, `last_assistant_message`, plus common fields | Hooks reference |
| `SessionEnd` input | `reason`: `clear`, `resume`, `logout`, `prompt_input_exit`, `other`; default hook timeout 1.5 s | Hooks reference |
| Shell and cwd of Bash commands | The user's shell, with aliases, functions, and options captured from its rc file at session start; a `cd` carries over to later Bash calls while inside the project directory (never in subagents) | Tools reference |
| Windows | The Bash tool runs in Git Bash; without Git Bash a separate PowerShell tool is used, whose input has the same fields | Tools reference; [Troubleshooting install](https://code.claude.com/docs/en/troubleshoot-install) |

### 6.2 Installation

The owner adds this to `~/.claude/settings.json` (never to a repository, G5); `examples/claude/settings.json` ships it:

```json
{
  "hooks": {
    "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "cardinal hook claude pre-tool-use", "timeout": 5 }] }],
    "SubagentStop": [{ "hooks": [{ "type": "command", "command": "cardinal hook claude subagent-stop", "timeout": 5 }] }],
    "SessionEnd": [{ "hooks": [{ "type": "command", "command": "cardinal hook claude session-end", "timeout": 2 }] }]
  },
  "permissions": { "allow": ["Bash(/usr/local/bin/cardinal run:*)"] }
}
```

The explicit `timeout` matters: the 600 s default would let a stuck hook stall every Bash call. Set `[hook] runner` to a path that survives upgrades; the hook's own executable may live in a versioned directory, and a vanished runner path silently turns every rewrite into the fallback. The allow rule names the runner path exactly as the hook writes it. cardinal must be the only hook that returns `updatedInput` for Bash, because the order in which several are applied is undefined.

### 6.3 `hook claude pre-tool-use`

1. Read stdin (bounded at 1 MiB) as JSON. Exit 0 with empty stdout, rewriting nothing, if `tool_name` is not `Bash`, `tool_input.command` is not a string, `hook.enabled` is false, `permission_mode` is `auto` (section 6.5), or anything fails.
2. Find and classify stages (section 7) within `hook.deadline_ms`, measured from process start. Past the deadline: no rewrite.
3. If at least one stage is classified, rewrite each classified stage (section 6.4) and print:

```json
{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"command":"<rewritten>","description":"<unchanged>","timeout":120000,"run_in_background":false}}}
```

`updatedInput` is the original `tool_input` object with only `command` replaced, kept as a `serde_json::Value` so fields unknown to cardinal survive unchanged. No `permissionDecision` is returned (section 6.5).

4. The effective timeout written into each spec: `tool_input.timeout` if it is a positive number, else `BASH_DEFAULT_TIMEOUT_MS` from the hook's environment (the hook inherits Claude Code's), else 120000; capped at `max(BASH_MAX_TIMEOUT_MS or 600000, default)`. `run_in_background: true` sets the spec's background flag.

The hook writes nothing to the state dir and takes no reservation (C1). It never exits non-zero: panics are caught at the top level and turned into "no rewrite".

### 6.4 Rewritten command

The hook replaces only the span of each classified stage, in place, and leaves every other byte of the command alone. For a stage `S` whose command word is `W`, the replacement is

```sh
if [ -x R ]; then S'; else S; fi
```

where `R` is the runner path and `S'` is `S` with `R run --spec-b64 <spec> --` inserted immediately before `W`. Assignment prefixes and redirections stay where they were in both branches. Example:

```sh
# original
cd packages/app && CI=1 pnpm test 2>&1 | tail -50 || echo failed
# rewritten
cd packages/app && if [ -x /usr/local/bin/cardinal ]; then CI=1 /usr/local/bin/cardinal run --spec-b64 eyJzY2hlbWEiOi… -- pnpm test 2>&1; else CI=1 pnpm test 2>&1; fi | tail -50 || echo failed
```

- **Everything else stays in the calling shell:** pipes, `&&`/`||`/`;` lists and their short-circuiting, a leading `cd` and the cwd it leaves for later Bash calls, shell options such as `pipefail` and `errexit`, functions used by other stages, and the command's first word unless its first stage is classified. Claude Code's timeout handling depends only on a leading `sleep`, and a `sleep` stage is never wrapped (section 7.2).
- **Both branches carry the stage's own words,** so variables, globs, `~`, and quoting are expanded by the calling shell either way, and exactly one branch runs. The `if` returns the status of the branch that ran, so pipeline and list semantics are unchanged.
- **`[ -x R ]`** is the fallback test: if the runner path is missing or not executable (uninstalled, moved, upgraded to another path), the `else` branch runs the original stage.
- **Several classified stages** in one command are each wrapped and reserved separately; the spec's stage index tells them apart. Scripts started inside a stage are not controlled separately; their weight is learned for the stage (design §6).
- **One-line form:** a stage span never contains a newline or a comment, so the replacement stays on one line and the rest of the line follows `fi` unchanged.
- On Windows the runner path is written with forward slashes (`C:/…/cardinal.exe`), which Git Bash accepts.

Running `bash`, `zsh`, and `sh` on this form gives the same output and status as the original for short-circuit lists (`cd missing && false || printf fallback`), `pipefail` pipelines, non-exported variables, globs, assignment prefixes, and redirections, and runs the original once with its status when the runner path is missing. `tests/rewrite_equivalence.rs` keeps these cases (section 13.5).

PowerShell tool (Windows without Git Bash), implemented in phase 3: the stage is wrapped as `if (Test-Path -LiteralPath 'R' -PathType Leaf) { & 'R' run --spec-b64 <spec> -- <stage words> } else { <stage> }`. Its permission matching has not been checked.

### 6.5 Permissions

Claude Code evaluates permission rules against the rewritten command, and splits `if … then … else … fi` into its branches for matching (section 6.1). The rewrite therefore returns no `permissionDecision`: with the allow rule for the runner installed, the rewritten command needs approval exactly when the original stage does, and deny rules on the original still match its copy in the `else` branch. Returning `"allow"` would run commands the owner never allowed, and `"ask"` would prompt for every command.

The check covered the `if` form as a whole command and after `cd … &&`. Stage wrapping also puts it inside pipelines and `&&`/`||` lists, and puts assignment prefixes in front of the runner. Each of these shapes is part of the G4 real-host check (section 15); the built-in classifier rewrites only shapes that check has confirmed, and leaves the others unrewritten.

**`auto` mode is not rewritten in phase 0.** The auto-mode classifier may deny the rewritten form of a stage that it allows in its original form; a command whose parts are all allowed by rules runs without the classifier. Phase 0 accepts the coverage loss: commands in `auto` mode bypass admission. A later phase can lift it by rewriting in `auto` mode only stages that an explicit allow rule covers, which needs cardinal to evaluate the owner's permission rules, or Claude Code to report the original call's permission decision to hooks.

### 6.6 `subagent-stop` and `session-end`

Both read stdin, append one record (`subagent_stopped` or `session_ended`, with `session_id`, `agent_id`, `agent_type`, `reason`, `cwd`), and never stop processes (C9). Both exit 0 whatever happens.

`session-end` also writes `sessions/<session>.json`: the agent process (the nearest ancestor of the hook whose name is not a shell: `sh`, `bash`, `zsh`, `dash`, `cmd.exe`, `powershell.exe`, `pwsh.exe`) and every descendant of it except the hook's own chain, each as PID, start time, name, and the first 256 bytes of the command line. It must finish within the 1.5 s default timeout; one process-table refresh takes milliseconds.

A process outlived its session when it is listed in a snapshot, is still alive with the same start time, and the agent process is gone. The agent check matters because `SessionEnd` with `clear` or `resume` keeps the same agent process running. Processes that detach from the agent before `SessionEnd` (double-forked daemons) are not in the snapshot; phase 1 can add a process-environment marker if the owner needs them.

`SubagentStop` cannot attribute processes to a subagent (nothing in a process identifies the subagent), so it only records.

## 7. Classification

### 7.1 Pipeline

```text
command string ──tokenize──> lists, pipelines, stages with byte spans
each stage ──normalize──> argv ──match rules──> class, rule, resources | unclassified
classified stages ──rewrite spans (last to first, so earlier offsets stay valid)──> rewritten command
```

A stage is one simple command: a pipeline element or a list element, with its own assignment prefixes and redirections. Unclassified stages are left alone: light unknown commands are not limited (design §7), and heavy unknown commands are limited only once a profile names them.

### 7.2 Tokenizer subset and refusals

The hand-written tokenizer accepts a conservative subset of POSIX shell syntax: words with `'…'`, `"…"` (backslash escapes inside), and `\x` escapes; `$NAME` and `${…}` inside words as opaque text; the operators `&&`, `||`, `;`, `|`, `&`, and newlines; redirections (`>`, `>>`, `<`, `2>&1`, `N>file`, `&>`); and `#` comments.

**The whole command is left unrewritten** when a stage span cannot be delimited with certainty or the outer shell could change what a stage means: a here-document, `$(`, backticks, `$((`, `<(`, `>(`, `(`, `)`, `{`, `}`, `[[`, `!`, a keyword in command position (`if then else elif fi for while until do done case esac function select`), a function definition, `eval` or `alias` anywhere in command position, or unbalanced quotes.

**A single stage is left unwrapped** (other stages may still be wrapped) when its command word is a shell builtin, keyword, or precommand modifier (`time`, `command`, `builtin`, `exec`, `noglob`, `nocorrect`, and the rest of the shell's builtins), `sleep`, or in `[classify] skip`; when it runs asynchronously (followed by `&`), because its runner would outlive the calling shell; or when its pipeline contains another classified stage, because a stage waiting for admission could block a running one through the pipe. Not understanding a command can only cause a missed rewrite, never a changed one.

### 7.3 Normalization

Normalization decides the class only; the rewrite always uses the stage's own words. For each stage:

1. Drop leading `NAME=value` assignments.
2. Unwrap shell-level wrappers (POSIX knowledge, built in): `env` (with assignments and `-u NAME` only; other options make it unclassified) and `nice [-n N]`.
3. Unwrap `sh`, `bash`, `zsh`, `dash` with `-c STRING` (optionally `-e`, `-l`): tokenize `STRING`; the stage is classified by its first matching simple command, one level deep.
4. Unwrap tool wrappers from `[classify] wrappers` (profile data, not code): drop the wrapper's words, then words starting with `-` until the first that does not.
5. Replace argv[0] by its base name (`./node_modules/.bin/vitest` → `vitest`), and drop a `+toolchain` word right after `cargo`.

`pnpm exec vitest`, `npx vitest`, `env X=1 vitest`, and `bash -c 'vitest'` all normalize to `["vitest"]`; each stage is still wrapped as written (`R run … -- npx vitest`).

### 7.4 Rule matching and resources

- A pattern is split into words. It matches a normalized argv when it is a prefix of it; the word `*` matches any one word. A match is cancelled if any argv word equals an `except` entry.
- The first matching rule in precedence order (section 3.3) classifies the stage. Each classified stage is its own job with its rule's resources.
- Rules have `id`, `commands`, optional `except`, `class`, optional `resources` (positive integers per resource name). The rule ID in records is `<profile file stem>/<id>`.

### 7.5 Shadowed tools

An alias or function in the owner's rc that shadows a classified tool's name is not applied when the stage runs through the runner, which executes the tool from `PATH`. Rules name tools that the owner chose in their own profile, and both the profile and the rc are the owner's, so the owner can keep a shadowed tool out of rewriting with `[classify] skip` (per repository, or in a generic profile for all). This relies on the owner knowing what their rc defines; cardinal does not detect shadowing in phase 0. A later check could run the owner's shell once and report every rule command word that resolves to an alias or function.

### 7.6 Classifier unit seam

The built-in classifier implements `trait Classifier { fn classify(&self, req: &ClassifyRequest, deadline: Instant) -> ClassifyResponse }`, called once per stage. External classifier units (phase 1+) speak the same types as JSON over stdin and stdout, one request per process:

```ts
// stdin
{ schema: "cardinal.classify-request.v1", command: string, stage: string, argv: string[], cwd: string,
  repo: { common_dir: string, main_worktree: string, remote: string | null } | null,
  tool: { name: "Bash", timeout_ms: number, background: boolean } }
// stdout
{ schema: "cardinal.classify-response.v1",
  decision: "wrap" | "pass",
  class?: string, rule?: string, resources?: { [name: string]: number },
  reason: string }
```

`argv` is the normalized form. Classifiers run in a fixed order; the first `wrap` wins; a unit that misses the deadline, exits non-zero, or prints invalid JSON counts as `pass` (design §5). Units decide the class only; where a stage may be wrapped is decided by section 7.2.

## 8. Observation

### 8.1 Child processes for git and gh

- **Constructed environment:** start empty and copy an allowlist: `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_*`, `TMPDIR`, `XDG_CONFIG_HOME`, `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM`, `GH_TOKEN`, `GITHUB_TOKEN`, `GH_HOST`, `GH_CONFIG_DIR`, `SSH_AUTH_SOCK`, and on Windows `SystemRoot`, `SystemDrive`, `ComSpec`, `PATHEXT`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `ProgramData`, `TEMP`, `TMP`. Every other `GIT_*` (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY`, `GIT_NAMESPACE`, `GIT_CEILING_DIRECTORIES`, `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`, …) is dropped, so a cardinal process started from a git hook can never act on another repository. Then add `GIT_OPTIONAL_LOCKS=0`, `GIT_TERMINAL_PROMPT=0`, `GH_PROMPT_DISABLED=1`, `GH_NO_UPDATE_NOTIFIER=1`, `NO_COLOR=1`. The `GIT_CONFIG_*` location variables stay because they decide where the owner's configuration lives, and git must read it the way the owner's git does (design §9).
- **Always** `git --no-optional-locks -C <absolute path> …`, so `status` does not refresh and rewrite the index. Observation never runs `fetch` or anything else that writes to a repository; stale remote-tracking refs only make fewer worktrees look merged.
- **Deadlines:** every child has one (`git_timeout_ms`, `gh_timeout_ms`). Output is read on threads; on the deadline the child is killed and the fact is `unknown`. Spawn failure, non-zero exit (where not meaningful), unparsable output, and timeouts all become `unknown`, never "safe".

### 8.2 Repositories and worktrees

Repositories to observe: the main worktrees of profiles with `match.paths`, plus `[observe] repos`, plus `--repo` arguments. Per repository:

| Fact | Command |
| --- | --- |
| Worktrees | `git worktree list --porcelain -z` (path, HEAD, branch, `locked`, `prunable`) |
| GitHub repository | `git remote get-url origin`, parsed to `owner/name` for `github.com` (or `GH_HOST`); anything else makes PR facts `unknown` |
| Base | `[git] base`, else `git symbolic-ref --short refs/remotes/origin/HEAD`; missing makes merge evidence `unknown` |
| Open PRs | `gh pr list --repo O/N --state open --limit 500 --json number,headRefName,headRefOid,isCrossRepository,url`; exactly 500 results makes it `unknown` |

Per worktree:

| Fact | Command | Value |
| --- | --- | --- |
| HEAD, branch, upstream, dirty, ignored entries | `git status --porcelain=v2 -z --branch --untracked-files=all --ignored=matching` | `dirty` is any entry other than `!` (ignored) |
| `has_upstream` | `# branch.upstream` header | detached HEAD: `no` |
| `unpushed` | `git rev-list --count HEAD --not --remotes` | `0` → `no`; otherwise `yes`, unless merge evidence is `merged_pr` (the commits exist on GitHub) |
| `base_ancestor` | `git merge-base --is-ancestor HEAD <base>` | exit 0 → `yes`, 1 → `no`, else `unknown` |
| `merged_pr` (only if not a base ancestor) | `gh pr list --repo O/N --state merged --search <HEAD oid> --json number,headRefOid,mergedAt,url --limit 20` | `yes` if a returned PR has `headRefOid == HEAD` |
| `open_pr` | from the repository's open PR list | `yes` if a same-repository PR has `headRefName == branch` or `headRefOid == HEAD` |

Merge evidence is `base_ancestor` or `merged_pr`, matched on the HEAD object ID, never on the branch name. So commits added after a merged PR, or a reused branch name, give no evidence (C12). Squash merges without a matching head stay without evidence and are left to the arbiter (design §7).

### 8.3 Ignored data

Each `!` entry from `git status` is classified with the profile's `[data]` patterns:

1. `protected` if the entry or anything below it matches a protected pattern (path patterns are checked by existence; name patterns by walking the entry);
2. else `regenerable` if the entry is, or is inside, a regenerable match;
3. else `unclassifiable`.

Walks never follow symbolic links and never cross into a directory that contains its own `.git` entry, which is reported as a `nested_repository` instead.

### 8.4 Managed worktrees

A worktree is managed by another app when its path is under a `[observe] managed` root or git lists it as `locked`. The `managed_by` field names the app or `git-lock`. Managed worktrees and everything inside them are only reported (design §4); `act` refuses both plan kinds for them (section 9.2).

### 8.5 Disk usage

`inventory --sizes` walks each worktree and its regenerable entries (threads per worktree, no symlink following). It reports `allocated_bytes` (blocks × 512 on Unix, file length on Windows, each inode once) and `exclusive_bytes` (only files whose link count is 1), because package managers hard-link into a shared store and deleting a linked `node_modules` frees much less than its apparent size. Without `--sizes`, size fields are `null`.

### 8.6 Processes

`inventory` lists reservations with their derived state and the members of their process groups, and outlived processes from session snapshots (section 6.6). Inventory is read-only: it never deletes stale reservations or signals anything.

## 9. plan, act, and invariants

### 9.1 Plan kinds

| Kind | Target | Effect |
| --- | --- | --- |
| `reclaim_worktree` | A linked worktree (never a main worktree) | `git -C <main> worktree move <path> <trash>`, inspect, then `git -C <main> worktree remove <trash>` without `--force` (git refuses dirty or locked worktrees itself, a second line of defense). The branch is kept |
| `reclaim_dir` | A regenerable ignored entry inside an observed worktree | Rename to `<trash>`, inspect, then `std::fs::remove_dir_all` (does not follow symbolic links) or file removal |

The trash path is `<parent of the target>/.cardinal-trash-<plan>`: on the same file system, so the move is an atomic rename. A trash directory in the state dir was rejected because it is often on another volume (always across drives on Windows), where a rename fails. The trash entry is the target itself under another name, exists only while `act` runs, and an interrupted `act` is finished by the next run (section 4.9). Caches outside worktrees (phase 1), process stops for outlived processes, and branch deletion are not phase 0 plan kinds. A recovery-needed reservation is never released by a plan; it is released only when its group is empty (section 4.5).

### 9.2 Checks

Each plan carries a list of checks. An **invariant** failing or being `unknown` makes `act` refuse for every caller, including the owner (design §3). A **condition** failing only makes owner approval necessary (section 9.3).

| Check | Kinds | Role | Pass when |
| --- | --- | --- | --- |
| `observed` | both | invariant | Every git and gh fact the plan depends on is known |
| `not_in_use` | both | invariant | Exclusive in-use lock acquired and no live or recovery-needed reservation names the worktree (section 4.6) |
| `not_managed` | both | invariant | The worktree is not under a managed root and not git-locked (design §4) |
| `clean` | `reclaim_worktree` | invariant | `dirty` is `no` |
| `no_unpushed` | `reclaim_worktree` | invariant | `unpushed` is `no` |
| `no_open_pr` | `reclaim_worktree` | invariant | `open_pr` is `no` |
| `has_upstream` | `reclaim_worktree` | invariant | The branch has an upstream (G6). This also keeps freshly created worktrees, which start clean at the base, from being reclaimed before their first push |
| `regenerable` | `reclaim_dir` | invariant | The target is a `regenerable` ignored entry of a worktree that observation listed |
| `not_symlink` | both | invariant | The target path itself is not a symbolic link |
| `no_protected` | both | invariant | Nothing in the moved tree, descendants included, is protected |
| `no_unclassifiable` | both | invariant | Nothing in the moved tree is unclassifiable ignored data |
| `no_nested_repository` | both | invariant | No directory below the moved tree's root has its own `.git` entry (another worktree, a submodule, a nested clone) |
| `no_process_inside` | both | invariant | No process has its working directory inside the moved tree |
| `merge_evidence` | `reclaim_worktree` | condition | `base_ancestor` or `merged_pr` is `yes` |

`plan` evaluates every check on the target in place, and `act` evaluates the last four invariants again on the moved tree. Plans are created even when invariants fail, with `invariants_ok: false`, so the arbiter sees the reason and an `act` on them demonstrates the refusal (G10).

### 9.3 Approval scope

Approval is needed when a condition fails; otherwise none is needed. This is the design's reclaim scope (design §5): without owner approval, the arbiter can reclaim regenerable data and whole worktrees that meet every reclaim condition. **`act` computes approval from the checks it evaluates at run time**; the `approval` stored in the plan is informational and never widens what `act` may do. `act` executes a plan that needs approval only with `--owner-approved`. The arbiter skill never passes that flag. It is self-regulation, not a security boundary (design §3). Phase 3 replaces the flag with an approval intent bound to the plan ID from the web UI.

### 9.4 Identity binding

A plan stores the target identity: `host_id`, repository common dir, worktree path, worktree root identity (`dev` and `ino` on Unix; creation time where the OS has one), HEAD object ID, and target path. `act` re-reads all of them; any difference makes the plan `expired` (C14). A new worktree created at the same path has a new root identity even if its HEAD and branch are the same.

### 9.5 act procedure

1. Load the plan; a wrong `host_id` gives `expired`.
2. Claim: create `plans/<id>/claim` with `create_new`, recording the trash path; fsync. If it exists: `already_executed`. (`--dry-run` skips claiming.)
3. Take the worktree's in-use lock exclusively (`try_lock`), then check reservations under the admission lock.
4. Re-observe the target from scratch (section 8) and compare identity; a difference gives `expired`.
5. Evaluate the checks on the target in place. Any invariant not `pass`: `rejected`. If a condition fails and `--owner-approved` is absent: `rejected` with reason `owner_approval_required`.
6. Move the target to the trash path (section 9.1); append `renamed` to the claim.
7. Inspect the moved tree: walk it completely, descendants included, for `no_protected`, `no_unclassifiable`, and `no_nested_repository`; check `no_process_inside`; for a worktree, also run `git status` at the trash path for `clean`. Any failure: move it back, append `restored`, `rejected`. Otherwise append `inspected`.
8. Drop the in-use lock, then delete the moved tree. Anything created at the original path after step 6 is outside the trash and survives. A partial deletion (for example a file held open on Windows) gives `partial`; everything in the moved tree was inspected, so nothing deleted can be owner data.
9. Write `result.json` (fsync), append an `act` record.

Two `act` runs on the same plan race only at step 2, where `create_new` lets exactly one through. The residual window is a process that already holds an open handle inside the moved tree other than its working directory; it can still write there after the inspection.

### 9.6 Dry-run and unit gating

- `act --dry-run` performs steps 1 and 3–5 and the walk of step 7 on the target in place, without claiming, moving, or deleting. It reports `dry_run` with the checks and the bytes that would be freed when the plan would execute, and otherwise the refusal (`rejected` or `expired`) it would get.
- `act --caller unit:<name>` marks the caller. When `host.toml` has `[unit.<name>] act = false` (the default for any unit), `act` behaves as `--dry-run` and appends a `planned` record (C16). Phase 0 has no units, but the gate is part of `act` now so units cannot bypass it later.

## 10. Inbox and explain

### 10.1 Inbox

In phase 0 items are raised only by the refresh that `cardinal inbox` runs first (skipped with `--no-refresh`); from phase 1, units raise them through the same `inbox::raise` function. The refresh first runs the recovery pass of section 4.4 under the admission lock, so recovery does not depend on another job being admitted. It then raises and clears:

| Kind | Key | Raised when | Cleared (resolved by `cardinal`) when |
| --- | --- | --- | --- |
| `recovery_needed` | `recovery_needed:<job>` | A reservation is `recovery_needed` (evidence: group members, verified or not) | The reservation is gone |
| `outlived_processes` | `outlived:<session>` | A session snapshot has live processes after its agent process ended | None of them is alive |
| `interrupted_act` | `interrupted_act:<plan>` | An interrupted act could not move its target back (section 4.9) | The trash entry is gone |

Statuses are `open`, `escalated` (waiting for the owner), `resolved`, and `dismissed`. Raising a key that exists updates `seen` and evidence of an open or escalated item and leaves resolved and dismissed items alone (C19).

### 10.2 explain

`explain` shows where a decision came from: configuration files and keys, the rule, observations (the exact command and its result), and records. Subjects: `job <id>`, `plan <id>`, `inbox <key>`, and `command [--cwd DIR] -- <string>` (a classification and rewrite trace without running anything).

## 11. CLI and JSON contracts

### 11.1 Conventions

- Every JSON document on stdout is one object with a `schema` field `"cardinal.<name>.v<N>"`. Adding fields keeps `N`; removing or changing a field's meaning bumps it. Readers ignore unknown fields.
- Enums are lowercase `snake_case` strings. Tri-state facts are `"yes" | "no" | "unknown"`.
- Paths are native absolute paths (on Windows without the `\\?\` prefix). Times are `*_ms` (Unix ms) or `*_s` (process start, Unix s).
- Human-readable errors go to stderr; machine-readable ones to stdout as `cardinal.error.v1`.

| Exit code | Meaning (all commands except `run` and `hook`) |
| --- | --- |
| 0 | Success, including an `act --dry-run` that would execute |
| 1 | Error; stdout has `cardinal.error.v1` |
| 2 | Usage error (clap) |
| 3 | `act` did not execute: `rejected`, `expired`, or `already_executed` |

`run` exits as in section 5.6; `hook` always exits 0.

### 11.2 Commands

| Command | Output |
| --- | --- |
| `cardinal run [--spec-b64 SPEC] -- COMMAND [ARG]...` | The stage's own output and status (section 5) |
| `cardinal hook claude pre-tool-use \| subagent-stop \| session-end` | Hook JSON or nothing (section 6) |
| `cardinal inventory [--repo PATH]... [--sizes]` | `cardinal.inventory.v1` |
| `cardinal inbox [--status open\|escalated\|resolved\|dismissed\|all] [--no-refresh]` | `cardinal.inbox.v1` |
| `cardinal inbox resolve KEY --outcome resolved\|escalated\|dismissed [--note TEXT]` | `cardinal.inbox-item.v1` |
| `cardinal plan [--repo PATH]...` | `cardinal.plan-list.v1`: plans for every whole-worktree candidate and every regenerable entry of worktrees that are not candidates |
| `cardinal plan reclaim-worktree PATH \| reclaim-dir PATH` | `cardinal.plan.v1` |
| `cardinal act PLAN [--dry-run] [--owner-approved] [--caller arbiter\|owner\|unit:NAME]` | `cardinal.act-result.v1` (default caller `arbiter`) |
| `cardinal explain job ID \| plan ID \| inbox KEY \| command [--cwd DIR] -- STRING` | `cardinal.explain.v1` |
| `cardinal --version` | text |

### 11.3 Schemas

Types are written in TypeScript notation; `?` marks fields that may be absent, `| null` fields that are present but empty.

```ts
type Tri = "yes" | "no" | "unknown";
type Resources = { [name: string]: number };            // positive integers
type Check = { name: string; role: "invariant" | "condition";
               result: "pass" | "fail" | "unknown"; detail: string };
type GroupIdentity = { kind: "pgid"; pgid: number; leader_start_s: number }   // leader = the runner
                   | { kind: "job_object"; name: string };

// cardinal.error.v1
{ schema; error: { code: "state_unavailable" | "config_invalid" | "not_found" | "ambiguous_id"
                        | "invalid_argument" | "not_implemented" | "internal"; message: string } }

// cardinal.run-spec.v1  (decoded --spec-b64)
{ schema; class: string | null; rule: string | null; resources: Resources;
  timeout_ms: number; background: boolean; stage_index: number; stage: string;
  session_id: string | null; agent_id: string | null; tool_use_id: string | null }

// cardinal.reservation.v1  (reservations/<job>.json)
{ schema; job_id: string; state: "reserved" | "starting" | "running";
  boot_s: number; created_ms: number; updated_ms: number;
  runner: { pid: number; start_s: number };
  group: GroupIdentity;
  child: null | { pid: number; start_s: number };                   // set at `running`
  stop_signal_ms: number | null;                                     // last SIGTERM by recovery
  priority: "normal"; resources: Resources;
  admission: "within_budget" | "overflow";
  class: string | null; rule: string | null;
  worktree: null | { key: string; path: string };
  session: null | { id: string; agent_id: string | null; tool_use_id: string | null; stage_index: number };
  cwd: string; argv: string[] }                                      // argv truncated to 16 KiB in total

// Reservation as shown by inventory and inbox evidence
type ReservationView = Reservation & {
  derived: "live" | "recovery_needed" | "stale";
  members: { pid: number; start_s: number; name: string }[];
  verified: boolean };

// cardinal.inventory.v1
{ schema; observed_ms: number;
  host: { id: string; hostname: string; os: "macos" | "linux" | "windows"; arch: string; boot_s: number };
  budget: Resources; reserved: Resources;                  // reserved = live + recovery_needed
  reservations: ReservationView[];
  repos: Repo[];
  outlived: { session_id: string; ended_ms: number;
              processes: { pid: number; start_s: number; name: string; command: string }[] }[];
  errors: { scope: string; message: string }[] }

type Repo = {
  common_dir: string; main_worktree: string; profiles: string[];
  github: string | null;                                   // "owner/name"
  base: null | { ref: string; oid: string };
  gh: "ok" | "unknown" | "not_applicable"; gh_error: string | null;
  worktrees: Worktree[] };

type Worktree = {
  path: string; main: boolean; head: string | null; branch: string | null; upstream: string | null;
  managed_by: string | null;                               // app name or "git-lock"
  facts: { dirty: Tri; has_upstream: Tri; unpushed: Tri; open_pr: Tri;
           merge_evidence: "base_ancestor" | "merged_pr" | "none" | "unknown";
           merged_pr: null | { number: number; url: string };
           in_use: { value: boolean; jobs: string[] } };
  ignored: { path: string; class: "regenerable" | "protected" | "unclassifiable" | "nested_repository";
             allocated_bytes: number | null; exclusive_bytes: number | null }[];
  reclaim: { candidate: boolean; failed: string[]; unknown: string[] };  // check names
  allocated_bytes: number | null; exclusive_bytes: number | null };

// cardinal.plan.v1
{ schema; id: string; created_ms: number; host_id: string;
  kind: "reclaim_worktree" | "reclaim_dir";
  target: { path: string; repo: string; worktree: string; head: string | null;
            root_id: null | { dev: number; ino: number }; root_created_ms: number | null };
  bytes: { allocated: number | null; exclusive: number | null };
  checks: Check[]; invariants_ok: boolean;
  approval: "none" | "owner" }                             // informational; act recomputes it

// cardinal.plan-list.v1
{ schema; plans: Plan[]; errors: { scope: string; message: string }[] }

// cardinal.act-result.v1
{ schema; plan_id: string; caller: string;
  status: "executed" | "partial" | "dry_run" | "rejected" | "expired" | "already_executed";
  reasons: string[];                                       // failing check names, "owner_approval_required", "identity_changed:<field>"
  checks: Check[]; freed_bytes: number | null; error: string | null; finished_ms: number }

// cardinal.inbox-item.v1
{ schema; key: string; kind: "recovery_needed" | "outlived_processes" | "interrupted_act";
  status: "open" | "escalated" | "resolved" | "dismissed";
  subject: { job_id?: string; session_id?: string; plan_id?: string; worktree?: string };
  evidence: object;                                        // the observation that raised it
  created_ms: number; updated_ms: number; seen: number;
  resolution: null | { outcome: "resolved" | "escalated" | "dismissed"; by: string; note: string; at_ms: number } }

// cardinal.inbox.v1
{ schema; items: InboxItem[] }

// cardinal.explain.v1
{ schema; subject: { kind: "job" | "plan" | "inbox" | "command"; id: string };
  summary: string;                                         // e.g. "admitted within budget after 12 s"
  sources: ( { kind: "config"; file: string; key: string; value: unknown }
           | { kind: "rule"; rule: string; pattern: string; normalized_argv: string[] }
           | { kind: "observation"; command: string[]; exit: number | null; result: string }
           | { kind: "record"; record: object } )[];
  rewritten: string | null;                                // for `command`: the rewrite the hook would produce
  outcome: string | null }                                 // e.g. "runner_died", "fail_open:<reason>"

// cardinal.record.v1  (one line in records/*.jsonl)
{ schema; ts_ms: number; host_id: string;
  kind: "job_admitted" | "job_started" | "job_finished" | "job_fail_open" | "job_infra_failure"
      | "job_recovered" | "tty_passthrough" | "session_ended" | "subagent_stopped" | "plan_created"
      | "act" | "planned" | "inbox_raised" | "inbox_resolved" | "hook_error" | "config_error";
  job_id?: string; plan_id?: string; inbox_key?: string; session_id?: string;
  data: object }                                           // kind-specific; job_finished.data has exit, signal, waited_ms, measurements

// cardinal.session-snapshot.v1  (sessions/<session>.json)
{ schema; session_id: string; reason: string; ended_ms: number;
  agent: { pid: number; start_s: number; name: string };
  processes: { pid: number; start_s: number; name: string; command: string }[] }
```

### 11.4 Phase 2 placeholders

These names are reserved and not registered in phase 0 (an unregistered command is a clap usage error; nothing pretends to work):

| Command | Contract sketch |
| --- | --- |
| `cardinal wait JOB [--timeout-ms N]` | Blocks until the accepted job ends; prints its output and exits with its status, or prints `cardinal.job.v1` with `--json` |
| `cardinal cancel JOB` | Cancels a waiting or running accepted job |
| `cardinal logs JOB [--follow]` | Streams `jobs/<id>/log` |
| `cardinal exec --stdio` | Executor protocol (design §8): JSON over stdin and stdout |

## 12. Libraries

| Crate | Decision | Reason |
| --- | --- | --- |
| `clap` (derive) | Use | Already in the workspace; subcommands, typed flags, trailing argv |
| `serde`, `serde_json` | Use | Every JSON contract; `Value` keeps unknown hook fields intact |
| `toml` | Use | Configuration (design §9) |
| `sysinfo` (`system` feature only) | Use | Process table, start times, parents, working directories, boot time on all three OSes; memory sensors later come from the same crate |
| `rustix` (`process` feature) | Use (Unix) | Safe `setpgid`, `getpgid`, `kill_process_group`, `test_kill_process_group`; avoids raw `libc` |
| `signal-hook` | Use (Unix) | Forwarding signals to the group without hand-written handlers |
| `windows-sys` | Use (Windows) | Job Object FFI. Chosen over `windows` (design candidate) because only a few raw calls are needed and it compiles much faster |
| `base64` | Use | `--spec-b64` encoding |
| `thiserror` | Use | Typed errors where callers branch (before or after the start boundary, config vs state) |
| `dunce` | Use | Canonical Windows paths without `\\?\`, so they compare equal to paths from git |
| `fs4` | Skip | std has file locks since Rust 1.89 with the same `flock`/`LockFileEx` semantics; the reserve file is written with zeros |
| `jobserver` | Skip (phase 2) | Only needed when cardinal shares a jobserver with Cargo |
| `anyhow` | Skip | Typed errors are needed for JSON error codes anyway |
| `rand`, `uuid`, `getrandom` | Skip | `RandomState` plus exclusive creation gives unique IDs |
| `chrono`, `time`, `jiff` | Skip | All times are integer milliseconds |
| `walkdir`, `ignore` | Skip | `read_dir` recursion is enough; git reports ignored entries |
| `shlex`, `conch-parser`, `brush-parser` | Skip | Splitting words is not enough to find stage spans, and a full grammar accepts constructs whose stages cannot be wrapped safely; the subset tokenizer refuses what it cannot delimit |
| `wait-timeout`, `duct` | Skip | One helper with `try_wait` polling and reader threads covers deadlines |
| `tracing`, `log` | Skip | Records and `explain` are the diagnostics; hooks must not write to stderr |
| `schemars` | Skip (phase 1 candidate) | Contract types in `contract` are the source of truth; generated JSON Schema files can follow when external units appear |
| `tempfile` (dev) | Use | Fixture directories in the OS temp dir |
| `assert_cmd` (dev) | Use | Running the real binary in integration tests |
| `proptest` (dev) | Use | Tokenizer, stage spans, rewrite round trips, JSONL torn tails |

## 13. Test strategy

### 13.1 Deterministic concurrency

Tests do not rely on starting processes at the same time. The `testpoint` module provides named pause points and failure injection, active only when `CARDINAL_TEST_DIR` is set (one cached environment read otherwise):

- `CARDINAL_TEST_PAUSE=<point>[,<point>]`: at a point the process creates `<dir>/<point>.<pid>.reached` and polls for `<dir>/<point>.release`; after 30 s it aborts so a broken test fails instead of hanging.
- `CARDINAL_TEST_FAIL=<point>`: the process returns an injected error at that point.

Points: `admission.locked`, `admission.waiting` (budget full, about to sleep), `inuse.waiting`, `run.before_starting`, `run.after_spawn`, `act.claimed`, `act.renamed`, `act.inspected`, `hook.classified`. **Every contended test waits for the second actor's `.reached` file before releasing the first**, so it proves the second actor actually hit the contended point. Examples:

- Two runners and a budget of one job: pause the first at `admission.locked`; start the second and wait for its `admission.waiting`; release; assert exactly one `within_budget` start before the other.
- Execution and deletion never overlap: pause `act` at `act.claimed` (exclusive in-use lock held); start a runner in the worktree and wait for its `inuse.waiting`; release `act`; assert the runner exits 125 and the stage never ran. Reverse order: a runner paused at `run.after_spawn`, then `act` is `rejected` with `not_in_use`.
- Deletion is limited to what was inspected: after `plan`, create `target/deep/x/fuzz/corpus/seed`; `act` is `rejected` and `target/` is back in place. Pause `act` at `act.inspected` and create a file at the original `target/` path; it survives the deletion.
- Two `act` runs on one plan: pause the first at `act.claimed`; the second returns `already_executed`.
- Start boundary: a runner killed at `run.before_starting` runs the stage zero times and the Bash-level status is 137 (the accepted case of section 5.3); an injected failure at `run.before_starting` or a failed spawn runs it once; a runner killed at `run.after_spawn` never reruns it.

### 13.2 Real process groups

Recovery tests run real children: `cardinal run -- sh -c 'sleep 30 & sleep 30 & wait'`, pause at `run.after_spawn`, SIGKILL the runner by its PID, then assert: the reservation is `recovery_needed` and verified, `act` on the worktree is `rejected` with `not_in_use`, the next admission or `cardinal inbox` sends SIGTERM and keeps counting, the group is empty afterwards (`kill(-pgid, 0)` gives `ESRCH`), and the next job is admitted. A runner killed at `run.before_starting` leaves a `stale` reservation. The derived-state function is also tested against a fake process table for the cases a real system cannot produce on demand: a reused PGID with a new leader (released), a reused PGID whose leader has exited (kept, never signalled), and a child that left its descendants behind (kept, never signalled). On Windows the same real test asserts the children die with the runner. Every test kills what it started, by PID, in a drop guard.

### 13.3 Git fixtures

`tests/support` provides the only way tests run git:

- Fixtures live under `tempfile::TempDir` in the OS temp dir, outside any repository.
- `Fixture::git(dir, args)` panics unless `dir`, every path-like argument (`-C`, `--git-dir`, `--work-tree`, absolute paths), and the resolved `git rev-parse --absolute-git-dir --git-common-dir` are inside the fixture root. A test builds a decoy fixture whose `.git` file points outside and asserts the helper refuses it.
- The environment is constructed, not inherited: `HOME`, `XDG_CONFIG_HOME`, and `GIT_CONFIG_GLOBAL` point inside the fixture, `GIT_CONFIG_NOSYSTEM=1`, `GIT_CEILING_DIRECTORIES` is the fixture's parent, `PATH` is the test process's; no other `GIT_*` passes.
- Author and committer identity come only from `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_COMMITTER_NAME`, `GIT_COMMITTER_EMAIL` (and fixed dates). Tests never run `git config user.*` or `--global`. The isolated config disables signing and hooks.
- "Remote" repositories are bare fixtures; GitHub is a fake `gh` (a shell script on Unix, a `.cmd` on Windows) selected with `[observe] gh`, returning canned JSON or failing (exit 1, timeout, invalid JSON).
- Product-side isolation is tested too: observation runs with `GIT_DIR`, `GIT_WORK_TREE`, and `GIT_INDEX_FILE` exported to point at a decoy fixture, and the decoy must be untouched.

### 13.4 Repository guard

`scripts/repo-guard snapshot|verify <repo> <file>` records, for the repository the tests run in: `git show-ref --head`, `git worktree list --porcelain`, `git config --local --list`, `git stash list`, and `git hash-object` of the index. `scripts/test` runs `cargo fmt --all --check` and `cargo clippy --all-targets --locked -- -D warnings`, takes a snapshot, runs `cargo test --locked`, and verifies. CI, the git hooks, and every work unit run it as `mise exec -- scripts/test`; CI runs it a second time with `GIT_DIR`, `GIT_WORK_TREE`, and `GIT_INDEX_FILE` exported as a git hook would. `tests/repo_guard.rs` proves the guard works: it snapshots a fixture, mutates it, and asserts `verify` fails.

### 13.5 Other rules

- Each integration test gets its own `CARDINAL_STATE_DIR` and `CARDINAL_CONFIG_DIR` in a temp dir.
- G5 and C20 tests list the worktree recursively, including ignored files, before and after hook, run, and act, and use stages that write nothing.
- C6 tests run each input through the real shells (`bash` and `zsh` where installed, `sh` always, Git Bash on Windows CI) once directly and once rewritten, and compare stdout, stderr, exit status, final `pwd` (printed by the outer shell after the command), and `env` output. Inputs include short-circuit lists (`cd missing && cargo test || printf fallback`), `pipefail` and `errexit`, non-exported variables and globs in a stage, assignment prefixes, redirections, several classified stages, and a missing runner path. Until the real runner exists, a fake runner that executes its argv after `--` stands in.
- The admission path performs no network or arbiter call at run time: `cardinal run` executes with recording fakes for `git`, `gh`, and `claude` first in `PATH` and a recording listener in `HTTP_PROXY`, `HTTPS_PROXY`, and `ALL_PROXY`; the test asserts no fake was invoked, no connection arrived, and the inbox directory is unchanged. On Linux it also asserts that `/proc/<runner>/fd` holds no socket at `admission.locked` and `run.after_spawn`.
- C13 fixtures put protected data deep inside a regenerable directory (`target/a/b/fuzz/corpus/seed`), so a check that stops at the top level fails. Refusal tests pair each refusal with a control that succeeds, including `reclaim_dir` of `node_modules` in a managed worktree (refused) next to the same in an unmanaged one (executed).
- CI runs the whole suite on macOS, Linux, and Windows. Unix-only tests (process groups, signals, zsh) are `#[cfg(unix)]`; Job Object tests are `#[cfg(windows)]`; nothing is skipped silently through features.

## 14. Work split

### 14.1 Order

```mermaid
flowchart LR
  F["F: foundation and contracts (first PR of #4)"] --> R["#4 runner and admission"]
  F --> H["#5 hook and classification"]
  F --> O["#6 observation and inventory"]
  F --> P["#7 plan, act, inbox, explain"]
  A["#8 arbiter skill"]
  R -. "integration gate" .-> H
  R -. "integration gate" .-> O
  R -. "integration gate" .-> P
  O -. "integration gate" .-> P
```

F declares every public function signature named below with bodies that return `cardinal.error.v1` code `not_implemented` (clippy denies `todo!` and `unimplemented!`), so each work unit only fills bodies in modules it owns plus one `match` arm in `cli`. Shared OS process helpers are implemented in F, not declared, because #4, #5, and #6 all need them.

| Work unit | Can start | Completes independently? | Integration gate |
| --- | --- | --- | --- |
| F | Immediately | Yes | — |
| #4 | After F | Yes | — |
| #5 | After F | Yes, with the fake runner of section 13.5 | `tests/rewrite_equivalence.rs` and `tests/hook_claude.rs` against the real runner, after #4 |
| #6 | After F | Yes, with an in-crate fake for reservations and in-use state | `tests/inventory.rs` against real reservations, after #4 |
| #7 | After F | Yes, with in-crate fakes for `observe` and `admission` | `tests/plan_act.rs`, `tests/act_single.rs`, and `tests/inbox.rs` against the real modules, after #4 and #6 |
| #8 | Immediately (content depends only on this document) | Yes | `tests/skill_commands.rs` against the clap definitions, after F |

A work unit is done when its own completion tests pass; its integration gate is a follow-up PR by the same unit once the dependency has merged.

### 14.2 Work units

| Work unit | Owns | Completion tests |
| --- | --- | --- |
| **F** (first PR of #4) | `src/lib.rs`, `src/main.rs`, `src/cli/mod.rs` (all commands and flags of section 11.2), `src/contract/`, `src/config/`, `src/state/`, `src/record.rs`, `src/sensor.rs`, `src/testpoint.rs`, `src/os/mod.rs` with the process table, start times, parent PID, process working directories, and boot time implemented; skeleton `mod.rs` of every other module with signatures: `admission::{list_reservations, in_use, recovery_pass}`, `observe::{inventory, observe_worktree}`, `classify::{find_stages, classify}`, `inbox::raise`, `plan::create`, `act::execute`; `tests/support/`, `scripts/repo-guard`, `scripts/test`, CI and `lefthook.yml` switched to `mise exec -- scripts/test`; `examples/config/` | `tests/records.rs` (torn tails, rotation), `tests/layering.rs` (passes, and fails when a forbidden import is added), `tests/repo_guard.rs`, `tests/fixture_guard.rs`, `tests/os_process.rs` |
| **#4** runner, admission, reservations | `src/admission/`, `src/run/`, `src/exec/`, `src/os/unix.rs` and `src/os/windows.rs` (groups, signals, Job Objects) | `tests/admission_concurrency.rs`, `tests/run_start_boundary.rs`, `tests/run_infra_failure.rs`, `tests/run_wait.rs`, `tests/run_tty.rs`, `tests/recovery.rs`, `tests/in_use.rs`, `tests/admission_isolation.rs` |
| **#5** Claude Code hook | `src/classify/`, `src/adapter/claude/`, `examples/claude/settings.json` | `tests/hook_claude.rs`, `tests/rewrite_equivalence.rs`, `tests/session_end.rs`, `tests/no_repo_writes.rs`, proptest suites in `classify` |
| **#6** observation, inventory | `src/observe/`, the `inventory` handler | `tests/observe_worktrees.rs`, `tests/observe_env.rs`, `tests/observe_readonly.rs`, `tests/inventory.rs` |
| **#7** plan, act, inbox, explain | `src/plan/`, `src/act/`, `src/inbox/`, `src/explain.rs`, their handlers | `tests/plan_act.rs`, `tests/act_single.rs`, `tests/act_trash.rs`, `tests/inbox.rs`, `tests/unit_dry_run.rs`, `tests/explain.rs` |
| **#8** arbiter subagent and skill | `integrations/claude-code/agents/cardinal-arbiter.md`, `integrations/claude-code/skills/cardinal-arbiter/SKILL.md`, install notes in `integrations/claude-code/README.md`, `tests/skill_commands.rs` | `tests/skill_commands.rs` parses every `cardinal …` line in the skill with the clap definitions |

Every work unit's completion command is `mise exec -- scripts/test`.

### 14.3 Arbiter skill v1 (#8) contract

The subagent runs headless, one at a time, and only through the CLI:

1. `cardinal inbox` → for each `open` item, `cardinal explain inbox KEY` and `cardinal inventory`.
2. Reclaim work: `cardinal plan` (or a targeted plan), then `cardinal act PLAN --caller arbiter` for plans with `invariants_ok: true` and `approval: "none"`. Never `--owner-approved`.
3. A plan that needs approval or a decision outside the reclaim scope (including `recovery_needed` items whose group is unverified, and `interrupted_act` items): `cardinal inbox resolve KEY --outcome escalated --note "<what the owner must decide, with the plan ID or processes>"`.
4. Resolve items it handled with `--outcome resolved` and a note; report refused acts with their `reasons`.

It never edits files in repositories, and never runs `git worktree remove`, `rm`, or `kill` directly. Starting it from an OS timer is phase 1; in phase 0 it is started by hand.

## 15. Purpose layer for phase 0

Real-host checks run on macOS with Claude Code and the hook installed as in section 6.2, a scratch state and config dir, and scratch repositories outside the owner's projects. Windows real-host checks are phase 3.

| ID | Condition (design §10) | Check on a real host |
| --- | --- | --- |
| G4 | Before the child starts, a missing, crashed, or unresponsive cardinal, or a runner that cannot start, leaves the command running exactly once with the same result; after start, no rerun and an infrastructure failure | A classified probe prints its args, `pwd`, sorted `env`, writes to stdout and stderr, appends one line to a counter file outside the repository, and exits 7. An agent runs it inside `cd sub && probe 2>&1 \| tail -5 \|\| echo fallback` (a) without the hook, (b) with the runner path not executable, (c) with the state dir unwritable, (d) with `admission.lock` held by another process past `lock_timeout_ms`, (e) with the hook's deadline forced to be missed, (f) with a failure injected before the start boundary. All outputs equal (a), and the counter grows by one each time. Permissions: in `default` mode, with allow rules for the probe and the runner, no shape of section 6.5 (whole command, after `cd … &&`, in a pipeline, in `&&`/`\|\|` lists, with an assignment prefix) prompts; without a rule for the probe, each prompts exactly as the original does; in `auto` mode nothing is rewritten. After start: SIGKILL the runner of a long probe; the counter shows one start, the exit status is 137, and `cardinal explain job` reports `runner_died`. The accepted zero-run case (runner killed before the spawn) ends with status 137 and no run |
| G5 | No files or instructions are added to target repositories | Snapshot `git status --porcelain --ignored --untracked-files=all` and a recursive listing (ignored files included) of a real worktree; install, run classified commands that write nothing, run `inventory`, `plan`, `act` (reclaiming a sibling worktree and a regenerable directory of this worktree); compare. Also confirm no repository file mentions cardinal |
| G10 | With valid plans, safe operations succeed and only G6/G7 operations are refused | Prepare: a merged clean worktree with `node_modules` (control), the `target/` of a dirty worktree (control), a worktree with `fuzz/corpus`, a `target/` with protected data deep inside, a dirty worktree, a worktree with an unpushed commit, a worktree without upstream, a worktree with a waiting job, and `node_modules` of an app-managed worktree. Ask the arbiter to reclaim all of them. The two controls are reclaimed; each other one is refused with the matching check; the protected worktree's clean `target/` is reclaimable separately |
| G14 | After a runner crash, reservation and in-use state remain until the group stops, then are released | Budget of one job. Start a classified command whose script starts background children; SIGKILL the runner. Immediately: `inventory` shows `recovery_needed` and `in_use`; `act` on that worktree's plan is refused (`not_in_use`). Start another classified command: it waits while recovery stops the group, then runs. Afterwards `ps -g <pgid>` is empty and the reservation is gone |

## 16. Plan layer for phase 0

Claims from design §11 that phase 0 implements, restated for phase 0, plus new claims A1–A11. EARS, one line each.

| ID | Claim (EARS) | Source | Test |
| --- | --- | --- | --- |
| C1 | When a classified stage reaches the PreToolUse hook, the hook shall rewrite it to run through `cardinal run` and shall not create a reservation. | G14 | `hook_claude::rewrite_takes_no_reservation` |
| C2 | When the runner decides admission, it shall decide inside the admission lock from the live and recovery-needed reservations and the new job's resources against the host budgets, and shall reserve all resources or none. (Sensors: phase 2.) | G1 | `admission_concurrency::*` |
| C5 | If the runner path is not executable, the shell prefix shall run the original stage; if the runner fails in any observable way before `spawn()` returns a child, the runner shall run the stage once; after that, cardinal shall not rerun it and shall report an infrastructure failure; if the hook misses its deadline, the command shall not be rewritten. | G4 | `run_start_boundary::*`, `run_infra_failure::*`, `hook_claude::deadline_no_rewrite`, `rewrite_equivalence::runner_missing` |
| C6 | When the hook rewrites a command, the other Bash input fields shall be unchanged, only classified stages shall be wrapped, and the command shall keep its meaning, including lists, pipelines, shell options, cwd, persisted cwd, and environment. (No injection in phase 0.) | G4 | `rewrite_equivalence::*`, `hook_claude::other_fields_preserved` |
| C8 | When a runner dies, its reservation and in-use state shall count as reserved and in use until its process group is empty or the boot changes; on Windows the Job Object stops the group, on Unix the recovery pass stops a verified group. | G14 | `recovery::*` |
| C9 | On SubagentStop and SessionEnd, cardinal shall record the end and shall not stop processes; processes that outlive their session shall appear in the inbox. | G8 | `session_end::*`, `inbox::outlived_processes_raised` |
| C12 | A whole worktree shall be a plan without approval only when it has merge evidence, is clean, has no unpushed commits, no open PR, an upstream, is not in use or managed, contains no protected, unclassifiable, or nested-repository data, and observation succeeded. | G3, G6, G7 | `plan_act::candidates::*` (commits after merged PR, reused branch name, no upstream, gh failure, control) |
| C13 | When `act` runs, it shall inspect the whole moved target including descendants and shall refuse it if it contains protected or unclassifiable ignored data. | G7, G10 | `plan_act::protected_deep_in_target_refused`, `plan_act::protected_worktree_refused_target_dir_reclaimed` |
| C14 | `act` shall require a plan ID, expire the plan when its identity changed, apply the same checks for every caller, and never run a plan twice. | G10 | `plan_act::identity::*`, `act_single::*` |
| C15 | The runner shall hold the worktree's in-use lock from acceptance until completion, and reclamation shall proceed only with the lock taken exclusively. | G6 | `in_use::waiting_and_running_jobs_block_act` |
| C16 | While a unit's `act` is false, `act` called as that unit shall leave the target unchanged and record the planned operation. | G3 | `unit_dry_run::*` |
| C17 | The admission path shall not use the network. | G11 | `layering::admission_has_no_observe_dependency`, `admission_isolation::no_network` |
| C18 | The admission path shall not call the arbiter or wait for it. | G1, G4 | `layering::admission_has_no_inbox_dependency`, `admission_isolation::no_arbiter` |
| C19 | When an item is raised, the inbox shall not duplicate an existing key and shall not reopen resolved or dismissed items. | G8 | `inbox::*` |
| C20 | cardinal shall create no files inside target repositories. | G5 | `no_repo_writes::*`, `act_trash::interrupted_act_finished` |
| A1 | When the wait reaches the wait cap, the phase 0 runner shall start the stage once over budget, record `overflow`, and print the overflow note to stderr. | Phase 0 interim rule: G4, and the seam for C4 | `run_wait::overflow_runs_once` |
| A2 | When cardinal runs git or gh, it shall pass a constructed environment from which repository-location variables inherited from the caller are absent. | G5 | `observe_env::inherited_git_vars_ignored` |
| A3 | Observation shall not write inside any repository, including its `.git` directory. | G5, C20 | `observe_readonly::index_and_refs_unchanged` |
| A4 | When a reservation's runner is dead, cardinal shall release it only when its process group is empty or the boot changed, and shall signal the group only when the recorded child is alive in it. | G14 | `recovery::unverified_group_not_signalled`, `recovery::reused_pgid_*`, `recovery::previous_boot_released` |
| A5 | If the hook fails, reads malformed input, misses its deadline, or sees `permission_mode` `auto`, it shall exit 0 with empty stdout. | G4 | `hook_claude::never_blocks::*`, `hook_claude::auto_mode_not_rewritten` |
| A6 | When `act` runs, it shall compute the need for owner approval from the checks it evaluates, and shall not execute a plan that needs it without `--owner-approved`. | design §5 | `plan_act::approval_recomputed_when_merge_evidence_disappears` |
| A7 | `act` shall refuse a target that contains another repository or worktree below its root. | G7 | `plan_act::nested_repository_refused` |
| A8 | `act` shall refuse both plan kinds for a worktree under a managed root or locked by git. | design §4 | `plan_act::managed_worktree_refused`, `plan_act::managed_reclaim_dir_refused` |
| A9 | If the in-use lock is held exclusively past the wait cap, the runner shall exit 125 without running the stage. | G6, G10 | `admission_concurrency::execution_and_deletion_never_overlap` |
| A10 | `act` shall delete only the tree it moved to trash and inspected. | G7 | `act_trash::created_after_plan_refused`, `act_trash::created_after_move_survives` |
| A11 | When stdin is a terminal, the runner shall run the stage directly without a process group or admission and record `tty_passthrough`. | G4 | `run_tty::passthrough` |

Out of scope for phase 0: C3 and C4 (phase 2, memory pressure and accepted jobs), C7 (phase 3, WSL), C10 and C11 (phase 1, disk thresholds and the reserve file), C21 (phase 2, measurement kinds).

## 17. Issues with the design

1. **Permission rules see the rewritten command.** Claude Code evaluates permission rules against `updatedInput`, not the original command ([hooks reference](https://code.claude.com/docs/en/hooks)), which design §6 did not account for. Checked against Claude Code 2.1.322: the `if … then … else … fi` form is split per branch, so with an allow rule for the runner the rewritten command needs approval exactly when the original does; every other fallback form prompts. Remaining limits: `auto` mode is not rewritten (section 6.5), and the shapes inside pipelines and lists still need the G4 real-host check.
2. **Running the whole command string in a child shell changed its meaning.** Claude Code keeps the Bash tool's working directory across calls and applies aliases, functions, and options captured from the user's rc file ([tools reference](https://code.claude.com/docs/en/tools-reference)). A child shell drops the persisted `cd`, rc options such as `pipefail`, and the short-circuit structure around the heavy command, and changes the command's first word, on which timeout handling depends. Resolution: only classified stages are wrapped, in place (section 6.4); design §6 "Command capture" and C6 now say so. Residual: an rc alias or function shadowing a classified tool (section 7.5).
3. **The Bash timeout does not kill.** design §6 bounds the wait by the Bash tool timeout. Claude Code moves a foreground command that reaches its timeout to the background instead of stopping it ([tools reference](https://code.claude.com/docs/en/tools-reference)), so an overrun costs the agent a background task, not the command. The bound is kept (section 5.4); design §14's first question is answered by section 6.1.
4. **Stopping "the recorded process group" needs an identity that survives a crash.** design §6 says admission or a reconciler stops a dead runner's group before releasing it. A child-led group's PGID is unknown until after the spawn, and a PGID can be reused once its group empties. Resolution: the runner leads the group, so the PGID is recorded before the reservation exists; the reservation is released only when the group is empty or the boot changed; only a group whose recorded child is still in it is signalled (section 4.5).
5. **G4 has two zero-run cases.** A runner killed between its start and the spawn, and a runner path that is executable but cannot be loaded, run the stage zero times. They are accepted because they end with a visible non-zero status, never run the stage twice, and never report success; a shell-side fallback on the exit status would run the stage twice after a real failure (section 5.3).
6. **A command in a worktree being reclaimed is refused, not failed open.** Running it would race the deletion; the runner exits 125 without running (section 4.6). This is cardinal working as intended, not one of G4's failure states.
7. **No accepted jobs in phase 0.** design §6's step for a wait that reaches the cap is an accepted job, which is phase 2. Phase 0 starts the stage over budget instead (A1). This weakens G1 until phase 2, which phase 0 does not claim.
8. **`git status` writes.** Without `--no-optional-locks`, `git status` refreshes and rewrites the index of the worktree it observes. Observation always disables optional locks (A3) so that observing does not change repositories.
9. **"No upstream" is an invariant.** G6 and G10 make a branch without upstream unreclaimable, so `act` refuses it even with owner approval. A worktree whose work was never pushed has to be pushed or removed by hand. This is intended: it also protects freshly created worktrees.
10. **Hard links distort disk numbers.** Package managers that hard-link from a shared store make `node_modules` look larger than what deleting it frees. Inventory reports exclusive bytes next to allocated bytes (section 8.5).
11. **PowerShell on Windows.** Claude Code on Windows without Git Bash runs commands through a separate PowerShell tool. design §9 names only the Bash tool. Section 6.4 fixes the PowerShell rewrite form for phase 3.
12. **Hook defaults are unsafe for cardinal.** Command hooks default to a 600 s timeout, and several hooks returning `updatedInput` have no defined order. The installation sets explicit timeouts and requires cardinal to be the only Bash-rewriting hook (section 6.2).
13. **Process groups do not track processes that leave them.** A tool that calls `setsid` or `setpgid` for its own children takes them out of the job's group on Unix; the reservation can be released while they run. Job Objects on Windows do not have this gap.
