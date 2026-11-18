# cardinal architecture

This document fixes the contracts that phases 0 and 1 build against: the crate and module layout, configuration, the state directory, the runner, the Claude Code hook, classification, observation, `plan` and `act`, the inbox, `cardinal reconcile` with disk thresholds and the arbiter's start, the read-only web UI, the CLI and its JSON, libraries, tests, and the work split. [design.md](design.md) is the source for principles, non-goals, invariants, purpose-layer conditions G1–G14, and plan-layer claims C1–C21; this document refers to its sections as "design §N" and does not restate them. Where the design turned out to be wrong or incomplete, [section 17](#17-issues-with-the-design) says so and gives the resolution used here.

## Contents

1. [Scope](#1-scope)
2. [Crate and module layout](#2-crate-and-module-layout)
3. [Configuration](#3-configuration)
4. [State directory](#4-state-directory)
5. [Runner](#5-runner)
6. [Claude Code hook contract](#6-claude-code-hook-contract)
7. [Classification](#7-classification)
8. [Observation](#8-observation)
9. [plan, act, and invariants](#9-plan-act-and-invariants)
10. [Inbox, reconcile, and explain](#10-inbox-reconcile-and-explain)
11. [CLI and JSON contracts](#11-cli-and-json-contracts)
12. [Libraries](#12-libraries)
13. [Test strategy](#13-test-strategy)
14. [Work split](#14-work-split)
15. [Purpose layer](#15-purpose-layer)
16. [Plan layer](#16-plan-layer)
17. [Issues with the design](#17-issues-with-the-design)

## 1. Scope

Phase 0 built the core (hook adapter, runner, admission and reservations, observation, records, `plan`/`act`/invariants, the inbox, the CLI for the arbiter), the Claude Code adapter, and the first arbiter skill, accepted by G4, G5, G10, and G14 (design §12) on macOS and Linux; on Windows the hook does not rewrite (section 5.5).

Phase 1's first release is report-only, and this document describes it: where it says phase 1 executes nothing, it means this release. Executing regenerable directories comes later in phase 1 from a design of its own (#33, to the requirements of section 9.7), and executing whole worktrees stays deferred (section 14.5). The first release adds detection and reporting, accepted by G8 and the report parts of G2, G3, G6, and G7 (section 15):

- **Reports, no execution** (section 9.3): `plan`, `inventory`, `reconcile`, and the arbiter identify reclaimable worktrees and regenerable directories and raise inbox items; `act` evaluates every plan and executes none (`execution_not_designed`).
- **`cardinal reconcile`** (section 10.3), started by an OS timer and at job ends: the inbox refresh, inbox items raised from observation (section 10.1), disk states and the built-in `disk-reclaim` reconciler's report (section 10.4), starting the arbiter (section 10.5), and the read-only web UI page (section 10.6).
- **The arbiter skill v2** (section 14.3) with squash-merge judgments and reports of what would be reclaimed.

Phase 1 first runs on macOS against real worktrees (section 14.4); the counts of that run are what the execution design starts from. Linux builds and tests everything; the real-data run is macOS only.

**Admission decides with exactly this:** the resources of every reservation that is live or recovery-needed, plus the new job's resources, compared per resource against the budgets in `host.toml`; a job gets all of its resources or none. There are no sensors in admission, no priority classes, no queue order, and no measured weights. Resource amounts come from the matching classification rule. This is the design's "sensor read failed, decide from reservations alone" case (design §7), kept until phase 2.

Seams left for later phases:

| Seam | Shape | Filled by |
| --- | --- | --- |
| Sensors in admission | `sensor::Sensors`; phase 1 implements disk free space for `reconcile` and the runner's job-end check (section 10.4); memory and admission's use of sensors stay `Unavailable` | Phase 2 |
| Priority classes | `Priority` enum with one variant, `Normal`; stored in reservations | Phase 2 |
| Accepted jobs, `wait`, `cancel`, `logs` | Command names and `jobs/` path reserved; lock handoff fixed in [section 5.7](#57-executor-contract-phase-2-seam); not registered in the CLI | Phase 2 |
| Queue tickets | `tickets/` path reserved | Phase 2 |
| Job spec, placement, executor | `exec::Invocation` (local form), `exec::Placement::Local`, and the `Executor` contract of section 5.7 | Phase 2 |
| Measurements | Records carry `measurements: [{kind, …}]`; only `wall_time` is written | Phase 2 (C21) |
| Parallelism injection | Not implemented; classification rules reject an `env` key | Phase 2 |
| Classifier and reconciler units | The built-in classifier and the built-in `disk-reclaim` reconciler are unit-shaped; the classifier JSON is fixed in [section 7.6](#76-classifier-unit-seam); `act` honors per-unit `act = false` ([section 9.6](#96-dry-run-and-gating)); external units are not run | Later |
| Disk admission (heavy jobs wait below the critical line, growth reserved before a job) | None; below the critical line the owner is told through the inbox (section 10.4) | Phase 2 |
| Pushing to the Cloudflare control plane | The snapshot document of section 10.6; nothing is sent | Owner decision (section 10.6) |
| Executing reclamation | `act` reports `execution_not_designed`; `plans/<plan>/claim`, `result.json`, and `<state>/reserve` stay reserved | Regenerable directories later in phase 1 (#33), to the requirements of section 9.7; whole worktrees deferred |

## 2. Crate and module layout

### 2.1 One crate

Everything lives in the single crate `crates/cardinal` (library plus the `cardinal` binary). The library target exists so integration tests can call modules directly; it is not a published API. The workspace has one more crate, `crates/test-support` (`cardinal-test-support`), a dev-dependency that holds the spawn API and git fixtures of section 13.3; it does not depend on `cardinal`.

| Option | Verdict |
| --- | --- |
| One crate with modules, layering checked by a test | **Chosen.** One compile unit, one binary, no version coordination. The dependency direction below is enforced by `tests/layering.rs`, which fails when a module imports one it must not |
| Workspace of `cardinal-core`, `cardinal-os`, `cardinal-claude`, `cardinal-cli` | Rejected for now. The compiler would enforce layering, but every contract change would touch several manifests, and no second consumer of the core exists yet. Split when one appears (a unit SDK, a control-plane connector) |

### 2.2 Modules

| Module | Owns | May use |
| --- | --- | --- |
| `contract` | serde types for every JSON document in [section 11](#11-cli-and-json-contracts) and the record, reservation, and run-spec files | std, serde |
| `config` | `host.toml` and `repos/*.toml`: locations, parsing, defaults | `contract` |
| `state` | State paths, atomic and durable writes, lock helpers with deadlines, IDs and keys, JSONL append and read | `contract` |
| `record` | Record appending, rotation, reading | `state`, `contract` |
| `testpoint` | Pause points and failure injection for tests ([section 13.1](#131-deterministic-concurrency)) | std |
| `os` | Platform backends behind one API: process table, start times, parent PID, process working directories, boot time; process-group setup and signals (Unix); `os::child`, which runs a short-lived child in an environment the caller constructs, under an absolute deadline and an output cap, and kills the child's process group (Unix) or Job Object (Windows) at the end of every run; on Unix it waits, signals groups, and takes over SIGCHLD and the termination signals through `os::unix`, process-wide state that cardinal, a command line program, owns ([section 8.1](#81-child-processes-for-git-and-gh)); `os::disk`, the free space of the volume holding a path (section 10.4); `os::spawn_detached`, which starts a cardinal process in a new session with null standard streams and does not wait for it | std, rustix, signal-hook, libc, sysinfo, windows-sys, objc2-core-foundation |
| `sensor` | `Sensors` trait; the host implementation reads disk free space by path through `os::disk` (section 10.4); memory stays `Unavailable` | `os`, `contract` |
| `exec` | `Invocation` (argv; cwd and env by inheritance), `Placement::Local`, `Executor` trait, local executor | `os`, `contract` |
| `admission` | Budget math (pure), reservation store, liveness and the recovery pass, worktree in-use query | `state`, `config`, `os`, `sensor`, `record`, `contract` |
| `run` | The runner: start boundary, fail-open, wait loop, child supervision, release, the job-end disk check that starts `cardinal reconcile` as a separate process (section 5.2) | `admission`, `exec`, `record`, `state`, `config`, `os`, `testpoint` |
| `classify` | Shell tokenizer, stage finder, normalizer, rule matcher, stage rewriting (section 6.4), `Classifier` trait, built-in classifier | `config`, `contract` |
| `adapter::claude` | Hook input and output, session-end snapshot | `classify`, `config`, `record`, `state`, `os` |
| `observe` | git and gh invocation with constructed environments and deadlines, repository and worktree facts, merge evidence and content in the base, closed pull requests, ignored-data classification, disk usage, outlived processes | `config`, `state`, `admission` (read-only queries), `os`, `contract` |
| `plan` | Plan generation, identity binding, the checks | `observe`, `admission`, `state`, `record`, `contract` |
| `act` | Plan evaluation and its report (sections 9.5 and 9.6); it executes nothing in phase 1 | `plan`, `observe`, `admission`, `inbox`, `state`, `record`, `contract`, `testpoint`, rustix |
| `inbox` | Inbox store, dedup, refresh (recovery pass and outlived processes) | `admission`, `observe`, `state`, `record`, `contract` |
| `reconcile` | `cardinal reconcile` and `cardinal arbiter run`: the pass order, disk states, the `disk-reclaim` reconciler, inbox items raised from observation, the arbiter's trigger and launcher (sections 10.3–10.5) | `act`, `plan`, `inbox`, `observe`, `sensor`, `snapshot`, `admission`, `record`, `state`, `config`, `os`, `contract` |
| `snapshot` | `cardinal.snapshot.v1` and the static page of the read-only web UI (section 10.6) | `inbox`, `record`, `state`, `contract` |
| `explain` | Explanation documents | `adapter::claude` (the hook's repository, origin, and timeout lookups for `explain command`), `classify`, `record`, `plan`, `inbox`, `admission` (the reservation snapshot of `explain job`), `state`, `contract` |
| `cli` | clap definitions and thin handlers that print JSON | everything |

"May use" is read transitively: a module may also use whatever the modules it may use may use, so `run` may use `contract` through `admission`. `testpoint` is open to every module above the foundation. `tests/layering.rs` checks every `crate::` and `super::` path in `src/` against this reading; a glob import of the crate root or an alias of `crate` counts as using every module, and `clippy::wildcard_imports` is denied, so no module comes into scope without a path the test sees.

```mermaid
flowchart TD
  cli --> adapter_claude[adapter::claude]
  cli --> run
  cli --> plan
  cli --> act
  cli --> inbox
  cli --> explain
  cli --> observe
  cli --> reconcile
  reconcile --> act
  reconcile --> snapshot
  reconcile --> sensor
  explain --> adapter_claude
  adapter_claude --> classify
  run --> admission
  run --> exec
  act --> plan
  act --> inbox
  plan --> observe
  inbox --> observe
  observe --> admission
  admission --> sensor
  sensor --> os
  admission --> os
  exec --> os
  admission --> state
  observe --> state
  classify --> config
  state --> contract
```

Two rules matter most and are part of the layering test:

- `admission`, `run`, and `exec` never import `observe`, `inbox`, `plan`, `act`, `reconcile`, or `snapshot`, and `admission` uses neither `std::net` nor `std::process`. gh lives only in `observe`, nothing on the admission path raises inbox items, and the runner starts `cardinal reconcile` only as a separate process after the stage's status is known (C17, C18). Section 13.5 checks the same at run time.
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

[disk]                   # lines for every watched volume, in MiB of free space (section 10.4)
warning_mib = 20480      # below: under pressure, reported (C10)
recovery_mib = 30720     # pressure ends above this
critical_mib = 5120      # below: critical
volumes = [{ mount = "/Volumes/Scratch", warning_mib = 600, recovery_mib = 900, critical_mib = 200 }]  # per-mount lines

[arbiter]
command = ["claude", "-p", "…", "--allowedTools", "…"]  # default: the launch of integrations/claude-code/README.md
timeout_s = 1200         # the launcher stops the session after this
min_interval_s = 1800    # between passes (section 10.5)
retry_s = 7200           # an item is due again this long after its pass ended

[reconcile]
observe_every_s = 3600   # full observation (git, gh) at most this often; the rest runs every pass

[unit.disk-reclaim]      # [unit.<name>] tables honor only `act` (section 9.6)
act = false              # an explicit act as this unit records planned while false (C16); the pass's disk report records planned either way
```

`[hook] runner` must be absolute and consist only of `A–Z a–z 0–9 / . _ + -`; otherwise the hook does not rewrite and records `config_error`. The path is the only configuration value that appears in shell text, and this rule means it never needs quoting.

`[disk]` lines must satisfy `critical < warning < recovery`, globally and per volume; `volumes` entries match the mount point a path resolves to (section 10.4). `[arbiter] command` is an argv, never a shell string; launchd and systemd start it with the timer's environment, so its first word should be absolute or found on the timer's `PATH` (section 10.3).

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

- **Repository identity** is the canonical git common dir. A profile matches when one of `match.paths` has that common dir, or when `git remote get-url origin` normalizes to one of `match.remotes`. Path matching needs no child process and is tried first; remote matching runs git (section 8.1) and only when a rule could match a stage at all. In the hook, a git still running at the hook's deadline means no rewrite, and a git that fails, or prints anything but one URL, leaves the profiles that match without the remote.
- **Precedence:** matching profiles in file-name order, then generic profiles in file-name order. The first rule whose pattern matches wins. `[data]` lists and `[classify] skip` are the union of the matching and generic profiles. `[git] base` and `[classify] wrappers` come from the first profile that sets them.
- **Data patterns:** an entry without `/` matches a file or directory with that name at any depth. A `regenerable` entry with `/` matches that worktree-relative path; a `protected` entry with `/` matches that sequence of names at any depth (`fuzz/corpus` protects `target/a/b/fuzz/corpus`), so protection never depends on where a tool put its data. Observation, `plan`, and `act` all match through `config::data_pattern_covers`. No globs.
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
  worktrees/<key>.lock        # empty; in-use lock, held shared by jobs
  sessions/<session>.json     # SessionEnd snapshots awaiting the outlived-process sweep
  inbox/.lock                 # empty; serializes inbox upserts
  inbox/<key-hash>.json       # one per inbox item
  plans/<plan>/plan.json      # immutable after creation
  plans/<plan>/claim, result.json  # reserved for the execution design (section 9.7); not created
  reconcile.lock              # empty; one reconcile pass at a time
  reconcile/state.json        # pass times; the arbiter trigger's last snapshot
  reconcile/observation.json  # the last full observation (cardinal.inventory.v1)
  disk/<volume>.json          # disk state of one watched volume (section 10.4)
  arbiter/.lock               # empty; held by the arbiter launcher (section 10.5)
  arbiter/last.json           # the last arbiter pass
  arbiter/<pass>.log          # an arbiter pass's output, capped
  ui/index.html, ui/snapshot.json  # the read-only web UI (section 10.6)
  records/records.jsonl       # current record file
  records/records.<n>.jsonl   # rotated files, n = 1..keep-1
  records/.lock               # empty; appenders shared, rotation exclusive
  tmp/                        # temporary files for atomic writes (same file system)
  jobs/                       # reserved for phase 2 accepted jobs
  tickets/                    # reserved for phase 2 queue tickets
  reserve                     # reserved for the execution design; not created
```

| File | Format | Written by | Read by |
| --- | --- | --- | --- |
| `VERSION`, `host-id` | text | first cardinal process that finds them missing: a fully written temp file is hard-linked into place, which fails if another process won, and the loser reads the winner's file | all |
| `reservations/<job>.json` | `cardinal.reservation.v1` | owning runner; the recovery pass deletes others' reservations; both only under the admission lock (section 4.3) | admission, observe, inbox, plan, act, explain, all under the admission lock |
| `sessions/<session>.json` | `cardinal.session-snapshot.v1` | `hook claude session-end` | inbox refresh |
| `inbox/<key-hash>.json` | `cardinal.inbox-item.v1` | inbox raise, refresh, resolve, and the arbiter launcher's assignments and outcomes, all under `inbox/.lock` | `inbox`, `explain`, the arbiter trigger (under `inbox/.lock`) |
| `plans/<plan>/plan.json` | `cardinal.plan.v1` | `plan` | `act`, `explain` |
| `disk/<volume>.json` | `cardinal.disk-state.v1` | `reconcile`, under `reconcile.lock` | `reconcile`, `inventory`, the UI |
| `reconcile/*.json` | `cardinal.reconcile-state.v1`, `cardinal.inventory.v1` | `reconcile`, under `reconcile.lock` | `reconcile`, the runner's job-end check (file time only), the UI |
| `arbiter/last.json` | `cardinal.arbiter-pass.v1` | the launcher, under `arbiter/.lock`; a trigger that finds a pass abandoned takes that lock to record it | the arbiter trigger, `inventory`, the UI |
| `records/*.jsonl` | one `cardinal.record.v1` object per line | every command | `explain`, `inventory`, the UI |

### 4.2 Write discipline

- **Atomic replace:** write to `tmp/<name>.<pid>.<n>`, flush, rename over the target. Readers see the old or the new file, never a mix. Leftover temp files older than one hour are removed by `plan` and `inbox` runs.
- **fsync policy:** reservations, live locks, and records are not fsynced; they are meaningless after a power loss (reservations from an earlier boot are released, section 4.5). Inbox items are fsynced (file, then parent directory on Unix), because losing one could bring back a resolved item or lose a pass's assignment (section 10.5). Plans are written without fsync; one lost to a power loss makes `act` report `not_found`, and the arbiter plans again. Disk states, reconcile state, and arbiter pass records are not fsynced: after a power loss they are measured or decided again. On macOS a durable write uses `F_FULLFSYNC` (std's `File::sync_all`), because `fsync` there leaves the drive's cache unflushed. On Windows, `FlushFileBuffers` on the file only.
- **Exclusive creation for uniqueness:** a job ID is claimed by creating `live/<job>.lock` with `create_new`, a plan ID by creating `plans/<plan>/` with `create_dir`; a collision retries with a new ID.
- **IDs:** 16 lowercase hex characters from a randomly keyed SipHash (`std::hash::RandomState`) of time, PID, and a counter. No RNG crate. The CLI accepts any unique prefix of at least 4 characters.
- **Keys:** worktree keys and inbox key hashes are FNV-1a 64 of the canonical path or key, as 16 hex characters (hand-written, stable across Rust releases unlike `DefaultHasher`). A worktree-key collision only makes two worktrees share an in-use lock (more conservative); inbox files store the full key and probe `-1`, `-2`, … on collision.
- **Times** are integers in milliseconds since the Unix epoch (`*_ms`). Process start times are seconds, as the OS reports them (`*_s`).

### 4.3 Locks

Locks use `std::fs::File::{lock, lock_shared, try_lock, try_lock_shared}` (stable since Rust 1.89): `flock` on Linux and macOS, `LockFileEx` on Windows. The OS drops them when the holder dies, which is what makes liveness detection work without TTLs.

- Lock files are always empty and separate from data files, because Windows locks are mandatory: a locked byte range cannot be read by other processes.
- std opens files close-on-exec and non-inheritable, so a child never inherits the runner's locks. A surviving child therefore cannot keep a dead runner looking alive.
- Waiting with a deadline is a `try_lock` loop with backoff (5 ms doubling to 50 ms). Every acquisition has a deadline; there is no unbounded `lock()`.
- Lock order is always: `reconcile.lock`, then the worktree in-use lock, then the admission lock, then the inbox or records lock. Nothing takes them in another order. `arbiter/.lock` comes after `reconcile.lock` and before the inbox lock: it is held by the arbiter launcher, whose session takes the others in this order through the commands it runs, and briefly by a trigger that records an abandoned pass (section 10.5); it is never inherited.
- **Reservations and live lock files change only under `admission.lock`, and `reservations/` is listed only under it.** A directory listing that runs while another process renames or unlinks an entry of that directory may omit entries, unchanged ones included, so a pass that listed `reservations/` while a runner replaced its reservation could count too few and admit over budget (C2). Under the lock: the admission and recovery passes (section 4.4), each write of `starting` and `running` and each release (section 4.5), the deletion of the live lock file of a runner that never got a reservation, and the readers `inventory`, `plan`, `act`, and `explain job`. A reader holds the lock only to read every reservation file and releases it before it evaluates them: liveness probes, the process table, git, records, and the inbox stay outside. Every one of these waits at most `lock_timeout_ms`. A reader that times out reports the reservations as unreadable, never as none: `inventory` adds an error with scope `reservations`, `inventory`, `plan`, and `act` leave the `not_in_use` check `unknown`, so it does not pass, and `explain job` fails as it does when the records cannot be read. Section 4.5 gives what a timeout does to a runner's writes.
- Lock files (`admission.lock`, `worktrees/*.lock`, `inbox/.lock`, `records/.lock`, `reconcile.lock`, `arbiter/.lock`) are never deleted while in use, so no process can lock a file another process has already unlinked. `worktrees/*.lock` grows by one empty file per worktree ever used and is not pruned: a runner opens it before it takes any other lock, so no lock could make its removal safe.

### 4.4 Admission protocol

The critical section only reads small files and writes one; process probes are a few system calls.

1. Take `admission.lock` exclusively (deadline `lock_timeout_ms`; on timeout, fail open).
2. On the runner's first pass only: create `live/<job>.lock` with `create_new` and take it exclusively. The runner holds it until it exits.
3. Run the **recovery pass**: read every `reservations/*.json` (a file that vanished meanwhile is skipped) and evaluate each one (section 4.5). Delete `stale` ones and their live lock files, and delete live lock files that are free and have no reservation. The pass never signals: `live` and `recovery_needed` reservations stay as they are.
4. Sum the resources of `live` and `recovery_needed` reservations. If, for every resource the new job needs, `sum + need ≤ budget` (resources without a budget always fit), write `reservations/<job>.json` (atomic) with `state: "reserved"` and the runner's group identity (section 5.5).
5. Release `admission.lock`.

The liveness lock is held before the reservation becomes visible, so another admitter can never see the new reservation with a free liveness lock. Live lock files are created, deleted, and garbage-collected only inside the admission lock, so collection can never delete a file between its creation and its locking. A missing live lock file counts as free. `cardinal inbox` runs the same recovery pass (section 10.1).

### 4.5 Reservation lifecycle and recovery-needed

```text
reserved ──(written immediately before spawn)──> starting ──(after spawn)──> running ──> deleted at release
```

Each transition, and the deletion at release, takes `admission.lock` with the deadline `lock_timeout_ms` (section 4.3). When the lock is not free in time, nothing is written or deleted:

- `starting`: the reservation stays `reserved` and the runner fails open with `admission_lock_timeout` (section 5.3). The release that fail-open does takes the lock too; when it times out, the job is abandoned: the reservation and live lock file stay and the live lock is dropped, and the recovery pass releases the reservation as `stale` once the runner has left its group.
- `running`: the reservation stays the complete `starting` one written before the spawn, `job_infra_failure` is recorded, and the stage keeps running. A `starting` reservation whose runner dies counts as `recovery_needed`, unverified, while its group lives.
- Release, and a job dropped while still `reserved`: the job is abandoned as above, and the recovery pass releases the reservation once the group is empty.

The group identity is fixed before the reservation exists: the runner makes itself a process-group leader, so the PGID is the runner's PID (section 5.5). A reservation's derived state is computed by whoever reads it; it is never stored:

| Condition (evaluated in order) | Derived state |
| --- | --- |
| `try_lock` on `live/<job>.lock` fails (the owner holds it) | `live` |
| `boot_s` differs from the current boot time by more than 2 s | `stale`: no process survives a reboot |
| The group is empty: `kill(-pgid, 0)` fails with `ESRCH`, or a process whose PID equals the PGID has a start time other than the runner's (POSIX does not reuse a PID while a process group with that ID exists, so the job's group must have emptied first) | `stale` |
| Otherwise | `recovery_needed` |

A `recovery_needed` group is **verified** when the recorded child (PID and start time) is alive and still has the recorded PGID; the inbox item shows this with the group's members. **Recovery never signals a group, verified or not:** nothing pins a dead runner's PGID between a check and a signal, so the group could empty and its ID pass to an unrelated group in that window. The reservation keeps counting until the group is empty or the boot changes. PGID reuse by an unrelated group therefore only delays release, and the delay clears itself when that group ends. Only a live runner signals its own group (section 5.5): the group's ID is the runner's PID, so no other group can take it while the runner lives. Nobody, including the owner, can release a non-empty group's reservation (G14). Recovery-needed reservations count as reserved for admission and as in use for their worktree.

### 4.6 Worktree in-use lock

- The worktree of a job is found by walking up from the runner's cwd to the nearest `.git` entry (file or directory); the key is the FNV-1a of that directory's canonical path. A job outside any worktree takes no in-use lock.
- The runner takes `worktrees/<key>.lock` **shared** at acceptance, before admission and before any wait, and holds it until the stage has exited, on every path that runs the stage: admitted, overflow, fail-open, and TTY passthrough (C15). Several jobs can share a worktree.
- **The lock marks a worktree in use; it does not protect data.** When the lock cannot be opened or taken within the wait cap (another process holds it exclusively, or the state dir is broken), the runner runs the stage anyway (G4) and records `in_use_unprotected`, so no deletion could rest on it (section 9.7, section 17 item 6).
- `plan`, `inventory`, and `act` only probe the lock, never taking it or waiting. A worktree is in use when the lock is held, when a `live` or `recovery_needed` reservation names the worktree key (read under the admission lock; this covers a crashed runner, whose shared lock the OS has already dropped, and a runner that went on without the lock), or when a process other than the reader has its working directory inside the worktree.

### 4.7 Records

- One JSON object per line, appended with a single `write` on a file opened in append mode (`O_APPEND`; `FILE_APPEND_DATA` on Windows), so concurrent appenders do not interleave.
- An appender holds `records/.lock` **shared** from opening `records.jsonl` until its write is done (waiting at most 1 s for a rotation to finish, else the record is dropped). Rotation and removal take it **exclusively**, so no file is renamed or removed while an appender has it open, and a delayed appender never writes into a file a later rotation has replaced.
- When `records.jsonl` exceeds `max_bytes`, the appender that notices takes the lock exclusively with a 100 ms deadline (on timeout the next append rotates), checks the size again, and renames `records.<keep-2>` → `records.<keep-1>` … `records.jsonl` → `records.1.jsonl`; with `keep = 1` it removes `records.jsonl`.
- Readers skip lines that do not parse. A crash mid-write costs at most the torn line and the line appended after it.
- A record failure, a panic in an append included, is never surfaced to a command and never causes a rerun (C5, C11): the runner loses the record and goes on.

### 4.8 Inbox store

`inbox/<key-hash>.json` holds one item (section 10.1). Upserts take `inbox/.lock` exclusively: an existing item with the same key is updated (`seen` incremented, `evidence` replaced) and is never reopened once `resolved` or `dismissed`.

### 4.9 Plans store

`plans/<plan>/plan.json` is written once, without fsync. `act` only reads it: it writes nothing to the plan's directory and neither claims nor consumes the plan (section 9.5). Plan directories older than 30 days are removed by `plan` runs.

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
| at `running` | Reservation with the child's identity | `recovery_needed` (verified while the child lives) until the group is empty, then `stale` |
| after the child exits, before release | Reservation | `recovery_needed` while members remain, then `stale`; records lack `job_finished` |
| during release | Nothing or a free live lock | Recovery pass |

`act` changes nothing but the records. Power loss: reservations, live locks, and in-use locks are reset by the reboot (`boot_s` check, OS locks); inbox items survive through fsync; plans may be lost and are planned again.

## 5. Runner

### 5.1 Invocation

```text
cardinal run [--spec-b64 SPEC] -- COMMAND [ARG]...
```

- **The stage** arrives as argv after `--`: the stage's own words after the calling shell has expanded them (variables, including ones that are not exported, globs, `~`, quoting), with the stage's redirections already applied to the runner process by that shell. The runner executes the argv directly, without a shell, so expansion, cwd, environment, and redirections are exactly those of the calling shell.
- **`--spec-b64`** is unpadded base64url of a `cardinal.run-spec.v1` JSON document (section 11.3): class, rule, resources, Bash timeout, background flag, session, agent, and tool-use IDs, stage index, and the stage's source text. Everything the runner needs from the hook travels here, so no configuration value is ever spliced into shell text, and base64url contains nothing a shell treats specially. Without a spec (people, tests), `admission.default_resources` applies.

### 5.2 Steps

1. Start watching signals and the parent process (section 5.5). When the signal handlers cannot be set up, the runner takes the in-use lock (step 4) and then runs the stage by TTY passthrough (step 2) or fails open.
2. If stdin is a terminal (**TTY passthrough**): take the in-use lock as in step 4, record `tty_passthrough`, and run the stage as fail-open does.
3. Decode the spec, resolve the state dir, load `host.toml`.
4. Take the worktree in-use lock shared (section 4.6). If that is not possible within the wait cap, continue without it and record `in_use_unprotected`.
5. Set up the process group (section 5.5).
6. Admission (section 4.4). If the budget is full, release the admission lock, sleep (250 ms doubling to 2 s), and retry until the wait cap (section 5.4).
7. Write `starting` (the start boundary, section 5.3), look again for a signal or the parent's exit, spawn the stage, write `running` with the child's PID and start time, append `job_started`.
8. Wait for the child, forwarding signals.
9. Release: delete the reservation when no member of the group other than the runner is left, otherwise leave it for the recovery pass (section 5.5); append `job_finished` (exit status, `waited_ms`, `measurements: [{kind: "wall_time", ms}]`), drop the live lock and the in-use lock, run the job-end disk check (below), and exit with the child's status or re-raise its signal (section 5.6).

**Fail open** (section 5.3): append `job_fail_open` if records still work, drop the reservation and live lock if taken (abandoning them when the admission lock is not free in time, section 4.5), restore the original process group, spawn the stage in it, and finish as in step 9; the runner never `exec`s, so the in-use lock stays held until the stage exits. Nothing is printed, except `<command>: <OS error>` with exit 127 (not found) or 126 when the stage cannot be executed, as shells do.

**Job-end disk check:** once the stage's status is known and the runner's locks are dropped, on every path that ran the stage, the runner reads the free space of its worktree's volume (or its cwd's) with `statfs`/`statvfs` (microseconds; never the slower measure of section 10.4) and compares it with that volume's warning line from `host.toml`. Below it, and when `reconcile/state.json` is more than 60 s old, it starts `cardinal reconcile --trigger job-end` with `os::spawn_detached` (new session, null standard streams) and does not wait. Any failure is ignored and recorded at most; the check never changes the exit status or output and never runs before the status is known (C18).

**Cancellation:** a handled signal (section 5.5) or the parent's exit before the spawn, seen during any wait or at the spawn itself on every path, ends the run without starting the stage. The runner appends `job_finished` with `cancelled`, releases what it holds, and dies by that signal, or exits 129 when the parent exited.

### 5.3 Start boundary

- **The stage has started once `spawn()` has returned a child.** Before that, the runner itself runs the stage on every failure it can observe: signal handlers that cannot be set up (step 1), an error in steps 3–7, a failed `spawn()` (the OS guarantees that no child ran), and a panic, which a `catch_unwind` around the whole run turns into fail-open. Every wait before the spawn has a deadline: the admission lock (`lock_timeout_ms`, then fail open), the in-use lock (the wait cap, then unprotected), and the budget (the wait cap, then overflow, section 5.4).
- **After `spawn()` has returned, the runner never runs the stage again.** The runner marks the stage as started the moment `spawn()` returns, before it reads anything about the child. A later error or panic ends in waiting for the child (again, by its PID, when the wait itself panicked) and returning its status when possible, and otherwise in exit 125. A panic while recovering from a panic re-enters recovery, which goes on from what the runner holds; a runner that still cannot get past its panics aborts without unwinding, like a runner that crashed. Once `starting` is written, no path that drops the job, unwinding included, deletes its reservation: the job is then abandoned, and only the recovery pass releases the reservation once the group is empty (section 4.5).
- **The `starting` record**, written immediately before `spawn()`, is the boundary as seen from the state dir: a reservation at `starting` whose runner died may or may not have a child, and recovery treats it as possibly running.
- **Zero-run cases that remain:** the runner killed (for example by SIGKILL) between its start and the spawn; a runner path that is executable but cannot be loaded (`[ -x R ]` passes, the exec fails with a loader error); and a cancellation, by the parent's exit or a signal before the spawn (section 5.2). The first two end the Bash call with a visible non-zero status (137, or 126 with the shell's message), nothing runs twice, and nothing reports success (section 17, item 5).

### 5.4 Wait cap and overflow

- Wait cap: `max_wait_ms` if the spec says background; otherwise `min(max_wait_ms, timeout_ms × wait_fraction)`. Phase 2 subtracts the expected run time once measurements exist.
- **When the wait reaches the cap, the runner starts the stage anyway, over budget** (admission result `overflow`). This is an interim rule until phase 2 replaces it with an accepted job (C4). Alternatives rejected: returning an error makes agents retry and adds load; waiting until the Bash timeout gets the command moved to the background by Claude Code with the wait still in front of it.
- **Waits add no output,** because text from the runner would enter the stage's pipes (`2>&1 | jq .`); `job_finished` records `waited_ms` and the admission result for `explain job`. Phase 2's accepted-job reply is the only output cardinal adds to a successful command (design §6).

### 5.5 Child execution

- **Execution:** argv is resolved through `PATH` as `env` does, with inherited stdio, cwd, and environment. The runner never proxies output, so terminal detection, buffering, and pipe behavior are unchanged, and the runner's death cannot break the child's output.
- **Process group:** in step 5 the runner calls `setpgid(0, 0)` and records `pgid` as its own PID; a failure is a pre-start failure. The child inherits the group, whose PGID is thus known before the reservation is written (section 4.5). A runner that already leads its group (`getpgid(0)` equals its PID) does not take that group as its own, because other processes can be in it: a job-control shell makes the first command of a pipeline the leader of the job's group, which the pipeline's other commands join. Step 5 then fails without calling `setpgid`, and the run fails open in that group, where the runner signals only its child (signals below).
- **Signals:** on the admitted path the runner forwards SIGINT, SIGTERM, SIGHUP, and SIGQUIT to the rest of the group. On the fail-open and TTY paths the child shares the caller's group and gets terminal signals itself; the runner ignores SIGINT and SIGQUIT while waiting, as `system()` does, and forwards SIGTERM and SIGHUP. A signal the caller ignores (`SIG_IGN`) gets no handler, so it stays ignored for the stage, as in a direct run. SIGCHLD is the exception: the runner sets it to its default action before it starts any child, because a caller that ignores it would have Linux reap the runner's children, and the runner could neither keep a child's PID reserved nor learn its status. The stage starts with SIGCHLD ignored again (set between `fork` and `exec`) when the caller ignored it. Each handled signal's handler writes to a pipe read by a thread of its own; the runner starts every reader before it installs the first handler, because a handler whose reader never ran would swallow its signal. It sets them up at its start, before any other thread exists, with the handled signals blocked, and unblocks them once every handler is installed: `signal-hook` installs a handler before it publishes the action that handler runs, and drops a signal that arrives in between, while a blocked signal stays pending until it is unblocked. The readers keep the signals blocked, so the handlers run on the runner's other threads. When a reader or a handler cannot be set up, the runner installs none, each signal gets back the disposition it had before the runner unblocks it, and the run fails open (`signal_setup`) with nothing forwarded: the stage shares the caller's group and gets terminal signals itself, and a SIGINT, SIGTERM, SIGHUP, or SIGQUIT that reaches the runner ends it with its default action. A stage that survives the signal then runs on without the in-use lock, which goes with the runner. For the same reason the runner cancels nothing when a SIGHUP comes with its parent's exit (a hangup, or the kernel's orphaned-group SIGHUP to a stopped runner, below); a parent exit without a signal is cancelled as on the other caller-group paths.
- **Job control:** on the fail-open and TTY paths, when the child stops, the runner stops itself with the same signal, so the caller's job control sees the stage stop, and continues the child once it is continued itself. A runner that never stopped itself would hide the stage's stop from the caller's job control. On these paths the runner signals the child by its PID and never signals the caller's group.
- **Parent death:** the runner watches its parent from its start and reads the parent PID itself before the spawn and before its wait ends. If the parent exits before the spawn (the Bash call was cancelled), the runner releases what it holds and exits 129 without starting the stage. After the spawn on the admitted path, it sends SIGTERM to the group, also when the child has exited and only descendants are left, and waits until no member is left. If members remain 5 s later, it records the job, leaves the reservation for the recovery pass, and sends SIGKILL to the whole group, itself included while it is a member; the reservation is released once the group is empty. Only a live runner signals: the group's ID is its PID, so the ID cannot pass to another group while it lives.
- **Parent death in the caller's group:** on the fail-open and TTY paths the runner holds no reservation (TTY passthrough bypasses admission, and fail-open drops what it reserved); what it holds until the child is gone is the in-use lock. After the spawn the child gets SIGTERM, and SIGKILL 5 s later, by its PID, which stays reserved because the runner reaps the child only when its wait ends; the runner waits for it at most 30 s. This cancellation progresses while the runner executes. A runner stopped with its stage (job control above) runs none of its threads, so it sees its parent's exit only once it is continued, and the cancellation waits until then. When the parent's exit leaves the caller's group orphaned, the kernel sends every member SIGHUP and then SIGCONT, which continues the runner; while another member keeps the group from becoming orphaned, nothing does. The kernel's continuation does not cover a runner that found its parent alive just before and stops right after it: the runner then stays stopped with the in-use lock, and the stage, continued by the same SIGCONT, is not guaranteed to stay stopped. In launchd's session on macOS (session ID 1), when a runner that launchd adopted exits, the kernel can send SIGHUP and SIGCONT to a stopped member of the caller's group that is not part of the stage, even one whose parent in another group of the session keeps the group from becoming orphaned; those signals come from the kernel's orphaned-group handling, not from cardinal. cardinal does not cover these cases with a helper process: a signal to the caller's group reaches its unrelated members, a helper in that group is stopped with it, and a helper outside it cannot pin the runner's PID, which could pass to another process between the helper's check and its signal.
- **Normal exit:** when the direct child exits, the runner returns its status at once. It deletes the reservation only when no member of the group other than itself is left: it moves back into the caller's group and asks `kill(-pgid, 0)`, or reads the process table when it cannot move. Otherwise the reservation stays for the recovery pass, which releases it once the group is empty, so the budget and the worktree stay taken while the stage's descendants run. Those members are not killed (the same as running the stage without cardinal) and show up in `inventory`. Processes that leave the group (`setsid`) are not tracked.
- **Windows:** the hook does not rewrite (section 6.3) and `cardinal run` always takes the fail-open path; the argument contract and Job Objects are phase 3 (section 17, item 10).

### 5.6 Exit status and infrastructure failure

| Situation | Exit | Added output |
| --- | --- | --- |
| Child exited with code N | N | none |
| Child killed by signal N | signal N: after releasing, the runner restores N's default disposition and raises it | none |
| Pre-start failure, TTY passthrough | the stage's status or signal | none |
| Runner error after the child started, child status unknown | 125 | `[cardinal] Infrastructure failure after the command started: <reason>. The command was not rerun.` on stderr |
| Runner killed after the child started | set by the killer (137 for SIGKILL) | none; the record has `job_started` without `job_finished`, and `explain job` reports `runner_died` |
| Release or record failure after the child exited | the child's status | none |

125 follows the convention of `env`, `nice`, and `timeout` for "the wrapper itself failed". It can collide with a child's own 125, so the stderr line and the records are the distinguishing signal. The line appears only when the stage's own status is unknown, so it never changes the output of a command whose result is known.

### 5.7 Executor contract (phase 2 seam)

```rust
pub trait Executor {
    /// Sets up the process group; runs before the reservation is written.
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

The local executor is the only implementation. The start boundary is the same for every executor: `starting` is written before `start`, and an `Ok` from `start` means the stage may have run. For phase 2's accepted jobs, the accepting runner starts a detached runner, which takes its own live lock and a shared in-use lock and rewrites the reservation's owner to itself before the accepting runner drops its locks and prints the job ID; both hold the locks for that moment, so the job is never unowned.

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
| How is `if … then … else … fi` matched against permission rules? | Each branch separately, in one-line and multi-line form, after `cd … &&`, inside pipelines and lists, and with two wrapped stages: with an allow rule for the runner, the outcome equals the original's. Other fallback shapes (`&&`/`\|\|`/`;` chains, `{ …; } \|\| …`, `test -x … && exec …`) prompt even when every part is allowed. `"ask"` always prompts; `"allow"` runs commands the original's rules would not. In `auto` mode the classifier denied a rewritten command whose original stage no rule allowed, while allowing the original | Checked against Claude Code 2.1.322 |
| Do assignment prefixes before the runner match its rule? | No: `FOO=1 R run …` prompts under `Bash(R run:*)`; only known variables such as `LANG` are stripped | Checked against Claude Code 2.1.322 |
| Several hooks returning `updatedInput` | Not documented; hooks run in parallel | Hooks reference |
| Hook timeout | `timeout` in seconds; default 600 for command hooks; a timed-out `PreToolUse` hook is discarded and the call continues through the normal permission flow | Hooks reference |
| Exit codes and output | Exit 2 blocks the tool call; other non-zero exits are non-blocking; exit 0 with empty stdout is success; exit 0 with invalid JSON is a non-blocking error | Hooks reference |
| `SubagentStop` input | `agent_id`, `agent_type`, `agent_transcript_path`, `stop_hook_active`, `last_assistant_message`, plus common fields | Hooks reference |
| `SessionEnd` input | `reason`: `clear`, `resume`, `logout`, `prompt_input_exit`, `other`; default hook timeout 1.5 s | Hooks reference |
| Shell and cwd of Bash commands | The user's shell, with aliases, functions, and options captured from its rc file at session start; a `cd` carries over to later Bash calls while inside the project directory (never in subagents) | Tools reference |
| Windows | The Bash tool runs in Git Bash; without Git Bash, a separate PowerShell tool runs commands | Tools reference; [Troubleshooting install](https://code.claude.com/docs/en/troubleshoot-install) |

### 6.2 Installation

The owner adds this to `~/.claude/settings.json` (never to a repository, G5); `examples/claude/settings.json` ships it:

```json
{
  "hooks": {
    "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "cardinal hook claude pre-tool-use", "timeout": 5 }] }],
    "SubagentStop": [{ "hooks": [{ "type": "command", "command": "cardinal hook claude subagent-stop", "timeout": 5 }] }],
    "SessionEnd": [{ "hooks": [{ "type": "command", "command": "cardinal hook claude session-end", "timeout": 2 }] }]
  }
}
```

The explicit `timeout` keeps a stuck hook from stalling Bash calls for the 600 s default. Set `[hook] runner` to a path that survives upgrades (a vanished runner path silently turns every rewrite into the fallback). No permission rule ships: approval stays with Claude Code (section 6.5). cardinal must be the only hook returning `updatedInput` for Bash; the order of several is undefined.

### 6.3 `hook claude pre-tool-use`

1. On Windows (section 5.5), exit 0 with empty stdout at once, before reading the input or the configuration, so the hook records nothing there. Elsewhere, read stdin (bounded at 1 MiB) as JSON. Exit 0 with empty stdout, rewriting nothing, if `tool_name` is not `Bash`, `tool_input.command` is not a string, `hook.enabled` is false, `permission_mode` is `auto` (section 6.5), or anything fails.
2. Find and classify stages (section 7) within `hook.deadline_ms`, measured from process start. The deadline is checked after classification and again after the output is serialized, right before it is written; past it: no rewrite.
3. If at least one stage is classified, rewrite each classified stage (section 6.4) and print:

```json
{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"command":"<rewritten>","description":"<unchanged>","timeout":120000,"run_in_background":false}}}
```

`updatedInput` is the original `tool_input` object with only `command` replaced, kept as a `serde_json::Value` so fields unknown to cardinal survive unchanged. No `permissionDecision` is returned (section 6.5).

4. The effective timeout written into each spec: `tool_input.timeout` if it is a positive number, else `BASH_DEFAULT_TIMEOUT_MS` from the hook's environment (the hook inherits Claude Code's), else 120000; capped at `max(BASH_MAX_TIMEOUT_MS or 600000, default)`. `run_in_background: true` sets the spec's background flag.

On the way to a rewrite the hook writes nothing to the state dir and takes no reservation (C1). A configuration error, a runner path that is a directory included, appends `config_error`; input the hook cannot use (unreadable, not a JSON object, over 1 MiB, or a Bash call without a string command), a missed deadline, or another failure appends `hook_error` with reason `invalid_input`, `deadline_missed`, or `failed`. It never exits non-zero: panics are caught at the top level and turned into "no rewrite".

### 6.4 Rewritten command

The hook replaces only the span of each classified stage, in place, and leaves every other byte of the command alone. For a stage `S`, the replacement is

```sh
if [ -f R ] && [ -x R ]; then S'; else S; fi
```

where `R` is the runner path, the `else` branch is the stage as written, and `S'` is `S` with `R run --spec-b64 <spec> --` inserted immediately before its command word. Example:

```sh
# original
cd packages/app && pnpm test 2>&1 | tail -50 || echo failed
# rewritten
cd packages/app && if [ -f /usr/local/bin/cardinal ] && [ -x /usr/local/bin/cardinal ]; then /usr/local/bin/cardinal run --spec-b64 eyJzY2hlbWEiOi… -- pnpm test 2>&1; else pnpm test 2>&1; fi | tail -50 || echo failed
```

- **Everything else stays in the calling shell:** pipes, `&&`/`||`/`;` lists and their short-circuiting, a leading `cd` and the cwd it leaves for later Bash calls, shell options such as `pipefail` and `errexit`, functions used by other stages, and the command's first word unless its first stage is classified. Claude Code's timeout handling depends only on a leading `sleep`, and a `sleep` stage is never wrapped (section 7.2).
- **Both branches carry the stage's own words,** so variables, globs, `~`, and quoting are expanded by the calling shell either way, and exactly one branch runs. The `if` returns the status of the branch that ran, so pipeline and list semantics are unchanged.
- **`[ -f R ] && [ -x R ]`** is the fallback test: if the runner path is missing, not a regular file (a directory passes `-x`), or not executable (uninstalled, moved, upgraded to another path), the `else` branch runs the original stage. The hook also refuses a directory runner path as a configuration error (section 6.3).
- **Several classified stages** in one command are each wrapped and reserved separately; the spec's stage index tells them apart. Scripts started inside a stage are not controlled separately; their weight is learned for the stage (design §6).
- **One-line form:** a wrapped stage's span never contains a newline or a comment (section 7.2), so the replacement stays on one line and the rest of the line follows `fi` unchanged.

In bash, zsh, and sh this form gives the original's stdout, stderr, and status for lists, `pipefail`, `errexit`, non-exported variables, globs, and redirections, with the runner present, missing, or a directory; `tests/rewrite_equivalence.rs` keeps these cases (section 13.5). Two kinds of stage are exceptions, and section 7.2 leaves them unwrapped:

- a stage that redirects a stream its pipeline connects (`cmd > log | tail`, `x | cmd < file`): zsh's `MULTIOS` sends such a stream to both the redirection and the pipe only while both belong to the same simple command, and in the rewrite the pipe belongs to the `if`;
- a stage that runs in the calling shell, alone or last in its pipeline, where a status it does not produce can decide `errexit`: a later command of its and-or list (`cmd && false`), or with `pipefail` an earlier stage of its pipeline (`false | cmd`). Once an `if`, like any compound command, has run in the calling shell, zsh no longer applies `errexit` to either, so the failure would not end the shell. A stage that feeds a pipe runs in a subshell, which keeps this to itself.

The fallback test and the runner are commands of the calling shell, so a trap or tracing that is active when the command starts (a `DEBUG` trap or `xtrace` from the owner's shell startup) sees them, the way an rc alias that shadows a tool changes what the runner runs (section 7.5); both are outside this equivalence. A command that sets a trap or tracing itself is not rewritten (section 7.2).

### 6.5 Permissions

cardinal installs no execution grants and delegates approval to Claude Code. The rewrite returns no `permissionDecision`, and Claude Code evaluates its permission rules against the rewritten command, splitting `if … then … else … fi` into its branches for matching (section 6.1). In `default` and `acceptEdits` modes a rewritten Bash call therefore needs one approval per call, one for all its stages, including commands that were allowed before rewriting; unattended sessions without an approval surface refuse them. `bypassPermissions` and `auto` modes are unchanged: the first runs the rewritten call as it runs the original, and the second is not rewritten (below). Deny rules on the original still match its copy in the `else` branch. Returning `"allow"` would run commands the owner never allowed.

Accepting Claude Code's suggested persistent rule `cardinal run *`, or any rule that matches a runner prefix, re-opens a way around the owner's rules: an agent could run any command directly as `R run -- cmd`, whatever the rules for `cmd` say. Such a rule must not be used. Bash permission rules match the invocation text and are not a program-level boundary. The G4 real-host check repeats this section for the Claude Code version in use.

**`auto` mode is not rewritten,** because the auto-mode classifier may deny a stage's rewritten form while allowing the original; commands in `auto` mode bypass admission. Lifting this needs cardinal to evaluate the owner's permission rules (to rewrite only stages an allow rule covers), or Claude Code to report the original call's permission decision to hooks.

### 6.6 `subagent-stop` and `session-end`

Both read stdin, append one record (`subagent_stopped` or `session_ended`, with `session_id`, `agent_id`, `agent_type`, `reason`, `cwd`), and never stop processes (C9). Both exit 0 whatever happens.

`session-end` also writes `sessions/<session>.json`: the agent process (the nearest ancestor of the hook whose name is not a shell: `sh`, `bash`, `zsh`, `dash`, `cmd.exe`, `powershell.exe`, `pwsh.exe`) and every descendant of it except the hook's own chain, each as PID, start time, name, and the first 256 bytes of the command line. It must finish within the 1.5 s default timeout; one process-table refresh takes milliseconds.

A process outlived its session when it is listed in a snapshot, is still alive with the same start time, and the agent process is gone. The agent check matters because `SessionEnd` with `clear` or `resume` keeps the same agent process running, and it is why the hook cannot raise the item itself: the sweep runs in the inbox refresh, which every `reconcile` pass runs (section 10.3). Processes that detach from the agent before `SessionEnd` (double-forked daemons) are not in the snapshot; a process-environment marker can be added if the owner needs them.

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

The hand-written tokenizer accepts a conservative subset of POSIX shell syntax: words with `'…'`, `"…"` (backslash escapes inside), and `\x` escapes; plain parameter expansions inside words, `$NAME` and `${NAME}`, also with a literal integer subscript, which evaluates nothing (`${NAME[0]}`, `${NAME[-1]}`, zsh's `$NAME[1]`), and positional and special parameters (`$1`, `${10}`, `$#`, `$@`, `$*`, `$?`, `$-`, `$$`, `$!`, `$_`); the operators `&&`, `||`, `;`, `|`, `&`, and newlines; redirections (`>`, `>>`, `<`, `2>&1`, `N>file`, `&>`); and `#` comments.

**The whole command is left unrewritten** when a stage span cannot be delimited with certainty or the outer shell could change what a stage means: a here-document or here-string, `$(`, backticks, `$((`, `$[`, `$'…'`, `<(`, `>(`, `(`, `)`, `{` and `}` outside `${NAME}`, `[[`, `!` outside `$!` and `${!}`, `|&`, a line continuation, a keyword in command position (`if then else elif fi for while until do done case esac function select`, and bash's and zsh's `coproc repeat foreach end`), a function definition, or unbalanced quotes.

**Every other expansion also leaves the whole command unrewritten,** in any stage and redirection target, quoted or not: `${…}` with an operator, a subscript other than a literal integer, substring, length, flag, or indirection (`${x:-y}`, `${a[i]}`, `${a[@]}`, `${x:1}`, `${#x}`, `${(e)x}`, `${!x}`, `${x@P}`), zsh's other subscripts and modifiers after an unbraced parameter (`$a[i]`, `$a[1,2]`, `$x:h`; `$PATH:/bin` is plain) and its `$#name`, `$=name`, `$~name`, `$^name`, and `$+name`, and `$"…"`. An operator can evaluate arithmetic that reads what the fallback test changes (`"${UNSET:-$[PIPESTATUS[0]]}"`, `"${text:PIPESTATUS[0]}"`), and in an earlier stage it runs in the calling shell, where it can assign the shells' function or command-path tables (`: ${functions[probe]:=cd sub}`, `: "${a[functions[probe]=0]}"`, `: "${BASH_CMDS[probe]:=/bin/echo}"`); either way a wrapped `probe` would differ from the stage as written.

**A command that reads the `-c` string is left unrewritten:** `BASH_EXECUTION_STRING` and `ZSH_EXECUTION_STRING` hold the whole command, which the rewrite of any stage changes (`echo "$BASH_EXECUTION_STRING" | wc -c; probe x`). This holds for either expanded in any word or redirection target, and for either named in a word the shell reads as a name (below: `declare -p BASH_EXECUTION_STRING`). A listing of every variable (`set`, `declare -p` without names) also shows the string, and there it shows the rewritten command; such a command is still rewritten, because the listing is a diagnostic and nothing the command runs depends on that line.

**The whole command is also left unrewritten when one of its stages can change what a later stage means or trace it,** because a later wrapped stage would then run something else than the same stage unwrapped, or its fallback test and runner would show in the trace. Command words are read after quote removal (`"eval"`, `\eval`) and behind precommand modifiers (`command`, `builtin`, `exec`, `noglob`, `nocorrect`, `-`, `time`). The command is refused for:

- a command word that is not a literal: one with an expansion (`$TOOL`, `"$X"`), an unquoted pattern character (`*`, `?`, `[…]`, and zsh's `^`, and `#` or `~` after the word's start), or an unquoted leading `~` (`$HOME`, `$PWD` as `~+`, `$OLDPWD` as `~-`, or a named directory), which can name any command, `eval` among them (`HOME=eval; ~ 'probe() { cd sub; }'`), or with `nullglob` vanish and leave its arguments to run in the calling shell (`./absent*/tool cd sub`); in arguments, `~` stays allowed;
- a builtin that defines, removes, or redirects commands or sources code into the calling shell: `eval`, `.`, `source`, `alias`, `unalias`, `unset` (bash falls back to functions), `functions`, `unfunction`, `autoload`, `enable`, `disable`, `hash`, `unhash`, `rehash`, and the `f` option of `typeset`, `declare`, `local`, `export`, `readonly`, `integer`, `float`, and `private`; zsh's `emulate` with anything after its mode (`-c CODE` runs code, and its flags set options as `set` does);
- the shells' tables of functions, aliases, command paths, options, builtins, reserved words, and parameters (`functions`, `aliases`, `galiases`, `saliases`, `commands`, `options`, `builtins`, `reswords`, `patchars`, `parameters`, the `dis_` variants, `functions_source`, `BASH_ALIASES`, `BASH_CMDS`), and array elements, whose subscript is arithmetic that can assign anything: an assignment word to either (`functions[probe]=…`, `a[i]=…`); a name argument that is not a plain identifier outside the tables, for every name a builtin assigns: the names of `typeset` and its relatives above (also after `--`), `printf -v` and zsh's `print -v`, `read`, `getopts`, and zsh's `set -A`; the `n` option of those relatives but `export` (bash's namerefs, which can point a plain name at a table element); `let` with a subscript or an expansion; `mapfile` and `readarray` (whose `-C` runs a command per line), `vared`, and `zparseopts`, whichever names they get; and a table element's name (`functions[`) in a word the shell reads as a name or arithmetic, where `functions[probe]=0` defines a function: an assignment word (an integer variable's value is arithmetic) and every argument of a builtin, behind precommand modifiers too (zsh evaluates arithmetic in `printf %d`, `shift`, `[ … -eq … ]`, `exit`, and other builtins' numeric arguments). Other commands get their arguments as text, so `grep -F 'functions[' code.txt` is rewritten;
- removing the export of `PATH` (`export -n PATH`, `typeset +x PATH`, `declare +x PATH`): the shell still finds commands in its `PATH`, but the runner gets only the exported environment;
- `trap`, and options that trace commands: `set -x`, `-v`, `-T`, `set -o xtrace|verbose|functrace`, `setopt`/`unsetopt` of `xtrace` or `verbose` (zsh spellings included), and `shopt -s extdebug`; an option word with an expansion counts as one of these;
- options that change how the shell finds a command, which the runner, searching `PATH` afresh, does not follow: zsh's `PATH_DIRS` (`set -Q`), which searches `PATH` for a command word with a slash (`tools/probe`); the hashing options, which decide whether the shell runs a remembered path (zsh's `HASH_CMDS`, `HASH_DIRS`, `HASH_EXECUTABLES_ONLY`, bash's `hashall` or `set -h`, and `shopt -s checkhash`); `restricted` (`set -r`), which refuses the runner's path; and bash's `keyword` (`set -k`), which moves a stage's `NAME=value` arguments into the runner's environment. Set or unset, in any spelling zsh accepts, each refuses the command. Other options, `set -e`, `set -u`, and `set -o pipefail` among them, do not change what a wrapped stage runs and stay accepted.

**A single stage is left unwrapped** (other stages may still be wrapped) when:

- its command word is a shell builtin, keyword, or precommand modifier (`time`, `command`, `builtin`, `exec`, `noglob`, `nocorrect`, and the rest of the shell's builtins), `sleep`, or in `[classify] skip`;
- it expands, in a word or a redirection target, a parameter whose value the rewrite changes or no two runs share. The fallback test sets `$?` (zsh's `$status`), `$_` (the test's last word, `]`), `PIPESTATUS` and `pipestatus`, and, when the runner is missing, zsh's `ERRNO`; bash's `BASH_COMMAND` holds the text of the command running, which the rewrite changes. `$$`, `$!`, `BASHPID`, `RANDOM`, `SRANDOM`, `SECONDS`, `EPOCHSECONDS`, and `EPOCHREALTIME` are computed per reference from the process, a clock, or a generator, so no test can show that the rewrite keeps them. Every other plain parameter, positional parameters, `$#`, `$@`, `$*`, `$-`, and `LINENO` among them, keeps its value inside the `if`. A literal integer subscript leaves the decision to its parameter: `"${PIPESTATUS[0]}"` keeps its stage unwrapped, while the pipeline before it is still wrapped (`cmd 2>&1 | tail; echo "${PIPESTATUS[0]}"`);
- it has assignment prefixes: in front of the runner they would also set the runner's own environment, from which it finds its state dir (`HOME=/tmp/h cargo test`, section 3), and as `env` arguments they would change meaning (`A=one B="$A" cmd`, readonly variables). Phase 0 therefore does not limit stages such as `CI=1 pnpm test`;
- it runs asynchronously (followed by `&`), because its runner would outlive the calling shell;
- its pipeline contains another classified stage, because a stage waiting for admission could block a running one through the pipe;
- it redirects a stream its pipeline connects: standard output while a later stage reads it, or standard input while an earlier stage feeds it (`cmd > log | tail`, `x | cmd < file`), because zsh's `MULTIOS` copies such a stream both ways only while the redirection and the pipe belong to the same simple command (section 6.4);
- it is the last stage of a pipeline of several (`x | cmd`), or it runs alone in its pipeline and its and-or list goes on after it (`cmd && echo ok`), because zsh would then ignore `errexit` for the status of the earlier stages under `pipefail` or for the rest of the list (section 6.4). A stage that feeds a pipe is still wrapped (`cmd 2>&1 | tail || echo failed`);
- its span contains a newline (inside quotes), which the one-line form does not allow.

Not understanding a command can only cause a missed rewrite, never a changed one.

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

### 7.5 Scope of the equivalence

The rewrite's equivalence covers commands as agents write them. Four things are outside it.

**Startup state.** An alias or function in the owner's rc that shadows a classified tool's name is not applied when the stage runs through the runner, which executes the tool from `PATH`. Shadowing aliases and functions, bash namerefs (`declare -n`) from the rc, which can turn a plain parameter or name into a table element, and any other startup state (options, traps, variables; section 6.4) are outside the equivalence. The owner writes both the rules and the rc, and can keep a shadowed tool out of rewriting with `[classify] skip` (per repository, or in a generic profile for all). Phase 0 does not detect shadowing; a later check could run the owner's shell once and report every rule command word that resolves to an alias or function.

**The calling shell's command cache.** A stage as written leaves its command's path in bash's hash table (zsh's command table); a wrapped stage leaves the runner's absolute path, which is not cached. A later stage that reads the cache sees the difference (`probe; type probe` reports `probe is hashed` only as written), and a later stage of the same command can resolve a name differently when a file in `PATH` appears or moves while the command runs. Effects on the cache created during the command are outside the equivalence; options that change how the shell uses the cache are refused (section 7.2).

**The macOS text encoding variable.** On macOS, CoreFoundation, which cardinal links, sets `__CF_USER_TEXT_ENCODING` in the runner before `main`, so a stage sees the system value even when the caller exported another; every CoreFoundation-linked process (Claude Code, and any such tool the stage runs) does the same, and no command's meaning depends on it.

**Inputs built to make the rewrite differ.** The rules of section 7.2 recognize the forms in which commands read or change what a wrapped stage depends on, not every construction that reaches the same state at run time: a table name assembled inside arithmetic from plain parameters (`t=functions; printf '%d' "${t}[probe]=0"`, `declare -i n; n="${t}[probe]=0"`, and the same through `shift`, `let`, or `printf -v`) passes them. Such a command can make a wrapped stage behave differently from the stage as written, but the rewrite grants no execution rights (section 6.5, design §6): the agent host still decides whether the rewritten command runs, so the difference gives the agent nothing it could not run directly. The rules stay conservative, refusing what they do not recognize, and are not extended to such constructions.

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

- **Constructed environment for git:** start empty and copy an allowlist: `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_*`, `TMPDIR`, `XDG_CONFIG_HOME`, `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM`, `GH_TOKEN`, `GITHUB_TOKEN`, `GH_HOST`, `GH_CONFIG_DIR`, `SSH_AUTH_SOCK`, `CARDINAL_TEST_RUN` (the test harness's run marker, section 13.4), and on Windows `SystemRoot`, `SystemDrive`, `ComSpec`, `PATHEXT`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `ProgramData`, `TEMP`, `TMP`. Every other `GIT_*` (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY`, `GIT_NAMESPACE`, `GIT_CEILING_DIRECTORIES`, `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`, …) is dropped, so a cardinal process started from a git hook can never act on another repository. Then add `GIT_OPTIONAL_LOCKS=0`, `GIT_NO_LAZY_FETCH=1`, `GIT_TERMINAL_PROMPT=0`, `GH_PROMPT_DISABLED=1`, `GH_NO_UPDATE_NOTIFIER=1`, `NO_COLOR=1`. The `GIT_CONFIG_*` location variables stay because they decide where the owner's configuration lives, and git must read it the way the owner's git does (design §9).
- **Constructed environment for gh:** built the same way from its own allowlist: `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_*`, `TMPDIR`, `XDG_CONFIG_HOME`, `GH_CONFIG_DIR`, the tokens gh authenticates with (`GH_TOKEN`, `GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN`, `GITHUB_ENTERPRISE_TOKEN`), `CARDINAL_TEST_RUN`, and the Windows variables above, plus the same added variables. `GH_HOST`, `GH_REPO`, and every `GIT_*` are dropped, so gh asks exactly the host and repository observation names (section 8.2). The caller's `GH_HOST` only decides which origin hosts count as GitHub.
- **No lazy fetch:** in a partial clone, a promisor repository (`extensions.partialClone`, `remote.<name>.promisor`, or `remote.<name>.partialCloneFilter` set; git registers a promisor remote from the filter even where `promisor` is false), git fetches a missing object from the promisor remote when a command needs it, writing a pack into the repository: `git diff --name-only` against the base in a `--filter=tree:0` clone did (probed with git 2.54). With `GIT_NO_LAZY_FETCH=1` (git 2.45.1 and later) such a command fails instead (exit 128, nothing written): a failed step, whose facts are `unknown`. Older git ignores the variable, and 2.45.0 reads it but still fetches in the inexact rename detection that `status` runs on a staged rename (the check moved into the promisor fetch itself in 2.45.1), so observation reads `git --version` once and, below 2.45.1, runs no command that reads objects, `status` included, in a promisor repository, which it tells from the configuration git reads where each command runs: the base in the directory observation entered, each worktree's commands in that worktree, whose `config.worktree` can name a promisor the repository's configuration does not; those facts are `unknown`.
- **Always** `git --no-optional-locks -c core.fsmonitor=false -C <absolute path> …`, so `status` does not refresh and rewrite the index, and a configured filesystem monitor does not start a daemon that writes inside `.git`. No filter driver runs either: observation first lists the drivers the worktree's configuration defines (`git config --name-only --get-regexp '^filter\.'`, which reads configuration only) and passes each one's `clean`, `smudge`, and `process` as empty and `required` as false (`GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>` pairs, which carry any driver name). `status` runs a tracked file's clean filter whenever the file's index entry needs a content check (after a same-size edit, a touch, or a checkout that left the entry racily clean), and a filter is the owner's program, free to write: Git LFS's clean filter stores the file in `.git/lfs/objects`, and a filter that writes into the git dir ran under `--no-optional-locks` (probed with git 2.54). Without its filter, such a file is compared raw with its cleaned blob, so status reports it modified whether or not it changed (section 8.2). Two residuals remain. A driver configured between the listing and `status` still runs: that takes a filter installed within a fraction of a second of a pass, and what runs is the owner's own filter writing its own cache, as the owner's next `git status` would, so nothing of cardinal's is created and no data is lost; observation lists the drivers again after `status`, and a changed list fails that worktree's observation (`filters_changed`). And a filtered file whose worktree bytes equal its cleaned blob reads as unchanged; ordinary edits do not produce such bytes (an LFS pointer left in the worktree is unchanged either way, and a git-crypt user would have to type ciphertext), and those bytes are in git as that blob. Reading a split index still freshens its shared index file, so a worktree that uses one (a `sharedindex.*` file in its git dir or the common dir, or `core.splitIndex` set) is not read with any command that reads the index: its status and tracked files are `unknown`, and the step counts as failed. Every git child runs in that directory, and gh in the directory of the repository or worktree it is asked about, so no child inherits cardinal's working directory. `observe::git` runs git this way for observation, and the hook runs its one git command, `remote get-url origin` (section 3.3), the same way. Observation never runs `fetch` or anything else that writes to a repository; stale remote-tracking refs only make fewer worktrees look merged.
- **Deadlines and output:** git and gh run through `os::child`; `os::child::git_env` and `os::child::gh_env` build the two environments above, which stay separate. Every child has an absolute deadline (`git_timeout_ms`, `gh_timeout_ms` from its start; the hook's git ends at the hook's deadline, section 6.3) and reads at most 256 MiB from each of stdout and stderr (the hook's git 64 KiB). Output is read on threads; on the deadline the fact is `unknown`, and a stream over the cap makes it `unknown` at once. A child that exits while a descendant still holds its output open (a tool that leaves a helper holding its output) ends the same way on every platform: the run waits for end of file until the deadline, so it ends as a timeout, not success. On Unix each child leads its own process group; on Windows it starts suspended, is assigned to its own Job Object, and only then resumes, so every process it starts is in the job. Every end of a run, a normal exit included, kills what is left of the group or the job before the child is reaped (on Unix the child stays unreaped until then, so the group's ID cannot pass to another group; before its first child cardinal sets SIGCHLD to its default action, because with SIGCHLD ignored, as a caller can pass it on across `exec`, the kernel reaps children itself), so no descendant that is still in the child's process group or job outlives the run. A descendant that called `setsid()` has left the group and is not killed; a reader thread blocked on a pipe it holds open outlives a timeout, so `os::child` serves only short-lived processes (CLI commands and the hook), whose exit ends such threads. A cardinal process that is terminated takes its live children along. On Unix a command that runs children first installs handlers for SIGTERM, SIGINT, and SIGHUP (each unless ignored; `os::child::prepare`, or else the first child does) that kill every live child's group and then end the process by the same signal with its default action, so a process that forwards these signals itself does not use `os::child`. Each handler's reader thread starts before any handler is installed; if setting them up fails, every signal gets its previous disposition back, no child starts, and the step fails, as does every later step of the process that runs a child. While the handlers go in, the three signals are blocked on the installing thread, and so on the reader threads it starts: signal-hook installs its OS handler before it publishes the action that handler runs, and a signal in between would reach a handler that does nothing with it. Blocked, the signal stays pending until the handlers are in place (or the previous dispositions are back) and then reaches them. A signal sent to the process meanwhile can still reach another thread that does not block it and be lost there, so `cardinal inventory`, `plan`, and `act`, the commands that observe, install the handlers before they start any thread. Resetting SIGCHLD and installing these handlers change process-wide signal state, which cardinal owns: it is a command line program, and its library serves only its own binary and tests (section 2.1). On Windows each child's job kills its processes when its last handle closes, and cardinal holds the only handle, so the processes in it end when cardinal ends, by any means. The child enters its job in the call after the one that creates it suspended; a cardinal ended between the two leaves at worst that child suspended, a process that never ran. Creating it inside its job (`PROC_THREAD_ATTRIBUTE_JOB_LIST`) would need a hand-written `CreateProcessW`, because std does not expose that attribute. A process cardinal starts otherwise (such as a detached runner, section 5.7) is in no such job and outlives it. Spawn failure, non-zero exit (where not meaningful), unparsable output, and timeouts all become `unknown`, never "safe". A failed git or gh step of a worktree or its repository also makes that worktree's `observed` check fail, even when other evidence answers the fact.

### 8.2 Repositories and worktrees

Repositories to observe: the main worktrees of profiles with `match.paths`, plus `[observe] repos`, plus `--repo` arguments. Per repository:

| Fact | Command |
| --- | --- |
| Worktrees | `git worktree list --porcelain -z` (path, HEAD, branch, `locked`) |
| GitHub repository | `git remote get-url origin`, parsed to `HOST/OWNER/NAME` for `github.com` (or the caller's `GH_HOST`); anything else makes PR facts `unknown`. JSON shows `owner/name`; gh always gets `--repo HOST/OWNER/NAME` |
| Base | `[git] base`, else `git symbolic-ref --quiet refs/remotes/origin/HEAD`, then `git rev-parse --verify --quiet <full ref>^{commit}`. The base is always a full ref, never revision syntax (`:path` would read the index): a configured base must pass the ref-name rules of `git check-ref-format` and is looked up as `refs/remotes/<base>`, then `refs/heads/<base>` (as itself when it starts with `refs/`); `origin/HEAD` must point under `refs/remotes/origin/`. Exit 1 (absent) makes merge evidence `unknown`; an invalid name, any other status, a signal, or output that is not such a ref or an object ID is a failed step (section 8.1) |
| Open PRs | `gh pr list --repo HOST/O/N --state open --limit 500 --json number,headRefName,headRefOid,isCrossRepository,url`; exactly 500 results makes it `unknown` |

Per worktree:

| Fact | Command | Value |
| --- | --- | --- |
| HEAD, branch, upstream, dirty, ignored entries | `git status --porcelain=v2 -z --branch --ignore-submodules=all --untracked-files=all --ignored=matching` | `dirty` is `yes` for any entry other than `!` (ignored), except a worktree-side modification (`.M`) of a path whose `filter` attribute names a configured driver (`git check-attr -z filter -- <path>…`, run only when a driver is configured, the paths as arguments, since `os::child` closes a child's stdin, in batches whose command line fits: on Windows within the 32,767 characters of `CreateProcessW`, or the 8,191 of cmd.exe when `[observe] git` names a batch file (`.bat`, `.cmd`), which the standard library starts through cmd.exe with each `%` escaped; 64 KiB of paths elsewhere; a `set` value counts where a driver named `set` is configured, because check-attr also prints `set` for a bare attribute), which cannot be told from an unchanged file (section 8.1): when only such entries are present, `dirty` is `unknown` with the reason `filtered_paths` and up to 20 of the paths, kept with the facts (`dirty_unknown`, section 11.3) and in the `clean` check's detail and facts. That is not a failed step: it fails `clean`, while a `reclaim_dir` plan in the worktree, which does not depend on `dirty`, keeps `observed` when the facts it needs are known. Status never enters a submodule, whose own index could be split (section 8.1); a populated gitlink fails `no_nested_repository` anyway |
| `has_upstream` | `# branch.upstream` header | detached HEAD: `no` |
| `unpushed` | `git rev-list --count HEAD --not --remotes` | `0` → `no`; otherwise `yes`, unless merge evidence is `merged_pr` (the commits exist on GitHub) |
| `base_ancestor` | `git merge-base --is-ancestor HEAD <base>` | exit 0 → `yes`, 1 → `no`, else `unknown` |
| `merged_pr` (only if not a base ancestor) | `gh pr list --repo HOST/O/N --state merged --search <HEAD oid> --json number,headRefOid,mergedAt,url --limit 20` | `yes` if a returned PR has `headRefOid == HEAD`; a full page of 20 without one is `unknown` |
| `open_pr` | from the repository's open PR list | `yes` if a same-repository PR has `headRefName == branch` or `headRefOid == HEAD` |
| `closed_pr` (only for a worktree that is dirty or has no merge evidence) | `gh pr list --repo HOST/O/N --state closed --head <branch> --json number,state,headRefOid,url --limit 20`, then `git merge-base --is-ancestor <headRefOid> HEAD` for each | the newest PR closed without merging (`state == "CLOSED"`) whose head is HEAD or an ancestor of it (a head missing locally is not); a full page of 20 is `unknown` |
| `content_in_base` (only when merge evidence is `none`) | `git merge-base HEAD <base>`, then `git diff --name-only -z --no-renames <merge base> HEAD` (the paths the branch changed) and `git diff --name-only -z --no-renames <base> HEAD` (the paths that differ from the base) | `yes` when no path is in both lists: every file the branch changed has the same content and mode in the base, as after a squash merge; otherwise `no`, with up to 20 of the paths in `content_diff`. Tree-to-tree diffs read no index |
| `git_dir` | `git rev-parse --absolute-git-dir` | the worktree's administrative entry, `<common dir>/worktrees/<id>` for a linked worktree |
| `last_activity_ms` | `lstat` of the file `git rev-parse --git-path index` names | the index's modification time: commits, checkouts, staging, and status refreshes by other tools (cardinal's own status writes nothing) |
| Tracked protected data, gitlinks | `git ls-files -z --stage` | a tracked path covered by a protected pattern fails `no_protected`; a gitlink (mode 160000) whose directory has its own `.git` entry fails `no_nested_repository`, with or without `.gitmodules` |

In a repository that keeps files in Git LFS, `dirty` is therefore `unknown` only while an LFS file's index entry needs a content check: after a checkout (every filtered file of a fresh clone did, probed with a filter that stores files as Git LFS does) until a git command of the owner or an agent refreshes the index, or after a tool touched the file. A worktree an agent has worked in usually has its index refreshed by the agent's own git commands and stays known; section 14.4 counts the others. Making `dirty` `unknown` for every repository that configures a filter driver would instead leave an LFS repository with no worktree ever reported.

Merge evidence is `base_ancestor` or `merged_pr`, matched on the HEAD object ID, never on the branch name. So commits added after a merged PR, or a reused branch name, give no evidence (C12). A squash merge through a pull request whose head is the worktree's HEAD is `merged_pr`. Other squash merges are left to the arbiter (design §7): `content_in_base` is the fact it judges from, and counts as evidence only when the arbiter or the owner passes `--squash-merged` to `act`, which reads the fact again (section 9.3).

### 8.3 Ignored data

Each `!` entry from `git status` is classified with the profile's `[data]` patterns:

1. `protected` if the entry, a directory it is inside, or anything below it (found by walking the entry) matches a protected pattern. Protected patterns match at any depth, a pattern with `/` as that sequence of components, so `fuzz/corpus` also protects `target/a/b/fuzz/corpus`; every module matches through `config::data_pattern_covers`;
2. else `nested_repository` if a directory at or below the entry has its own `.git` entry;
3. else `regenerable` if the entry is, or is inside, a regenerable match (a regenerable pattern with `/` is worktree-relative);
4. else `unclassifiable`, also when the walk could not read part of the entry.

An entry that matches a regenerable pattern keeps that match (`regenerable_match`) whatever its class, so a `target/` holding `fuzz/corpus` is `protected` and still gets a plan, which its failing check keeps ineligible and names (section 9.2), and which the disk report counts as blocked (section 10.4).

Walks never follow symbolic links and never cross into a directory that contains its own `.git` entry.

### 8.4 Managed worktrees

A worktree is managed by another app when its path is under a `[observe] managed` root or git lists it as `locked`. The `managed_by` field names the app or `git-lock`. Managed worktrees and everything inside them are only reported (design §4); `act` refuses both plan kinds for them (section 9.2).

### 8.5 Disk usage

`inventory --sizes` walks each worktree and its regenerable entries (threads per worktree, no symlink following). It reports `allocated_bytes` (blocks × 512 on Unix, file length on Windows) and `aliased_bytes`, the part held by files with links outside the entry (an `st_nlink` above the names the walk found), which deleting the entry does not free: a `node_modules` hard-linked from a package store is mostly aliased. APFS clones are not accounted for. Without `--sizes`, size fields are `null`. Free space per volume is the disk state of section 10.4, which `inventory` reads from `disk/` without measuring: it is as of the last `reconcile` pass, with its reading times.

### 8.6 Processes

`inventory` lists reservations with their derived state and the members of their process groups, read as a snapshot under the admission lock (section 4.3), outlived processes from session snapshots (section 6.6), and the arbiter's state (section 10.5). Inventory is read-only: it never deletes stale reservations or signals anything.

## 9. plan, act, and invariants

### 9.1 Plan kinds

| Kind | Target |
| --- | --- |
| `reclaim_worktree` | A linked worktree (never a main worktree) |
| `reclaim_dir` | An ignored entry inside an observed worktree that matches a regenerable pattern (section 8.3) |

Phase 1 executes neither: `act` evaluates a plan and reports it (section 9.3), and section 9.7 lists what executing either kind requires. Caches outside worktrees (the pnpm store, sccache), process stops for outlived processes, and branch deletion are not plan kinds. A recovery-needed reservation is never released by a plan; it is released only when its group is empty (section 4.5).

### 9.2 Checks

Each plan carries a list of checks. An **invariant** failing or being `unknown` makes `act` refuse for every caller, including the owner (design §3). A **condition** failing only makes owner approval necessary (section 9.3).

| Check | Kinds | Role | Pass when |
| --- | --- | --- | --- |
| `observed` | both | invariant | Every git and gh fact the plan depends on is known |
| `not_in_use` | both | invariant | The in-use lock is not held (probed, section 4.6), no live or recovery-needed reservation names the worktree, and no process other than the reader has its working directory inside the worktree. The whole worktree counts in `reclaim_dir` too: a dev server, a language server, or an open agent session started there uses its `node_modules` and `target/`. Open descriptors are not read (section 9.7, requirement 5) |
| `not_managed` | both | invariant | The worktree is not under a managed root and not git-locked (design §4) |
| `clean` | `reclaim_worktree` | invariant | `dirty` is `no` |
| `no_unpushed` | `reclaim_worktree` | invariant | `unpushed` is `no` |
| `no_open_pr` | `reclaim_worktree` | invariant | `open_pr` is `no` |
| `has_upstream` | `reclaim_worktree` | invariant | The branch has an upstream (G6). This also keeps freshly created worktrees, which start clean at the base, from being reclaimed before their first push |
| `regenerable` | `reclaim_dir` | invariant | The target is an ignored entry of a worktree that observation listed and matches a regenerable pattern; whether everything in it is regenerable is for `no_protected`, `no_unclassifiable`, and `no_nested_repository` |
| `plain_target` | both | invariant | The target is not a symbolic link, not a mount point, and on the same device and mount as its parent, and no directory below it is on another device or mount, so the walk classified and measured all of it. A bind mount on the same device counts as a mount point: the mount is told by its mount ID (`statx`) on Linux and by the path it is mounted on (`statfs`) on macOS; a mount that cannot be read is `unknown`. `plan`'s walk and `act`'s snapshot read mounts the same way (`os::mount`), for the target and every directory they would enter, and list a directory only after its mount is read. On Windows a folder a volume is mounted on is a reparse point, which both treat as a symbolic link |
| `no_protected` | both | invariant | Nothing in the target, descendants included, is protected |
| `no_unclassifiable` | both | invariant | Nothing in the target is unclassifiable ignored data |
| `no_nested_repository` | both | invariant | No directory below the target's root has its own `.git` entry (another worktree, a submodule, a nested clone) |
| `merge_evidence` | `reclaim_worktree` | condition | `base_ancestor` or `merged_pr` is `yes`, or the caller passed `--squash-merged` and `content_in_base` is `yes` (section 9.3) |

Bulk `cardinal plan` and step 5 of a `reconcile` pass make a plan for every linked worktree that observation lists and for every ignored entry of every worktree that matches a regenerable pattern, whether or not its checks pass: a failing plan carries its reason (G6, G7, G10), and a worktree without merge evidence has the plan `merge_undecided` needs (G8). `plan` evaluates every check on the target; `act` evaluates them again at run time, walking the whole target (section 9.5). Plans are created even when invariants fail, with `invariants_ok: false`, so the arbiter sees the reason and an `act` on them demonstrates the refusal (G10). A `merge_evidence` that fails while `content_in_base` is `yes` says so in its detail and facts, which is how the arbiter finds the plans it may judge.

`plan`'s walk records the target and every entry below it, parents before children in name order, without following symbolic links, with one directory open at a time. A directory whose recorded device is another than the target's is recorded and never listed. Otherwise the walk reads the directory's mount by path: on Linux by `statx` of the path, which does not open the directory, and on macOS by opening it (`O_NOFOLLOW`) for `fstatfs`, which identifies it without reading any of its entries. A directory on another mount than the target is recorded and never listed; such a directory, or one on another device, fails `plain_target`. To list a directory, the target included, the walk opens it with `O_DIRECTORY | O_NOFOLLOW` and lists it only when that descriptor has the `dev` and `ino` the walk recorded for the directory and the target's mount, read from the descriptor (`statx` with `AT_EMPTY_PATH` on Linux, `fstatfs` on macOS); it then lists through that descriptor and reads each entry's metadata relative to it (`fstatat` without following links). Every directory the walk lists is therefore the one it recorded, on the target's mount, and every entry below the target is read from a directory so listed: a directory replaced by a symbolic link, by another directory, or by a mount after the walk recorded it is not listed. A target that fails this is reported as a mount boundary, which fails `plain_target`; a directory below the target that fails it stays recorded as the walk saw it, with nothing below it. An entry removed before the walk reads its metadata is left out, and a directory removed after that stays recorded as the walk saw it, with nothing below it. The target itself is reached by path, so a directory above it replaced after the walk checked the path for symbolic links is not detected by the walk; `act` reaches the target through descriptors from the worktree root (section 9.5). On Windows the walk lists by path.

### 9.3 Approval scope and reports

Approval is needed when a condition fails; otherwise none is needed. This is the design's reclaim scope (design §5): without owner approval, the arbiter can reclaim regenerable data and whole worktrees that meet every reclaim condition. **`act` computes approval from the checks it evaluates at run time**; the `approval` stored in the plan is informational and never widens what `act` may do. A plan that needs approval is `rejected` (`owner_approval_required`) unless `--owner-approved` is given, which `act` accepts only together with `--caller owner` (a usage error otherwise). The arbiter skill never passes it. It is self-regulation, not a security boundary (design §3): the arbiter's allow rule for `cardinal act` would let its session pass any flag, and what that could skip is a condition, never an invariant. Phase 3 replaces the flag with an approval intent bound to the plan ID from the web UI.

**Squash-merge judgment.** `--squash-merged` (callers `arbiter` and `owner`) says the caller judged HEAD squash-merged. `act` then counts merge evidence as present only when `content_in_base` is `yes` in its own observation at its checks (section 9.5, step 4); otherwise `merge_evidence` fails as without the flag. The flag can therefore turn only "every file the branch changed is identical in the base" into evidence, and every invariant still holds: the worktree is clean, its commits are on a remote, and its branch is kept (section 17 item 13). In phase 1 the judgment changes a report only: the plan reports `dry_run` instead of `rejected` (`owner_approval_required`).

**No execution in phase 1.** `act` executes no plan, whatever the caller, flags, approval, or configuration. A plan whose invariants pass and whose approval is met (none needed, or `--owner-approved` from the owner) reports `dry_run` with `execution_not_designed` in `reasons`; every other evaluation reports `rejected` or `expired` (section 9.5). The reason names what is missing, a design (section 9.7), rather than a grant, flag, platform, or setting a caller could change, and it is the same for both plan kinds, so the arbiter, the records, and the page read every `dry_run` of phase 1 alike. `--dry-run` is accepted and changes nothing. `[unit.<name>] act` keeps its phase 0 meaning (C16): while it is false the unit's runs are recorded as `planned` naming `unit.<name>.act`, and while it is true as `act` records; both report. A plan that needs approval is `rejected` (`owner_approval_required`) without `--owner-approved`, so `dry_run` always means "would reclaim" (G3's report part).

### 9.4 Identity binding

A plan stores the target identity: `host_id`, repository common dir, worktree path, worktree root identity (`dev` and `ino` on Unix; creation time where the OS has one), the worktree's git dir (section 8.2), HEAD object ID, branch, and target path. `act` re-reads all of them; any difference makes the plan `expired` (C14), and a new worktree at the same path has a new root identity. The target entry is not bound to the plan (a package manager may have recreated `node_modules`): `act` reports on the entry its snapshot finds. Binding an execution to that entry and to the directories above it is a requirement of section 9.7.

### 9.5 act procedure

`act` reports in these steps on every platform. It takes no lock but the admission lock, to read reservations, and the records lock, to append its record, and writes nothing else.

1. **Load.** Load the plan; a wrong `host_id` gives `expired`.
2. **In-use facts.** Probe the worktree's in-use lock, never taking it or waiting, so a report never makes a command wait, and read the reservations naming the worktree under the admission lock.
3. **Snapshot.** Open the target from the worktree root through directory descriptors, each relative to its parent without following links (`openat` with `O_DIRECTORY | O_NOFOLLOW`), and walk it as `plan`'s walk does (section 9.2). A tree deeper than the open-file limit fails the snapshot (`EMFILE`), and the checks that read it fail with it. On Windows the snapshot walks by path without following links.
4. **Observe.** Re-observe the worktree from scratch (section 8): the local git facts, and for a worktree plan gh and, with `--squash-merged`, `content_in_base`. Compare the identity of section 9.4, the worktree root as the snapshot opened it included, and for a worktree plan that it is not the main worktree; a difference gives `expired` with `identity_changed:<field>` reasons.
5. **Check.** Read the processes whose working directory is inside the worktree and evaluate every check of section 9.2 on these facts and the snapshot: an invariant not `pass` gives `rejected` with the failed checks, a failed condition without approval `rejected` (`owner_approval_required`), and otherwise `dry_run` with `execution_not_designed` and the snapshot's bytes in `freed_bytes`.
6. **Record.** Append the record of section 9.6 and print the result.

git reads the tree after the snapshot, so a change made before the snapshot is in what git checked. The snapshot and the identity checks describe what `act` observed; they do not exclude a change made meanwhile, which is one reason executing needs a design of its own (section 9.7).

### 9.6 Dry-run and gating

Every `act` of phase 1 is a dry run (section 9.3). A unit whose `act` is false appends a `planned` record naming the configuration key instead of an `act` record (C16); every other run appends an `act` record. `disk-reclaim`'s automatic report is not an `act`: it appends `planned` records from the pass's checks whatever its unit's `act` (section 10.4). Both carry the status, the checks with their facts, and the provenance of section 10.2. A dry run writes nothing to the plan's directory or inside the repository and reports the same each time it is repeated on unchanged facts.

### 9.7 Requirements for executing reclamation

Executing a plan is not designed in this document. A design of its own (#33, for regenerable directories) must meet the requirements below before `act` executes anything; each says what must hold and what has been measured, not how. Until then `act` reports `execution_not_designed`, and `plans/<plan>/claim`, `plans/<plan>/result.json`, and `<state>/reserve` stay reserved names that nothing creates (section 4.1). Section 14.5 lists enabling execution as pending that design.

**What is deleted is what was checked**

1. **The target and the directories above it.** An execution is bound to the entry its checks read and to the directories from the worktree root to its parent, by `dev` and `ino`. A moved directory keeps its identity, and a rename through a descriptor held on it still moves the target: probed on macOS, with the target's parent moved into `fuzz/corpus`, which is protected.
2. **Each deleted entry.** An entry is deleted only while it is the inode that was checked and only while it is inside what the execution owns. Unlinking by name after checking an entry's metadata deletes a file renamed over that name in between (probed), and an atomic save puts a new file, possibly the only copy of unrelated data, exactly there. Descriptors held on a tree follow it when it is moved out of the deletion area, so a deleter that walks through them deletes wherever the tree now is, inside `fuzz/corpus` included. Renaming each entry into a hold directory before its check closes the first window at 2.2 times the deletion time (20,000 files: 0.77 s against 1.73 s).
3. **Git's classification for the whole execution.** An entry's class can change while the entry does not: a `.gitignore` saved after the checks turns `! target/` into `? target/draft` with the same inode, size, and mtime, so no comparison of metadata sees it. The ignore rules the classification read (each `.gitignore` on the path, `info/exclude`, `core.excludesFile`) must be bound to the checks, or the classification established again once nothing can change the target.
4. **Later writes and later use.** Live deletion and recovery both keep what was written, created, or brought into use after the checks: a file appended to, protected or unclassifiable data, a nested repository, entries a new protected pattern covers, and entries a process started using.

**Users of the target**

5. **Processes the in-use facts miss.** Section 9.2 reads the in-use lock, reservations, and working directories. A process can also hold a target through open descriptors, which the user's own processes expose (probed on macOS: 648 processes read in 55 ms for 8,192 vnode descriptors), while other users' processes and, on Linux, non-dumpable ones cannot be read. Other ways in leave no descriptor: a reader blocked opening a FIFO (probed on macOS), a writer through a shared mapping whose descriptor it closed, a watcher (FSEvents, inotify), a lookup stalled across a move, a reopen by file identity, a descriptor passed over a socket or opened after a scan, and container and VM file servers. Files with links outside the target (a package store's files hard-linked into `node_modules`) are open in every process that runs a package and are not the target's to free. For each way in, the design detects it or names what a miss costs, and the owner accepts that cost (section 14.5).
6. **Special files and file systems.** A FIFO or a socket in a target is where processes meet, so deleting one can strand a process, and a device does not belong in build output. A mount below a target moves with it when the target is renamed (probed on APFS and Linux). Processes on other hosts or behind a FUSE server are invisible to any local check, so network and FUSE file systems need a rule; a VirtioFS share or a bind mount into a container is the host's own file system, and its guests' processes are invisible too.
7. **Jobs that start meanwhile.** A runner whose wait cap ends runs without the in-use lock (G4, section 4.6), so the lock cannot exclude it. A job that starts during an execution finds the target whole or absent, never partly deleted, or fails visibly.

**Rollback, recovery, durability, and space**

8. **Recovery and rollback.** An execution that dies is finished or undone later by another process. Before it deletes anything, recovery establishes the in-use checks again (requirement 5) as well as each entry's inode, size, and mtime, so it keeps what requirement 4 keeps. A rollback stays bound to the target's original parent: when that parent moved, the name is occupied, or a restore did not complete, the data stays where it can be found, and its place is tracked until the plan expires. Entries staged away from the target keep their mapping to their originals across a crash, and a restore counts as done only once the directories it changed are durable.
9. **A claim never blocks a retry.** A claim written by creating its file and then writing it (`create_new`, as `state::fs::create_new_durable` does) is left empty or short by a crash or a full disk, and every later attempt then finds a claim it cannot read. A claim appears complete or not at all, and an unreadable one has a defined outcome.
10. **Space on a full volume.** Progress on a full volume covers each volume the execution writes to, the target's and the state dir's, judged on its own: a claim write can succeed while the rename on the target's volume still fails. Working space is released only when a write needs it, right before that write, since space released before cardinal observes it can be taken by another writer. C11 asks cardinal to complete minimal reclamation on a full volume; executing and the disk reconciler do not depend on each other in a cycle, and on APFS a local snapshot can keep the freed blocks (section 17 item 15).
11. **Starters.** Which starters exist (the owner, the arbiter, a reconciler, recovery) and where they run is the design's. Each has its own rule for what it may delete, with which checks, and how long it may hold `reconcile.lock`, and a recovery and an execution of the same plan never overlap. An execution driven by disk pressure measures free space again after every change it makes, a partial success included, and stops at the recovery line or at a reading it cannot take. Deleting adds to the latency of section 10.4: on one APFS host about 7.5 s for 110,000 entries, after about 1 s for a snapshot and a comparison.
12. **Power loss.** Each record an execution writes is durable only after the change it describes, and each change only after the record that lets recovery undo or finish it. Killing a process tests the recovery decisions, not this order, since the page cache survives it.

**Who executes, and when**

13. **Grants.** Which callers may execute and how each is enabled. A grant agrees with what its caller does: the arbiter skill only reports (section 14.3), so an arbiter grant waits for a skill that executes, and a unit's grant names what the unit deletes. Enabling is per caller and per host, after the evidence of section 14.4.
14. **Windows.** Moving and deleting through handles, and files that running programs hold open (design §2); phase 3.

**Whole worktrees** (section 17 item 17)

15. **An idle state that can be enforced.** For a regenerable directory a missed process costs a rebuild; for a worktree it loses work. Removing one needs a state in which nothing may use it, entered and checked through a mechanism every user of the worktree honours, not inferred from a scan.
16. **The worktree's ID serialized against git.** A linked worktree's `.git` file names its administrative entry, `<common dir>/worktrees/<id>`, and git gives a free ID to the next worktree created with the same directory name. Moving or deleting the entry frees the ID while the worktree still points at it: probed with git 2.54, a worktree created meanwhile took the ID, and `git add` in the first worktree staged into the new worktree's index. A `git worktree prune` can likewise remove an entry that is restored later. The entry's removal and any restore are serialized with git's own creation and pruning of worktrees.
17. **Per-worktree git state classified as disposable.** A clean, pushed HEAD does not mean nothing of value is left: the worktree's HEAD reflog, its per-worktree refs (`refs/bisect/`, `refs/worktree/`), `config.worktree`, and an operation in progress (a rebase, merge, cherry-pick, or bisect) live in its administrative entry. A commit made on a detached HEAD and left by switching back is reachable only from that reflog, which deleting the entry destroys. Each of these is positively classified as disposable, not merely absent from `git status`.

**Implementation notes.** Deleting in place from the top (`rm -rf`, `remove_dir_all`) lets a job see a half-deleted tree and binds nothing to the checks; a no-replace rename (`renameat2` with `RENAME_NOREPLACE` on Linux, `renameatx_np` with `RENAME_EXCL` on macOS) takes a tree out of every path in about 1 ms; the user's Trash copies between volumes and frees nothing until emptied; and a trash directory inside a worktree is a file cardinal creates in a repository (G5). Descriptors are read through `proc_pidinfo` and `proc_pidfdinfo` on macOS and `/proc/<pid>/fd` on Linux; a durable write on macOS needs `F_FULLFSYNC`; Windows moves and deletes through handles opened with `FILE_FLAG_OPEN_REPARSE_POINT` and renamed with `FileRenameInfo`.

## 10. Inbox, reconcile, and explain

### 10.1 Inbox

Items are raised through `inbox::raise`: by the refresh that `cardinal inbox` runs first (skipped with `--no-refresh`) and that every `reconcile` pass runs, by `reconcile` from its full observation and its disk states, and by the arbiter launcher. The refresh first runs the recovery pass of section 4.4 under the admission lock, so recovery does not depend on another job being admitted; it then raises and clears. Every raiser clears only items it read before it started observing, and only while they are unchanged (checked under `inbox/.lock`), so it never closes an item that another raiser raised or updated from a newer observation:

| Kind | Key | Raised when (by) | Cleared (resolved by `cardinal`) when |
| --- | --- | --- | --- |
| `recovery_needed` | `recovery_needed:<job>` | A reservation is `recovery_needed` (evidence: group members, verified or not) (refresh) | The reservation is gone |
| `outlived_processes` | `outlived:<session>` | A session snapshot has live processes after its agent process ended (refresh) | None of them is alive; the session snapshot is then deleted |
| `reclaimable` | `reclaimable:<worktree>@<head>` | A whole-worktree plan passes every check without approval (reconcile; evidence: the plan ID) | The worktree is gone or its HEAD changed |
| `merge_undecided` | `merge_undecided:<worktree>@<head>` | Every invariant of a whole-worktree plan passes and `merge_evidence` fails (reconcile; evidence: `content_in_base` with `content_diff`, `closed_pr`, the plan ID) | The worktree is gone, its HEAD changed, or merge evidence appeared |
| `closed_pr_dirty` | `closed_pr_dirty:<worktree>#<pr>` | The worktree is dirty and its branch's pull request was closed without merging (reconcile) | The worktree is gone or clean |
| `managed_merged` | `managed_merged:<worktree>@<head>` | A managed worktree (section 8.4) has merge evidence, is clean, and is not in use; only its app or the owner can remove it (reconcile) | The worktree is gone or its HEAD changed |
| `unclassifiable_data` | `unclassifiable:<worktree>:<set>`, `<set>` the FNV-1a of the sorted paths of its unclassifiable ignored entries | A worktree has unclassifiable ignored data, or part of its ignored-data walk failed (reconcile; evidence: up to 50 paths with their sizes when known, and the observation errors) | The worktree is gone, or its unclassifiable entries are no longer this set: none are left, or a different set raises its own item and supersedes this one |
| `disk_low` | `disk_low:<volume>` | A watched volume leaves `ok`, entering `warning` or `critical` (reconcile, after `disk-reclaim`; evidence: the volume's disk report, section 11.3, updated by every pass under pressure) | The volume is back to `ok` |
| `disk_critical` | `disk_critical:<volume>` | A watched volume enters `critical`, from `ok` or `warning` (reconcile, after `disk-reclaim`; evidence as for `disk_low`) | The volume leaves `critical` |
| `arbiter_failing` | `arbiter_failing:<pass>` | Three arbiter passes in a row failed, timed out, or were abandoned (launcher, trigger; evidence: the passes and their logs) | A pass completes |

Statuses are `open`, `escalated` (waiting for the owner), `resolved`, and `dismissed`. `<worktree>` is the worktree's path; `<volume>` is the key of section 10.4. An item about a worktree also names, in `subject`, the HEAD and branch it was raised for. Without `--status`, `cardinal inbox` lists open and escalated items. `cardinal inbox resolve` records the resolution with `by: "cli"`, cardinal's clearing with `by: "cardinal"`.

**Occurrences.** Each key in the table is followed by `@<since_ms>`, the time of the observation that found the condition, so an item is one occurrence of a condition; what else makes a recurrence new is in the key too (a worktree's HEAD, a pull request's number, a set of unclassifiable entries). Raising a condition whose latest occurrence has not ended updates `seen` and evidence of an open or escalated item and leaves a resolved or dismissed one alone (C19), so a resolved item never comes back while its condition lasts. An occurrence ends when a raiser observes its clearing condition (the last column): the raiser sets the item's `ended_ms` and resolves it if it is open or escalated. A condition found again after its occurrence ended is a new occurrence and raises a new item, whatever the old one's status: unclassifiable entries that change from set A to set B and back to A raise three items.

The arbiter decides which items need the owner (section 14.3): it reports `reclaimable` items, which phase 1 does not execute (section 9.3), judges `merge_undecided` items, and escalates what only the owner can decide, such as `closed_pr_dirty`, `managed_merged`, and `unclassifiable_data` (whether each entry is regenerable or protected, a line in the repository's profile). Unclassifiable data is never deleted (design §7), so without this item it would stay unreclaimable without anyone being asked. `disk_low`, `disk_critical`, and `arbiter_failing` are for the owner directly and never start an arbiter (section 10.5): in phase 1 only the owner can free space, and a disk item already holds the evidence the owner needs.

### 10.2 explain

`explain` shows where a decision came from: configuration files and keys, the rule, observations, and records. Observations are the facts the checks read, each with its value and the tool or read that produced it (`git`, `gh`, `lstat`, the process table, …), and what could not be observed with its error; the observation module returns facts, not command transcripts, so exit codes and raw output are not kept. A plan stores them with its checks (`facts`) and in `provenance` (errors and the configuration keys used: the repository's `[data]` lists and `[observe] managed`), and every `act` record repeats them for its own evaluation. Subjects: `job <id>`, `plan <id>`, `inbox <key>`, and `command [--cwd DIR] -- <string>` (a classification and rewrite trace without running anything). Job and plan IDs resolve by unique prefix.

| Subject | `outcome` |
| --- | --- |
| `job` | `exited:<code>` or `signaled:<signal>` (from `job_finished`), `infra_failure:<reason>`, `running`, `waiting` (reserved, not started), `runner_died` (started or reserved, live lock free), or `fail_open:<reason>` |
| `plan` | The status of the latest `act` or `planned` record (`dry_run`, `rejected`, or `expired` in phase 1); null before any `act`. A gated dry run also lists `unit.<name>.act` as a configuration source |
| `inbox` | `<status> by <by>: <note>` (without `: <note>` when empty) once resolved, escalated, or dismissed; null while open |
| `command` | null. Each wrapped stage lists its rule, the pattern that matched, the normalized argv, and the profile key, from the classification module's trace; `rewritten` is the command the hook would produce, or null with the reasons it stays unrewritten in `summary` |

### 10.3 Reconcile

`cardinal reconcile [--trigger timer|job-end|manual]` runs one pass and exits; nothing stays resident between passes (design §9). What can notice a disk crossing without a resident service:

| Trigger | Cost | Reach | Verdict |
| --- | --- | --- | --- |
| The PreToolUse hook | Work on every Bash call, on the synchronous path, which must write nothing on the way to a rewrite (C1) | Every agent command | Not used |
| The runner's job end (section 5.2) | One `statfs` per job; a detached start only below the warning line | Growth that cardinal's jobs cause, when they end | Used, for disk |
| The runner while its job runs | A sampling thread in every runner | Growth during long jobs (fuzzing) | Phase 2, with measurements |
| An OS timer: a launchd agent (`StartInterval`) on macOS, a systemd user timer on Linux | One process start per interval. A pass without full observation reads a few files, the process table, and the free space (milliseconds); the full observation runs git per worktree and gh per repository, at most every `observe_every_s` | Everything, within one interval | Used: the base trigger |
| launchd `LaunchEvents` on the notify(3) key `com.apple.system.lowdiskspace` | Nothing until it fires | Only at the system's own threshold, far below the warning line | Not used; a possible backstop |
| A resident service | A process per host | Immediate | Rejected by design §9 |

The repository ships `examples/launchd/dev.cardinal.reconcile.plist` (`ProgramArguments` with the absolute path of `cardinal`, `reconcile`, `--trigger`, `timer`; `StartInterval` 300; `RunAtLoad`; `ProcessType` `Background`; `LowPriorityIO`; a `PATH` in `EnvironmentVariables` that finds git, gh, and claude; output to `/dev/null`) and `examples/systemd/cardinal-reconcile.service` and `.timer` (`Type=oneshot`, `OnUnitActiveSec=5min`, `Persistent=true`). They are installed by hand in user-level configuration, never in a repository.

A pass runs these steps in order; a failing step is recorded and the next one runs:

1. Take `reconcile.lock` exclusively with one try; when it is held, print `skipped: "busy"` and exit 0.
2. Remove old temp files.
3. Run the inbox refresh (section 10.1).
4. Read the free space of every watched volume and update its disk state (section 10.4).
5. When the last full observation is older than `observe_every_s`, or a volume is under pressure (section 10.4), observe (`inventory` of section 8, with sizes only for the entries matching a regenerable pattern in worktrees on volumes under pressure), keep the result in `reconcile/observation.json`, build the plans from it (section 9.2), and raise and clear the worktree items of section 10.1. These plans, their checks and exclusive bytes evaluated together and timestamped, are the one set `disk-reclaim` reports from.
6. For each volume under pressure at step 4, read it again; the pass's last successful reading, this one or else step 4's, updates the disk state (section 10.4), and `disk-reclaim` runs against it while that state is under pressure. Then raise, update, and end the disk items of every watched volume from its state, with the disk report of section 11.3. The trigger and the page, which come next, see the items complete.
7. Run the arbiter trigger (section 10.5).
8. Write the UI page (section 10.6) and `reconcile/state.json`.
9. Print `cardinal.reconcile.v1`.

Watched volumes are the mounts of the state dir and of each worktree in the last full observation.

### 10.4 Disk thresholds and reclamation

**Free space** is what new data can use. On macOS it is the `kCFURLVolumeAvailableCapacityForImportantUsageKey` of the path, read through CoreFoundation's `CFURL` (`objc2-core-foundation`), falling back to `kCFURLVolumeAvailableCapacityKey` when that is missing or zero, as `sysinfo` does; it includes purgeable space (local snapshots, caches the system drops when a write needs room). On Linux it is `statvfs` (`f_bavail × f_frsize`). On one host's APFS volume without local snapshots, important usage read about 10% more than `statfs` and opportunistic usage about 30% less, each query taking 15–30 ms; other hosts and volumes can differ. A read that fails is `unknown` and changes nothing: the disk state and disk items stay as they were, the failure is recorded, and `disk-reclaim` runs only against a successful reading of the pass. The runner's job-end check uses `statfs`, which read lower than important usage on that host, so there it starts a pass early rather than late; where `statfs` reads higher, the check can miss a crossing, and the timer catches it within one interval.

**Volumes and lines.** A path's volume is its mount: the mount path from `statfs` on macOS, the mount ID from `statx` on Linux (`os::mount`). `<volume>` in keys and file names is the FNV-1a of that identity. Lines come from the matching `[disk] volumes` entry, else the global lines.

**States.** Each volume is `ok`, `warning`, or `critical`, kept with the time it entered and its last 12 readings in `disk/<volume>.json`, written only under `reconcile.lock`:

- `ok` → `warning` below the warning line, or when the readings of the last hour predict the critical line within two timer intervals (free space above critical, divided by the growth rate; design §7);
- `ok` or `warning` → `critical` below the critical line, a first reading included;
- `critical` → `warning` above the critical line;
- `warning` or `critical` → `ok` only above the recovery line.

A volume in `warning` or `critical` is **under pressure**: the pass observes and `disk-reclaim` runs in both, so falling below the critical line never stops the report. Leaving `ok` raises `disk_low` for the episode and each entry into `critical` raises `disk_critical`, both at step 6 of the pass, after `disk-reclaim`, with its disk report; every later pass under pressure updates that report. Transitions take the pass's last successful reading, step 6's when it succeeds and else step 4's, so a failed reading never ends a live item. `disk_critical` ends when a reading finds the volume out of `critical` and `disk_low` when one finds it back to `ok`, so a volume that has recovered is never shown as critical. Both go to the owner directly (section 10.1). Stopping heavy jobs below the critical line is admission's, phase 2.

**`disk-reclaim`** (C10) is built in and unit-shaped. It reports from one set of facts: the plans of step 5 of the same pass, whose checks and exclusive bytes were evaluated together and carry their time, and the pass's last successful reading of the volume (section 10.3, step 6). It runs no `act` and observes nothing again; an explicit `act` on a plan is the on-demand recheck (section 9.5). For a volume under pressure it takes the `reclaim_dir` plans on that volume that passed every check without approval (`invariants_ok`, `approval: "none"`), in design §7's reclamation order after its first tier, which is whole worktrees and waits for their execution (section 17 item 17): worktrees by `last_activity_ms`, oldest first, and within a worktree the most exclusive bytes first (`allocated_bytes` less `aliased_bytes`, section 8.5); a target whose bytes are all aliased frees nothing and is skipped. It lists targets in that order until their exclusive bytes would lift that reading above the recovery line, and when all of them would not, it reports the shortfall, the bytes still missing. It appends a `planned` record for each listed target, with the checks and bytes it used. A target a job starts using after the checks stays listed until the next pass finds it in use; the evidence carries the time of the checks. Data in use and protected data are skipped because their plans fail `not_in_use` or `no_protected`; each entry on the volume that matches a regenerable pattern but whose plan failed is reported in the disk report's `blocked` with its worktree, its path, its first failed check, and its bytes, as is a target whose bytes are all aliased, so the owner sees how much space is held and by what. Caches outside worktrees and old toolchain outputs have no plan kind yet.

**One order.** Only `disk-reclaim` evaluates `reclaim_dir` plans for disk pressure. The arbiter never acts on them on its own (section 14.3): a live worktree's regenerable data is reclaimable only when its volume is under pressure, in the order above, because outside pressure no goal needs it gone and each deletion costs a reinstall or a rebuild.

**Latency.** A crossing is noticed at the end of the job that crossed it or at the next timer pass. A pass that finds `reconcile.lock` held exits `busy` (section 10.3), and the pass holding it reports on a volume only when its own reading at step 4 found pressure there. A crossing just after that reading is reported by the first pass that starts after the running one ends: at most the rest of the running pass, one timer interval, and the length of the reporting pass, so one interval and two pass lengths. A pass is bounded by its deadlines (lock waits, and git and gh per worktree and repository, section 8.1) and by its observation with sizes; executing would add its deletions (section 9.7, requirement 11). The warning line leaves room for the growth over that bound; section 14.4 measures a pass under pressure, and the lines are set from it (section 14.5, section 17 item 14).

### 10.5 Starting the arbiter

Only `reconcile` starts the arbiter, at step 7 of a pass, by starting `cardinal arbiter run` with `os::spawn_detached`; the pass does not wait.

**Due items.** Each inbox item records the last pass it was assigned to and how that pass ended (`arbiter` in `cardinal.inbox-item.v1`), written durably with the item. An open item is **due** when no pass has been assigned it, or when `[arbiter] retry_s` (2 hours by default) has passed since that pass ended, whatever its outcome. An item a pass left open, because the arbiter could not finish it (an `act` that a job made `rejected`) or because it waits on something outside (a verified `recovery_needed`), and the items of a pass that failed, timed out, or was abandoned are therefore tried again after that cooldown, and not before. A pass starts when an item is due and `min_interval_s` has passed since the last pass started. `disk_low`, `disk_critical`, and `arbiter_failing` items are never due, being the owner's (section 10.1), and items are read under `inbox/.lock`. An item the arbiter's own commands raise (an `outlived_processes` item from the refresh its `cardinal inbox` runs) waits for the next pass after the interval: the arbiter's commands never start an arbiter, because only `reconcile` does and the arbiter's allow list includes neither `reconcile` nor `arbiter`.

**Assigned items.** Before it starts the session, the launcher records the new pass and the due items' keys in `arbiter/last.json` and then assigns the items to the pass: under `inbox/.lock` it writes `arbiter: {pass, assigned_ms}` into each, durably. The pass works on those and no others, and on their subjects only: it plans just the worktrees they name and acts only on those plans (section 14.3). The launcher names the pass in the session's environment (`CARDINAL_ARBITER_PASS`); the session reads its items with `cardinal inbox --assigned`, the open and escalated items assigned to that pass (section 11.2), and answers them with `cardinal inbox resolve KEY --assigned`, which refuses (`not_assigned`) an item not assigned to that pass, and every item once the pass has ended or been recorded `abandoned`. An item that becomes due while a pass runs therefore waits for a pass of its own, and a session left running after its pass (below) answers nothing. When the pass ends, the launcher writes its outcome and end time into the `arbiter` entry of each item assigned to it, and of no other, before it records the end in `last.json`. When the pass in `last.json` is recorded `abandoned`, its items that have no outcome get that outcome with one end time, `timeout_s` after the pass started, and items that have one keep it. An item whose pass `last.json` no longer names (it is not fsynced, so a power loss can roll it back) is treated the same way, ended `timeout_s` after its `assigned_ms`. So a launcher that dies while assigning or recording strands no item, and an item listed in `last.json` but never assigned is due at the next trigger.

**One at a time.** `cardinal arbiter run` takes `arbiter/.lock` exclusively with one try (held: exit 0, printing the running pass from `arbiter/last.json`) and checks the trigger again from the items it lists. It writes `arbiter/last.json` with a new pass ID, the start time, and the due items' keys, assigns those items to the pass, and starts `[arbiter] command` through `os::child` in a process group of its own, with cwd `arbiter/` (no repository, so no repository's instructions are loaded), its own environment with `CARDINAL_ARBITER_PASS` set, and standard output and error into `arbiter/<pass>.log`, capped at 1 MiB. The lock stays with the launcher, close-on-exec as every lock (section 4.3): a session's processes could not keep it, because a process can close descriptors it inherits, and Node closes every one but the standard three in each child it starts (libuv's `POSIX_SPAWN_CLOEXEC_DEFAULT`; probed). A pass is **running** while the launcher holds the lock, or, after the launcher died (the lock free and `last.json` without an end), until `timeout_s` after the pass started; the next trigger after that records it `abandoned`, in `arbiter/last.json` and in those of its items that have no outcome.

**Bounded.** At `[arbiter] timeout_s` (20 minutes by default) `os::child` kills the session's process group, which cannot have passed to another group because its leader is not yet reaped (section 8.1). When the pass ends, the launcher records its outcome, `completed`, `failed:<status>`, or `timed_out`, in the items assigned to the pass (under `inbox/.lock`), then in `arbiter/last.json` and an `arbiter_pass` record; three failed passes in a row raise `arbiter_failing`. A launcher killed with SIGKILL kills nothing, so its session runs on unwatched, as do processes that left the session's group (`setsid`) while the launcher lived, and a pass started after `timeout_s` may overlap them. Nothing relies on one session at a time: every command a session runs takes its own locks (section 4.3), and in phase 1 nothing executes (section 9.3), so an overlap costs model usage only.

**The command** defaults to the launch in `integrations/claude-code/README.md`: `claude -p` with a prompt that starts the `cardinal-arbiter` subagent, and an allow list of the five `cardinal` subcommands the skill uses. It runs as a subagent because a main-thread agent does not load the skills its definition lists (PR #27). Each pass costs model usage; the trigger limits passes to due items and to one per interval, and tries an item again only after `retry_s` (section 14.5).

### 10.6 Read-only web UI

The phase 1 web UI shows one host's state and answers nothing; answering pending decisions from it is phase 3 (design §12).

**What is shown** is `cardinal.snapshot.v1` (section 11.3): the host; each watched volume's disk state and readings; the worktrees of the last full observation (path, branch, managing app, facts, reclaim candidacy, last activity, and sizes when known); reservations with their derived state; open and escalated inbox items with their evidence; the last 50 `act`, `planned`, and `inbox_*` records; and the last arbiter pass with its report. Each part carries the time it was observed, so stale parts show as stale. Process command lines appear only on the local page.

**Marked for the owner**, and listed first: escalated items, with the arbiter's note as the question, and items of the kinds only the owner can act on (`disk_low`, `disk_critical`, `arbiter_failing`), a disk item with its disk report. Each mark shows the CLI command that answers it as text (`cardinal inbox resolve KEY …`; `cardinal act PLAN --caller owner --owner-approved`, labelled as a recheck that returns a report); the page has no buttons and sends nothing.

**When and by what.** Every `reconcile` pass writes it at step 8 from what it already holds, with no extra git, gh, or network call; `cardinal ui` writes it on demand and prints the page's path.

**Where it is shown:**

| Option | What it takes | Cost | To decide first |
| --- | --- | --- | --- |
| **A. Local static page** | `ui/index.html` in the state dir: one self-contained file (inline style and script, the snapshot embedded as JSON, no external resources), opened in a browser | About 300 lines in `snapshot`; nothing leaves the host | Nothing |
| B. Cloudflare Worker and one Durable Object, push only | A connector step after step 8 (`cardinal push`) sends the same snapshot with a per-host token; the Worker keeps the latest snapshot per host in a SQLite-backed Durable Object and serves the same page behind Cloudflare Access | A TypeScript Worker in this repository, an HTTPS client in the binary (or a `curl` child), a token in `host.toml`. Durable Objects with SQLite storage run on Workers Free (100,000 requests a day) or Workers Paid (from $5 a month); a push every 5 minutes is 288 requests a day per host. Cloudflare Access is free up to 50 users, with a card on file | The account, the plan, the Access application and identity provider, a hostname, per-host tokens, and what may leave the host (paths, branch names, pull request URLs, process names) |
| C. A static page on Cloudflare (Workers static assets or R2) reading a pushed JSON object | As B without the Durable Object | As B, with less code | As B |

Phase 1 builds A, which needs no account and no decision, and B once the Cloudflare setup is decided (section 14.5). The snapshot is the push payload either way, so B adds a transport and a Worker, not a contract. B is the design's control plane (design §5, §9); C saves the Durable Object but has nowhere to keep phase 3's answers. Prices are Cloudflare's published ones as of November 2026.

## 11. CLI and JSON contracts

### 11.1 Conventions

- Every JSON document on stdout is one object with a `schema` field `"cardinal.<name>.v<N>"`. Adding fields keeps `N`; removing or changing a field's meaning bumps it. Readers ignore unknown fields.
- Enums are lowercase `snake_case` strings. Tri-state facts are `"yes" | "no" | "unknown"`.
- Paths are native absolute paths (on Windows without the `\\?\` prefix). Times are `*_ms` (Unix ms) or `*_s` (process start, Unix s).
- Human-readable errors go to stderr; machine-readable ones to stdout as `cardinal.error.v1`.

| Exit code | Meaning (all commands except `run` and `hook`) |
| --- | --- |
| 0 | Success, including an `act` that reports `executed`, `partial`, or `dry_run`, and a `reconcile` or `arbiter run` that found another one running |
| 1 | Error; stdout has `cardinal.error.v1` |
| 2 | Usage error (clap), including `--owner-approved` without `--caller owner` |
| 3 | `act` reports a refusal: `rejected`, `expired`, or `already_executed` |

`run` exits as in section 5.6; `hook` always exits 0. New enum values (statuses, kinds, reasons) are additions within a schema version: readers treat a value they do not know as unknown, never as success.

### 11.2 Commands

| Command | Output |
| --- | --- |
| `cardinal run [--spec-b64 SPEC] -- COMMAND [ARG]...` | The stage's own output and status (section 5) |
| `cardinal hook claude pre-tool-use \| subagent-stop \| session-end` | Hook JSON or nothing (section 6) |
| `cardinal inventory [--repo PATH]... [--sizes]` | `cardinal.inventory.v1` |
| `cardinal inbox [--status open\|escalated\|resolved\|dismissed\|all] [--assigned] [--no-refresh]` | `cardinal.inbox.v1`; `--assigned`: the open and escalated items assigned to the pass `CARDINAL_ARBITER_PASS` names (section 10.5) |
| `cardinal inbox resolve KEY --outcome resolved\|escalated\|dismissed [--note TEXT] [--assigned]` | `cardinal.inbox-item.v1`; with `--assigned`, an item not assigned to the pass `CARDINAL_ARBITER_PASS` names, or any item once that pass has ended, is refused (`not_assigned`) and left unchanged |
| `cardinal plan [--repo PATH]...` | `cardinal.plan-list.v1`: plans for every linked worktree and every ignored entry matching a regenerable pattern, failing ones included (section 9.2) |
| `cardinal plan reclaim-worktree PATH \| reclaim-dir PATH` | `cardinal.plan.v1` |
| `cardinal act PLAN [--dry-run] [--owner-approved] [--squash-merged] [--caller arbiter\|owner\|unit:NAME]` | `cardinal.act-result.v1` (default caller `arbiter`) |
| `cardinal reconcile [--trigger timer\|job-end\|manual]` | `cardinal.reconcile.v1` (section 10.3) |
| `cardinal arbiter run` | `cardinal.arbiter-pass.v1` (section 10.5) |
| `cardinal ui` | The path of the written page as text (section 10.6) |
| `cardinal explain job ID \| plan ID \| inbox KEY \| command [--cwd DIR] -- STRING` | `cardinal.explain.v1` |
| `cardinal --version` | text |

### 11.3 Schemas

Types are written in TypeScript notation; `?` marks fields that may be absent, `| null` fields that are present but empty.

```ts
type Tri = "yes" | "no" | "unknown";
type Resources = { [name: string]: number };            // positive integers
type Check = { name: string; role: "invariant" | "condition";
               result: "pass" | "fail" | "unknown"; detail: string;
               facts?: { name: string; value: unknown; source: string }[] };  // what the check read
type Provenance = { errors: { source: string; message: string }[];
                    config: { file: string; key: string; value: unknown }[] };
type GroupIdentity = { kind: "pgid"; pgid: number; leader_start_s: number };  // leader = the runner; phase 3 adds a Job Object kind

// cardinal.error.v1
{ schema; error: { code: "state_unavailable" | "config_invalid" | "not_found" | "ambiguous_id"
                        | "invalid_argument" | "not_assigned" | "not_implemented" | "internal"; message: string } }

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
  worktree: null | { key: string; path: string; protected: boolean };  // false: in_use_unprotected
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
  disk: DiskState[];                                       // read from disk/, not measured
  arbiter: { running: boolean; last: ArbiterPass | null };
  errors: { scope: string; message: string }[] }

type Repo = {
  common_dir: string; main_worktree: string; profiles: string[];
  github: string | null;                                   // "owner/name"
  base: null | { ref: string; oid: string };
  gh: "ok" | "unknown" | "not_applicable"; gh_error: string | null;
  worktrees: Worktree[] };

type Worktree = {
  path: string; main: boolean; head: string | null; branch: string | null; upstream: string | null;
  git_dir: string | null; last_activity_ms: number | null;
  managed_by: string | null;                               // app name or "git-lock"
  facts: { dirty: Tri; dirty_unknown: null | { reason: "filtered_paths"; paths: string[] };  // up to 20 paths
           has_upstream: Tri; unpushed: Tri; open_pr: Tri;
           merge_evidence: "base_ancestor" | "merged_pr" | "none" | "unknown";
           merged_pr: null | { number: number; url: string };
           content_in_base: Tri; content_diff: string[];   // "unknown" unless merge_evidence is "none"
           closed_pr: null | { number: number; url: string };
           in_use: { value: boolean; jobs: string[] } };
  ignored: { path: string; class: "regenerable" | "protected" | "unclassifiable" | "nested_repository"; regenerable_match: boolean;
             allocated_bytes: number | null; aliased_bytes: number | null }[];
  reclaim: { candidate: boolean; failed: string[]; unknown: string[] };  // check names
  allocated_bytes: number | null };

// cardinal.plan.v1
{ schema; id: string; created_ms: number; host_id: string;
  kind: "reclaim_worktree" | "reclaim_dir";
  target: { path: string; repo: string; worktree: string; head: string | null; branch: string | null;
            git_dir: string | null;
            root_id: null | { dev: number; ino: number }; root_created_ms: number | null };
  allocated_bytes: number | null; aliased_bytes: number | null;  // aliased: held by files with links outside the target
  checks: Check[]; invariants_ok: boolean;
  approval: "none" | "owner";                              // informational; act recomputes it
  provenance: Provenance }

// cardinal.plan-list.v1
{ schema; plans: Plan[]; errors: { scope: string; message: string }[] }

// cardinal.act-result.v1
{ schema; plan_id: string; caller: string;
  status: "executed" | "partial" | "dry_run" | "rejected" | "expired" | "already_executed";  // phase 1: dry_run, rejected, expired
  reasons: string[];                                       // failing check names, "owner_approval_required", "identity_changed:<field>",
                                                           // "execution_not_designed" (every dry_run of phase 1)
  checks: Check[]; freed_bytes: number | null;             // dry_run: the snapshot's bytes
  skipped_changed: string[];                               // empty in phase 1
  error: string | null; finished_ms: number }

// cardinal.inbox-item.v1
{ schema; key: string;
  kind: "recovery_needed" | "outlived_processes" | "reclaimable" | "merge_undecided" | "closed_pr_dirty"
      | "managed_merged" | "unclassifiable_data" | "disk_low" | "disk_critical" | "arbiter_failing";
  status: "open" | "escalated" | "resolved" | "dismissed";
  subject: { job_id?: string; session_id?: string; worktree?: string;
             head?: string | null; branch?: string | null };  // a worktree item's identity as raised
  evidence: object;                                        // the observation that raised it
  created_ms: number; updated_ms: number; seen: number;
  ended_ms: number | null;                                 // the occurrence ended: its clearing condition was observed (section 10.1)
  arbiter: null | { pass: string; assigned_ms: number;     // the last pass the item was assigned to;
                    outcome: ArbiterPass["outcome"]; ended_ms: number | null };  // outcome and ended_ms set when that pass ends
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
      | "job_recovered" | "in_use_unprotected" | "tty_passthrough" | "session_ended" | "subagent_stopped"
      | "plan_created" | "act" | "planned" | "inbox_raised" | "inbox_resolved"
      | "disk_state" | "reconcile" | "arbiter_pass" | "hook_error" | "config_error";
  job_id?: string; plan_id?: string; inbox_key?: string; session_id?: string;
  data: object }                                           // kind-specific; job_finished.data has exit, signal, waited_ms, admission, measurements

// cardinal.session-snapshot.v1  (sessions/<session>.json)
{ schema; session_id: string; reason: string; ended_ms: number;
  agent: { pid: number; start_s: number; name: string };
  processes: { pid: number; start_s: number; name: string; command: string }[] }

// cardinal.disk-state.v1  (disk/<volume>.json); DiskState in inventory and snapshot
type DiskState = { schema; volume: string; mount: string;
  measure: "important_usage" | "statvfs";
  state: "ok" | "warning" | "critical" | "unknown"; since_ms: number;
  lines: { warning_bytes: number; recovery_bytes: number; critical_bytes: number };
  readings: { at_ms: number; free_bytes: number | null; total_bytes: number | null }[] }  // last 12

// DiskReport: disk-reclaim's report on one volume (section 10.4); reconcile prints it, disk items carry it, the page shows it
type DiskReport = { volume: string;
  reading: { at_ms: number; free_bytes: number };          // the pass's last successful reading
  lines: { warning_bytes: number; recovery_bytes: number; critical_bytes: number };
  checks_ms: number;                                       // when the plans' checks and sizes were evaluated
  targets: { plan_id: string; worktree: string; target: string; exclusive_bytes: number }[];  // in order
  shortfall_bytes: number;                                 // 0 when the targets reach the recovery line
  blocked: { worktree: string; target: string; check: string; bytes: number }[] };  // first failed check, or "aliased"

// cardinal.reconcile.v1  (stdout of reconcile)
{ schema; trigger: "timer" | "job-end" | "manual"; skipped: null | "busy";
  disk: DiskState[]; observed: boolean;
  disk_reports: DiskReport[];                              // one per volume disk-reclaim ran on
  inbox: { raised: string[]; cleared: string[] };
  arbiter: "started" | "running" | "not_needed" | "interval";
  errors: { scope: string; message: string }[] }

// cardinal.reconcile-state.v1  (reconcile/state.json)
{ schema; last_pass_ms: number; last_observation_ms: number | null }

// cardinal.arbiter-pass.v1  (arbiter/last.json, stdout of arbiter run); ArbiterPass
type ArbiterPass = { schema; id: string; started_ms: number; ended_ms: number | null;
  outcome: null | "completed" | "timed_out" | "abandoned" | `failed:${string}`;  // null while running
  items: string[];                                         // the keys of the items assigned to the pass
  log: string; failures_in_a_row: number }

// cardinal.snapshot.v1  (ui/snapshot.json, embedded in ui/index.html)
{ schema; generated_ms: number; host: Inventory["host"];
  disk: DiskState[];
  observation: { observed_ms: number; repos: Repo[] } | null;   // the last full observation
  reservations: { observed_ms: number; items: ReservationView[] };
  inbox: (InboxItem & { needs_owner: boolean })[];         // open and escalated; marked as in section 10.6
  records: Record[];                                       // the last 50 act, planned, and inbox_* records
  arbiter: ArbiterPass | null }
```

### 11.4 Phase 2 placeholders

These names are reserved and not registered before phase 2 (an unregistered command is a clap usage error; nothing pretends to work):

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
| `sysinfo` (`system` feature only) | Use | Process table, start times, parents, working directories, boot time on all three OSes; memory sensors later come from the same crate. Its `disk` feature is skipped for free space: on macOS it lists only browsable volumes (a `-nobrowse` disk image or a hidden volume is missing) and reports by mount point, while cardinal needs the volume of a path |
| `rustix` (`process`, `fs` features) | Use (Unix) | Safe `setpgid`, `getpgid`, `kill_process_group`, `test_kill_process_group`; `openat`/`statat`/`fstat` for the walk and the snapshot relative to directory descriptors, which never follow a symbolic link or a replaced directory; `statvfs`/`statfs` for free space; avoids raw `libc` |
| `objc2-core-foundation` (`CFURL`, `CFString`, `CFNumber`, `CFDictionary`, `CFArray` features; macOS) | Use | The important-usage capacity of the volume holding a path (`CFURL` resource properties), the measure section 10.4 uses. Already in the lock file as `sysinfo`'s binding; Foundation's `NSURL` would add an Objective-C runtime dependency for the same value |
| `signal-hook` | Use (Unix) | Forwarding and re-raising signals without hand-written handlers |
| `libc` | Use (Unix) | `sigaction` and `pthread_sigmask`, which rustix does not expose: reading a signal's disposition, so an ignored signal gets no handler; putting back the one a failed setup replaced; setting SIGCHLD to its default; blocking signals while their handlers are installed |
| `base64` | Use | `--spec-b64` encoding |
| `thiserror` | Use | Typed errors where callers branch (before or after the start boundary, config vs state) |
| `dunce` | Use | Canonical Windows paths without `\\?\`, so they compare equal to paths from git |
| `windows-sys` (`Win32_Foundation`, `Win32_Security`, `Win32_System_JobObjects`, `Win32_System_Threading`) | Use (Windows) | Telling an exited process from a running one: Windows lists an exited process until its last handle closes, and sysinfo reports every Windows process as running. `os::child`'s Job Objects: each child starts suspended (std's `creation_flags`), joins a job with the kill-on-close limit, resumes through std's `main_thread_handle`, and the job is terminated at the end of every run, so a timed-out child's whole tree goes, nothing a finished child left behind stays, and the tree ends with cardinal. Preferred over `windows` (design candidate) for its smaller compile |
| `process-wrap` 10 (`os::child`) | Skip | Its sync frontend does the suspended start and the job assignment, but creates the job without the kill-on-close limit and exposes no handle to set it, so the children of a terminated cardinal would run on |
| `fs4` | Skip | std has file locks since Rust 1.89 with the same `flock`/`LockFileEx` semantics |
| `jobserver` | Skip (phase 2) | Only needed when cardinal shares a jobserver with Cargo |
| `anyhow` | Skip | Typed errors are needed for JSON error codes anyway |
| `rand`, `uuid`, `getrandom` | Skip | `RandomState` plus exclusive creation gives unique IDs |
| `chrono`, `time`, `jiff` | Skip | All times are integer milliseconds |
| `walkdir` | Skip | `plan`'s walk must read a directory's mount before listing it (section 9.2), and `walkdir` lists every directory it reaches before yielding it (`filter_entry` and `skip_current_dir` act after that), so it would list a mount point below the target. The walk lists one directory at a time, in name order, through the descriptor it checked (`std::fs::read_dir` on Windows), and closes it before listing anything below it |
| `ureq`, `reqwest` | Skip (section 10.6, option B) | Needed only for a Cloudflare push, which waits for the decision of section 14.5; until then cardinal's own code opens no network connection (gh does, for observation) |
| `ignore` | Skip | git reports ignored entries |
| `shlex`, `conch-parser`, `brush-parser` | Skip | `shlex` only splits words, and a parser such as `brush-parser` gives stage spans but not the safe-to-wrap policy, the caller-state analysis, zsh handling, or fail-open refusal, so a parser plus a whitelist would still need everything the subset tokenizer does |
| `wait-timeout`, `duct` | Skip | `os::child`, polling for the exit status (`waitid` with `WNOWAIT` on Unix, `try_wait` on Windows) with reader threads, covers deadlines and output caps |
| `tracing`, `log` | Skip | Records and `explain` are the diagnostics; hooks must not write to stderr |
| `schemars` | Skip (phase 1 candidate) | Contract types in `contract` are the source of truth; generated JSON Schema files can follow when external units appear |
| `tempfile` (dev) | Use | Scope roots in the per-run temp dir |
| `assert_cmd` (dev) | Use | Assertions on the output of the real binary; never to start it (section 13.3) |
| `proptest` (dev) | Use | Tokenizer, stage spans, rewrite round trips, JSONL torn tails |
| `rustix` (`process`, and `event` on Linux, test-support) | Use (Unix) | `waitid(WNOWAIT)` and `kill_process_group` for cleanup that signals a group only while its leader is unreaped; `pidfd_open`, `poll`, and `pidfd_send_signal` for the sweep of `run-processes`, which signals a process through a pidfd where the kernel has them. `sysinfo` does not do for the sweep: its start times have one-second resolution, and it holds no pidfd |
| `process-wrap` (`std`, `job-object`, test-support) | Use (Windows) | Starts a child suspended, puts it in a job object, then resumes it, so nothing the child starts escapes the job. Not on Unix: its process-group wrapper reaps the leader before waiting on the group and later signals the group ID, which may be reused by then. `command-group` is its deprecated predecessor |
| cargo-nextest (tool, pinned in `mise.toml`) | Use | Builds an archive of the test binaries, then runs each test in its own process through a target runner (the sandbox), with per-test timeouts and a leak check on inherited output. Its leak check misses children that closed their output and kills nothing, so `scripts/test` keeps its own sweep |
| `sandbox-runtime` (`srt`) | Skip | Same `sandbox-exec` and bubblewrap underneath, but it always denies writes to `.git/hooks`, `.git/config`, and `.gitconfig` inside allowed roots (so `git init` in a fixture fails), keeps `/tmp/claude` writable outside them, and needs Node at run time; the profile in `scripts/test` is a dozen lines |

## 13. Test strategy

### 13.1 Deterministic concurrency

Tests do not rely on starting processes at the same time. The `testpoint` module provides named pause points and failure injection, active only when `CARDINAL_TEST_DIR` is set (one cached environment read otherwise):

- `CARDINAL_TEST_PAUSE=<point>[,<point>]`: at a point the process creates `<dir>/<point>.<pid>.reached` and polls for `<dir>/<point>.release`; after 30 s it aborts so a broken test fails instead of hanging.
- `CARDINAL_TEST_FAIL=<point>`: the process returns an injected error at that point.
- `CARDINAL_TEST_PANIC=<point>[,<point>]`: the process panics at a listed point.

Points: `admission.reserved` (reservation committed, admission lock released), `admission.waiting` (budget full, about to sleep), `inuse.waiting`, `run.watching` (the runner's signal handlers about to be set up), `run.before_starting`, `run.spawning` (admitted path, last cancellation check done, about to spawn), `run.spawned` (the spawn returned, nothing about the child read), `run.after_spawn`, `run.reaped` (the stage's child reaped, its status not read), `run.finished` (the status read and `job_finished` appended, the runner's locks dropped, the job-end check not started), `run.reconcile_spawning` (the job-end check found the volume below the warning line and is about to start `cardinal reconcile`), `act.snapshotted` (snapshot recorded, local facts not read again, checks not run), `reconcile.observed` (the full observation kept, no item raised), `arbiter.locked` (the launcher holds `arbiter/.lock`, no command started), `arbiter.recorded` (the pass and its keys in `last.json`, no item assigned), `arbiter.outcomes` (the outcome in every item of the pass, the end not in `last.json`), `walk.target_checked` (`plan`'s walk found the target a directory on its parent's mount and has not opened it to list it), `walk.dir_checked` (`plan`'s walk read the mount of a directory below the target by path and has not opened it to list it), `observe.filters_listed` (a worktree's filter drivers listed, `status` not run), `hook.classified`, `record.opened` (shared records lock held, current file open, line not written), `inbox.observed` (the refresh observed reservations, processes, and sessions; nothing raised or cleared). **Every contended test waits for the second actor's `.reached` file before releasing the first**, so it proves the second actor actually hit the contended point. Examples:

- Two runners and a budget of one job: pause the first at `admission.reserved`; start the second and wait for its `admission.waiting`; release; assert the second starts only after the first's reservation is gone.
- `act` and a runner: a runner paused at `run.after_spawn` makes `act` `rejected` with `not_in_use`.
- Snapshot inspection: editing a tracked file at `act.snapshotted` gives `rejected` (`clean`) with nothing touched.
- Walk: replacing the target at `walk.target_checked`, or a directory below it at `walk.dir_checked`, with a symbolic link to a directory outside the worktree, with that directory, or (Linux) with a bind mount of it leaves the replacement's entries out of the walk.
- Records: pause an appender at `record.opened`; two appends that would each rotate leave `records.jsonl` in place, and after the release the paused record is read back with the others.
- Start boundary: a runner killed at `run.before_starting` runs the stage zero times and the Bash-level status is 137; an injected failure at `run.before_starting` or `run.watching`, a panic at `run.spawning`, or a failed spawn runs it once, after `run.watching` in the caller's group without admission; a runner killed at `run.after_spawn`, or a panic at `run.spawned` or `run.after_spawn`, never reruns it, and a panic returns its status, also when the fail-open after a panic at `run.spawning` panics at `run.spawned`. A panic at `record.opened` on every append loses the records and runs the stage once; with the records lock held until `run.spawned`, every append from the first one after the spawn panics and is lost, and the reservation stays while the stage runs. An injected failure at `run.reaped` leaves the status unknown: exit 125 with one line, and no rerun.
- Parent death: killing the runner's parent at `admission.waiting`, or at `run.before_starting` on the admitted and the fail-open path, leaves the stage unstarted and no reservation behind; killing it at `run.after_spawn` once the direct child has exited stops the descendant it left; with a grandchild that ignores SIGTERM, the reservation stays until the runner's SIGKILL 5 s later has emptied the group, and the recovery pass then releases it. On the fail-open and TTY paths, with the runner stopped with its stage: in a group that another member keeps from becoming orphaned, killing the parent leaves the runner, the stage, and an unrelated stopped member stopped, and SIGCONT to the runner alone makes it end the stage by SIGTERM within the bound and free the in-use lock, with the other member still stopped; when killing the parent orphans the group, the runner, continued by the kernel, ends a stage that ignores SIGHUP by SIGTERM within the bound.
- Signals: a stage killed by SIGTERM gives `bash -c` the same status and message with and without cardinal; a runner that already leads its group fails open in it, and SIGTERM to the runner leaves another member of that group running; a signal the caller ignores stays ignored for the stage on every path, SIGCHLD included, while the runner still learns the stage's status; a signal sent while the stage is stopped reaches it once continued; a stage that stops itself on a terminal stops the runner.

### 13.2 Real process groups

Recovery tests run real children: `cardinal run -- sh -c 'sleep 30 & sleep 30 & wait'`, pause at `run.after_spawn`, SIGKILL the runner by its PID, then assert: the reservation is `recovery_needed` and verified, `act` on the worktree is `rejected` with `not_in_use`, recovery passes leave every member running and keep counting the reservation, and once the test has ended the group (`kill(-pgid, 0)` gives `ESRCH`) the next job is admitted and the reservation released. A runner killed at `run.before_starting` leaves a `stale` reservation, and a stage whose child exited with a descendant left keeps its reservation until the descendant ends. The derived-state function is also tested against a fake process table for the cases a real system cannot produce on demand: a reused PGID with a new leader (released), a reused PGID whose leader has exited (kept, never signalled), and a child that left its descendants behind (kept, never signalled). Every child a test starts is cleaned up by the spawn API (section 13.3); a group the runner made for its stage, by a `GroupGuard`; a grandchild that reported its PID, by a `PidGuard` that checks its start time first.

### 13.3 Spawn API and git fixtures

`cardinal-test-support` is the only way tests start processes and run git, in integration tests and in `#[cfg(test)]` unit tests alike. `clippy.toml` forbids every way the dependencies offer to build or start a process everywhere else: `std::process::Command::{new, spawn, output, status}`, `assert_cmd`'s constructors (its `Command`, the `CommandCargoExt` trait on std's `Command`, and `cargo_bin_cmd!`), and `process-wrap`'s `CommandWrap` entry points; the spawn API and each product spawn site carry `#[expect(clippy::disallowed_methods, reason = "…")]`. A test that needs something the API does not offer gets it as a typed operation added to `cardinal-test-support`, never as a raw `Command` or a way to reach the one inside the API.

- `Scope::new()` creates a root under the temp dir that is outside any repository (it panics otherwise), with `home/` (and the XDG directories inside it), `tmp/`, and a git config that disables signing, hooks, fsmonitor, auto maintenance, credential helpers, and every transport (`protocol.allow=never`, `file` included).
- `Scope::command(program, cwd)` returns a `Command` built with `env_clear()` and the scope's environment: `HOME`, `XDG_*`, `TMPDIR` (Windows: `TEMP`, `TMP`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`), and `GIT_CONFIG_GLOBAL` inside the root, `GIT_CONFIG_NOSYSTEM=1`, `GIT_CEILING_DIRECTORIES` at the root's parent, `GIT_TERMINAL_PROMPT=0`, `LC_ALL=C`, fixed commit identity and dates; `PATH`, `CARDINAL_TEST_RUN`, and on Windows the variables programs need to start come from the test process. `cwd` must resolve inside the root, and the `Command` offers no way to change it or to restore the inherited environment. Variables a test needs it sets explicitly with `env`. `get_program`, `get_args`, `get_envs` (the whole constructed environment), and `get_current_dir` read what was built. `cmd` must start with `/D`; `Scope::shell` runs `sh -c` or `cmd /D /C`. On Unix a child holds no descriptor of the test process but the standard streams it is given and the scope's marker (below): between fork and exec the child marks every other descriptor from 3 up close-on-exec (`close_range` on Linux 5.11 and later, otherwise each one up to the highest the test process holds), so one that whatever started the run left open (such as a CI runner's sockets) stops at the test, and the test process's own descriptor flags never change.
- Each child's cleanup slot is registered before it starts. On Unix the child leads a new process group, or with `Command::new_session` a new session (`setsid`) whose group it leads; when it exits (seen with `waitid(WNOWAIT)`, which leaves it unreaped; a stopped child has not exited), when `kill` is called, or when its `Child` or the `Scope` is dropped, the group is sent SIGKILL and then the leader is reaped. The group is signalled only while its leader is unreaped, so its ID cannot belong to anything else. A child started with `Command::join_group(&leader)` runs in the group another unreaped child leads, as a shell without job control runs a command; it is killed and reaped alone, and the group goes with its leader. On Windows the child runs in a job object it joined before it ran (`process-wrap`), and the job is terminated at the same points. A process that leaves its group or job is caught by the sweep of section 13.4.
- On Unix every child holds its scope's marker: `Scope::new` creates `.cardinal-scope` in the root and opens it close-on-exec at the first free descriptor from 200, above the small numbers scripts redirect and shells keep for themselves, and between fork and exec each child clears close-on-exec on it, so the child and its descendants hold it unless they close it. A child also keeps every marker the test process inherited, so the children of a helper carry the scope that started the helper too. The marker is identified by device and inode, not by path. The guards below own exactly the processes that hold it: macOS lists a process's descriptors with `proc_pidinfo(PROC_PIDLISTFDS)` and reads each file's identity with `PROC_PIDFDVNODEPATHINFO`, and Linux reads `/proc/<pid>/fd` after opening the process's pidfd. A process that moved into the root does not hold it, and one that left the root still does. The environment would not do as a marker: macOS hides it for platform binaries such as `/bin/sh` and `/bin/sleep`, even from their owner, and most children are platform binaries. A process that closed every descriptor, the marker with them, is left to the end-of-run sweep (section 13.4), which finds it by the run's sandbox on macOS and the run marker on Linux. Windows has no marker: a job object holds every child, `LeftoverGuard` selects the processes working inside the root, and `PidGuard` goes by start time alone.
- For processes the spawn API does not clean up by itself, and for terminal input, `cardinal-test-support` offers these typed operations: `process::GroupGuard::new(&scope, pgid)` kills by PID every live member of a group that a started process made for itself (`setpgid(0, 0)`), for at most 2 s, and asserts that none is left; `process::LeftoverGuard::new(&scope)` does the same for every process of the scope, for at most 5 s; `process::PidGuard::new(&scope, pid)` kills a process the test did not start, by PID and start time (on Linux through a pidfd it opens when it is made), when dropped or with `kill`; `Child::kill_leader` kills a child alone and leaves its group running; `Command::output_within(limit)` runs a child to completion with one deadline for its exit and the end of its output, kills a child still running then as `Child::kill` does, and fails; `pty::Pty` opens a pseudo-terminal whose slave a child takes as stdin, without becoming anyone's controlling terminal. `Command::copy_env(&other)` gives a command every variable another sets or removes, for a shell that starts the other's program. Each guard signals only processes of its scope: `PidGuard` registers a PID only if its process holds the marker when the guard is made, and every guard reads the start time and the marker again right before the signal (section 13.4), so a process that took over a registered PID, or a group ID that names another group by the time the guard drops (the group emptied and a new process took the ID), is never signalled; start times are compared at the finest resolution the OS reports, microseconds on macOS (`proc_pidinfo`) and clock ticks on Linux (`/proc/<pid>/stat`). A test still drops a group's guard before it reaps the group's leader where it can.
- Helper processes re-run the test binary in a helper mode named by command-line arguments that `support::helper::args` builds (`support::helper::command` starts one through a scope); inherited variables never select a mode or a path. A test crate's own mode is `Mode::Custom` with one argument (a JSON request), which its `helper` test reads back with `support::helper::custom_request`.
- `Scope::mount_namespace(command, binds)` (Linux) starts the child in a user namespace and a mount namespace of its own, with each `(source, target)` of `binds` bind-mounted before its program runs; `Scope::bind_mount(child, source, target)` bind-mounts into that namespace later, from a short-lived child that joins it. Both paths must resolve inside the root. The namespaces are entered between fork and exec (`unshare`, `setns`), so the tests run them inside bubblewrap's confinement; no ID map is written, because bubblewrap's `/proc` is read-only (`unshare -r` fails there writing `/proc/self/uid_map`). Inside the namespace the child's IDs and the owner of every file show as the overflow IDs, while file access is still checked against its real IDs. Its mounts never propagate back and end with the namespace. `Scope::tmpfs(child, target, size_mib)` mounts a size-limited tmpfs into that namespace the same way, for a small volume that fills up (probed in a user namespace without privileges: writes past the size fail with `ENOSPC`, and space returns at once on unlink). That probe ran in a Linux container on a development host; whether GitHub's Linux runners let an unprivileged user namespace under bubblewrap mount a tmpfs is unverified, and F1's first CI run settles it.
- `Scope::attach_image(target)` (macOS) clones (`clonefile`) an empty APFS disk image that `scripts/test` creates under `R` before the tests start, attaches the clone at `target` inside the root with `hdiutil attach -nobrowse`, and detaches it (`hdiutil detach -force`) when its guard drops; the process that mounts it runs outside the sandbox, so the guard, not the sweep, ends it. Probed on one development Mac: inside the tests' sandbox `hdiutil create` fails and `hdiutil attach` of an existing image works, which is why the harness creates the image. Whether GitHub's macOS runners allow `hdiutil attach` is unverified, and F1's first CI run settles it. A test that needs it and finds no image fails where `CI` is `true` and is skipped with the reason otherwise, like the bind-mount tests.
- `Fixture` (a scope with git) offers typed operations only: `init`, `init_bare`, `commit_file`, `add`, `commit`, `worktree_add`, `worktree_add_existing`, `worktree_lock`, `worktree_remove`, `worktree_list`, `push`, `remote_add`, `remote_add_url`, `remote_set_url`, `push_remote`, `branch`, `switch`, `detach`, `set_upstream`, `tag`, `update_ref`, `symbolic_ref`, `symbolic_ref_delete`, `split_index`, `stash_push`, `config_set`, `config_get`, `fsmonitor`, `fsmonitor_running`, `rev_parse`, `show_ref`, `for_each_ref`, `log`, `status`, `partial_clone`, `clean_filter`. Each builds its own arguments; names must be plain (no leading `-`, no `..`), paths must resolve inside the root with symbolic links followed as the OS would, a remote URL (`remote_add_url`, `remote_set_url`, for an `origin` that observation only reads) must be `https://` with a plain host and path, and `config_set` takes keys of the `branch`, `cardinal`, and `test` sections only. `fsmonitor` sets `core.fsmonitor=true` in one repository's local config, so a test can show that observation starts no filesystem monitor; its guard stops any monitor daemon running for that repository and removes the setting when dropped, and a test that uses it also holds a `LeftoverGuard`, since the daemon leaves the group of the git that started it. Before git runs, `git rev-parse --absolute-git-dir --git-common-dir` in the directory must name dirs inside the root; "not a git repository" is told apart from any other failure, which panics. `Fixture::push` and `Fixture::partial_clone` (a `--filter=tree:0` clone, for the lazy-fetch test) are the only ways to move objects: both reach only a `BareRepo`, which only `Fixture::init_bare` creates, and only these two calls enable the `file` transport. `partial_clone` sets `uploadpack.allowFilter` in the bare repository's own config, because the `upload-pack` of a local clone does not read the cloning command's `-c` settings. `clean_filter` sets a `filter.<name>.clean` driver, `required`, that runs a script inside the root, so a test can show that observation runs no filter driver. `tests/fixture_guard.rs` covers the refusals (a decoy `.git` file pointing outside, links out of the root, config keys, option-shaped names).
- "Remote" repositories are bare fixtures; GitHub is a fake `gh` (a shell script on Unix, a `.cmd` on Windows) selected with `[observe] gh`, returning canned JSON or failing (exit 1, timeout, invalid JSON).
- Product-side isolation is tested too: observation runs with `GIT_DIR`, `GIT_WORK_TREE`, and `GIT_INDEX_FILE` exported to point at a decoy fixture, and the decoy must be untouched.

### 13.4 Test harness and repository guard

`scripts/test` is the completion check, run as `mise exec -- scripts/test` by humans, the git hooks, every work unit, and CI (on Windows `sh scripts/test`). The test harness, not each test, provides these properties:

- **Constructed environment before any startup file.** The script starts as `sh`, which reads no startup file for a script, and re-executes itself with `env -i` and `bash --noprofile --norc`. Only `PATH`, `CARGO_HOME`, `RUSTUP_HOME`, `RUSTUP_TOOLCHAIN`, `CI`, `CARGO_TERM_COLOR`, and on Windows the variables programs need to start (one list in `scripts/clean-env.sh`, which the guard shares) and those the MSVC linker lookup needs, pass; `HOME`, `BASH_ENV`, `ENV`, `GIT_*`, `GH_*`, `CLAUDE_*`, and `CARDINAL_*` do not. A per-run root `R` comes from `mktemp -d` outside any repository; `HOME` and the XDG directories point inside it from then on.
- **Build, then run confined.** Phase 1 runs `cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D warnings` (also for `x86_64-pc-windows-msvc` when that target is installed, as `mise.toml` asks), and `cargo nextest archive`. Phase 2 runs the archive with cargo-nextest in a new `env -i` environment (`HOME`, `XDG_*`, temp dir, empty `GIT_CONFIG_GLOBAL` under `R`, `GIT_CONFIG_NOSYSTEM=1`, `GIT_CEILING_DIRECTORIES` at `R`'s parent, `GIT_TERMINAL_PROMPT=0`, `PROPTEST_DISABLE_FAILURE_PERSISTENCE=1`, the run marker, and `CI` when it is set, so a test that would skip for a missing host feature fails in CI), each test in its own process with its package root as working directory and a target runner that confines it and everything it starts: on macOS `sandbox-exec` with a profile that denies every file write outside `R` except `/dev/null`, `/dev/tty`, `/dev/fd/*`, and ptys, and all network except Unix sockets under `R`; on Linux bubblewrap (read-only `/`, a fresh `/dev`, empty file systems over `/tmp`, `/var/tmp`, `/run`, `/var/run` when it is not a link, and `/dev/shm`, the checkout bound back read-only when one of them would hide it, writable `R`, no network, dies with its parent) when it works on the host, otherwise the run says it is unconfined, and a run whose checkout bubblewrap still hides fails before the tests; on Windows the environment and the guard only. A test that writes outside `R` fails on macOS and wherever bubblewrap works. A read-only bind does not stop a connect, so the empty file systems are what keep the host's Unix sockets in those places (agents, the session bus, container daemons) out of reach; a socket elsewhere, such as under the real home directory, stays reachable under bubblewrap.
- **No test outlives the run.** `.config/nextest.toml` kills a test after 5 minutes and fails one whose children still hold its output a second after it ends. After the tests, every process the run started that is still alive is printed, killed, and fails the run, including one that started a new session, closed every descriptor, or forked twice to become a daemon. `run-processes` (a binary of `cardinal-test-support`) lists and kills the run's processes, found by what they cannot shed:
  - On macOS, the sandbox. The profile also denies looking up a Mach service name unique to the run, and `run-processes` finds every process whose sandbox denies that name and allows a second one, which only this run's sandbox does. A process cannot leave its sandbox. The environment would not do here: macOS hides it for platform binaries such as `/bin/sleep`, even from their owner. A process launchd starts on a test's behalf (`launchctl`, `open`, an XPC service) runs outside the sandbox and is not found.
  - On Linux, this user's processes with `CARDINAL_TEST_RUN=<id>` in `/proc/<pid>/environ`; a process that hides its environment and appeared during the run fails the run too. A scope passes the marker to every child it starts, and cardinal passes it to its git and gh children, whose environments are otherwise built from allowlists (section 8.1), so what they leave running is found too. Only the harness sets the marker; outside tests it is unset, and passing it changes nothing.

  A PID is signalled only while it names the process that was found, so a process that exits during the sweep cannot pass the signal to another process given its PID. On Linux `run-processes` opens a pidfd before reading the environment, checks afterwards that the process has not exited, repeats the check right before the kill, and sends SIGKILL through the pidfd. On macOS, which has no pidfd, and on Linux without pidfds (before 5.3, or under an emulator that lacks them, as a container of another architecture may run), it records the process's start time when it finds it and checks membership and the start time again right before `kill`; a process that exits between that check and the signal leaves its PID free, and a new process given that PID in that interval gets the signal. The guards of `cardinal-test-support` (section 13.3) signal the same way: on Linux through a pidfd opened before they read the process's start time and its scope marker again, otherwise by PID right after that read, with the same interval. What they cannot see, a process that closed every descriptor and the scope marker with them, is this sweep's. The process table must list `run-processes` itself, so a table that lists nothing (`proc_listallpids` returns 0 when it fails) or is cut short fails the run rather than reading as "nothing left". Before the tests, a probe started in the tests' confinement proves the scan sees it. The probe reports its PID from inside the confinement, and the check waits for that process rather than the runner that starts it: bubblewrap sets its command to die with it only right before the exec, so a kill during its setup would orphan the command. Once the probe has reported, `run-processes` kills it and its runner, and the run fails if anything of it outlives the kill. The harness signals nothing else: the self-check's socket listener exits by itself once its socket is removed or the harness is gone. Each leftover is printed with its process group and command line and its parent's PID and command line. The sweep runs when the archive run ends and again when the harness exits, not after each test process: until then confinement keeps a leaked process from writing outside `R`, and the run fails if it is still alive at the end. Windows has no sweep: only nextest's leak check applies there.
- **Proptest writes nothing into the checkout.** `PROPTEST_DISABLE_FAILURE_PERSISTENCE` turns off failure persistence for every proptest whose config starts from the default; the sandbox stops a `*.proptest-regressions` file elsewhere. To keep a shrunk case, add the minimal input proptest prints as a plain `#[test]`.
- **Doctests** are off (`doctest = false`): doc comments carry no examples, and nextest does not run them.
- **Repository guard.** `scripts/repo-guard snapshot|verify [--checkout] <worktree> <file>` (and `identify <worktree>`) re-runs itself under `env -i`, so no inherited `GIT_*` or startup file reaches it or git. It first checks that git resolves `<worktree>` to the top level, git dir, and common dir that its `.git` entry names. Where git has semantics of its own, the guard asks git instead of re-implementing them. The state is: the refs with HEAD, `packed-refs`, the worktree list, each `worktrees/<name>/gitdir`, every config entry git sees in the worktree with the file it comes from (`git config --list --show-origin --includes`, so nested and conditional includes and `config.worktree` count) and the content of each such file but the common `config` and of each file an `include.path` or `includeIf.<condition>.path` names, which may hold no entry and so have no origin (git names a file relative to the worktree or as an absolute path in its own form, `C:/...` with Git for Windows; the shell resolves it from the worktree, and a directory it cannot find fails the guard; an include path is resolved as git does, `~/` from `HOME` and a relative path from the including file, and one whose directory is missing is recorded as missing), the hooks directory git resolves (`git rev-parse --git-path hooks`, which follows `core.hooksPath` wherever it is set, relative to the worktree) with contents and modes, `info/`, the stash list, this worktree's git dir (HEAD, index, `sharedindex.*`, `config.worktree`, logs, sockets such as an fsmonitor daemon's), and the reflog of the branch it has checked out, as manifests walked without following symbolic links. Locally, other worktrees of the repository change legitimately, and other agents add and remove them, so the branches they have checked out at the snapshot or at the verify (refs, remote-tracking refs, reflogs, `branch.<name>.*` config, `packed-refs` lines), their entries in the worktree list and their `worktrees/<name>/gitdir` lines, their git dirs, the objects, and refs outside git's own namespaces are left out, while this worktree's own entry and `gitdir` line stay compared: the state marks the lines of each branch, and `verify` drops those of every branch another worktree had checked out at either point from both states, so a worktree that switches branch, or is added or removed, during the run does not fail it, while a branch another worktree had checked out only in between counts as a change. Where the tests run confined, the confinement already denies writes to the shared git dir and the guard is the second check, and CI runs the guard strict on every platform: with `CI=true` the guard compares the whole worktree list, the whole common git dir (objects by name), and every ref. A local run that is not confined (Linux without bubblewrap) has neither, so a test that changes what is left out passes it locally and fails only in CI. With `--checkout` or `CI=true` the state also holds the checkout's files as `git status` lists them: each tracked file that differs from the index or HEAD and each untracked file git does not ignore, with its content. `scripts/test` snapshots before phase 2 and verifies after it, with `--checkout` when the tests run unconfined (Linux without bubblewrap, Windows): a test's working directory is its package root inside the checkout, so a relative path lands there. Locally and confined the checkout is not compared, because the tests cannot write to it and the developer may edit files while they run. `tests/repo_guard.rs` snapshots a fixture, makes each kind of change a test that escaped its fixture would make, and asserts `verify` fails; it also shows that exported `GIT_*` variables do not move the guard to another repository.
- **Self-check.** `scripts/test --self-check` runs the canaries of `crates/test-support/canaries` (a separate workspace the normal run never builds) and expects: a raw `Command::new` and the `CommandCargoExt` form fail clippy; a test writing outside `R` fails and leaves nothing (reported as not prevented where there is no confinement); a test cannot connect to a Unix socket a listener holds beside `R`, while the same test run unconfined can and a socket inside `R` works; tests that leave a process with its output closed, one in a new session, one that closed every descriptor, and a double-forked daemon are killed by the sweep, which fails; a process table that lists nothing fails the sweep; and `scripts/test` started with `BASH_ENV`, `ENV`, `GIT_DIR`, `GIT_WORK_TREE`, and `GIT_INDEX_FILE` pointing at a decoy runs no startup file, gives a test none of them, and has the guard inspect this checkout while the decoy stays unchanged.

CI runs `scripts/test`, `scripts/test --self-check`, and `scripts/test` with `BASH_ENV` and decoy `GIT_*` exported, on macOS, Linux (with bubblewrap installed and unprivileged user namespaces allowed), and Windows. `tests/repo_guard.rs` covers the guard; `run-processes` has a unit test that hands the kill a record whose PID names another process, as PID reuse would, and asserts that process survives.

To reproduce the Linux job on another OS, run `scripts/test` in a container:

1. Start `rust:1.100-bookworm` (the Rust version `mise.toml` resolves to) with `--platform linux/amd64`, `--security-opt seccomp=unconfined --security-opt apparmor=unconfined`, which bubblewrap needs for an unprivileged user namespace, and `--init`, so that PID 1 reaps the processes orphaned to it. Without it, a container whose first process does not reap (such as `sleep infinity`) keeps every orphan as a zombie in its process group: tests that wait for a group to empty time out, and the sweep reports the zombies as left by the run.
2. Install `bubblewrap` and `procps` with apt, add the `rustfmt` and `clippy` components, and put `cargo-nextest` of the version in `mise.lock` (its release tarball) on `PATH`.
3. Create a user that is not root and give it `CARGO_HOME` and `RUSTUP_HOME`: the sweep looks only at the current user's processes, and root would make the run differ from CI.
4. Copy the tree in without the files git ignores and without macOS metadata (on macOS, `git ls-files -co --exclude-standard -z | COPYFILE_DISABLE=1 tar --no-xattrs --no-mac-metadata --null -T - -cf src.tar`; otherwise `._*` files fail the layering and config tests), then `git init` and commit it there, since the guard needs a repository.
5. As that user, run `CI=true scripts/test` and `CI=true scripts/test --self-check`.

### 13.5 Other rules

- Each integration test gets its own `CARDINAL_STATE_DIR` and `CARDINAL_CONFIG_DIR` inside a scope.
- Runner and admission tests start every child (the runner, shells, helpers) through one scope, in its worktree, shells without startup files (`bash --norc --noprofile`). The runner joins a group that an idle process of the test leads, as under a shell without job control, so it does not lead its own group (section 5.5); a test of a runner that leads its group starts it in a group of its own. Tests of the caller's session start a job-control `bash` (`set -m`) as the leader of a new session, which gives the runner's parent shell a group of its own. Dropping the test environment kills the caller's group, then every process still holding the scope's marker (`LeftoverGuard`), and asserts that none is left.
- G5 and C20 tests list the worktree recursively, including ignored files, before and after hook, run, and act, and use stages that write nothing.
- C6 tests run each input through the real shells (`bash` and `zsh` where installed, `sh` always) once directly and once rewritten, and compare stdout, stderr, exit status, final `pwd` (printed by the outer shell after the command), and `env` output. Inputs include short-circuit lists (`cd missing && cargo test | cat || printf fallback`), `pipefail`, and `errexit` from the rc, from a function, and from the command itself, with stages before `&&` and `||` and after a pipe, non-exported variables and globs in a stage, redirections (also of a stream the stage's pipeline connects), several classified stages, a missing runner path, and stages and commands that must stay unwrapped (section 7.2): among them, operators that read the fallback test's state, earlier stages that write the function or command-path tables, and a command word with a leading `~`. Wrapped stages run through the real runner: the built `cardinal`, linked into the test's `bin/` so the runner path passes the `[hook] runner` rule, with a state and config dir of the test's own. Its `job_started` records count the jobs it started, and a runner that fails open fails the test. The missing runner is a path that does not exist, a regular file without execute permission in place of the link, and a directory. On macOS, CoreFoundation, which `cardinal` links, sets `__CF_USER_TEXT_ENCODING` before `main`, so every stage the runner starts carries it; the agent's shell inherits the same value from the agent, which links CoreFoundation as well, and the test's shells get it from the test process.
- Tests of what reads reservations (`inventory`, the inbox refresh, `plan`, `act`) make them with the real runner (`tests/support/jobs.rs`): a runner paused after the spawn holds a `live` reservation and the worktree's in-use lock, one killed by SIGKILL after the spawn leaves a verified `recovery_needed` reservation while its group runs (unverified once the test also kills the stage's shell, the recorded child), and one killed before the start boundary leaves a `stale` one, which the next admission pass deletes, so it is made last. Each test's `host.toml` budget admits all its jobs at once, whatever the host's CPU count, and a stage's processes read a pipe the test holds, so they live until the test ends them. `plan`, `act`, and `explain` tests run the binary on fixture repositories whose branches are pushed to a bare remote, with gh answered by a fake that `[observe] gh` names; a fact changes through the fixture, gh's answers, or a real reservation. `explain command` is checked against the hook's own rewrite of the same command and configuration.
- Fakes remain where a system cannot produce the case on demand: gh; the process table of `tests/recovery.rs` (a reused PGID, a child that left its descendants behind); the reservations `tests/runner_support` writes and holds live to fill the budget at a set moment; the recovery pass of `inbox::concurrent_refresh_keeps_newer_items`, which needs a reservation that a newer refresh observes after an older one observed none; the recording `git`, `gh`, and `claude` of the admission-isolation test below; free-space readings, which a test writes into `CARDINAL_TEST_DIR` per path prefix and `sensor` reads only when that variable is set, for the order, stop, hysteresis, and prediction tests (the real measure is covered by the volume tests of section 13.6); and the arbiter command, a script `[arbiter] command` names that records its argv, environment, and inherited descriptors, holds or exits on the test's signal, and runs `cardinal` commands the test lists.
- **One check per refusal.** A refusal test builds a target on which every check passes except the one it names, and asserts that `reasons` equals exactly that check; it also runs the control, the same target without the fault, which reports `dry_run`. Two checks rejecting the same target can hide a missing one.
- **Tests pin what they assume.** Every test sets the budgets, disk lines, and intervals it depends on in its own `host.toml`, as fractions of what it measured (the test volume's size, the host's CPU count), so a different host cannot change which branch it takes.
- **Every writer of what a decision reads holds that decision's lock**, and a test shows each: reservations under `admission.lock` (C2), disk states and reconcile state under `reconcile.lock`, the arbiter's pass record under `arbiter/.lock`, and inbox items under `inbox/.lock`. Each such test runs a writer paused inside the lock against the decision and asserts the decision waits or sees the write.
- The admission path performs no network or arbiter call at run time: `cardinal run` executes with recording fakes for `git`, `gh`, and `claude` first in `PATH` and a recording listener in `HTTP_PROXY`, `HTTPS_PROXY`, and `ALL_PROXY`; the test asserts no fake was invoked, no connection arrived, and the inbox directory is unchanged. It also asserts that the runner holds no socket at `admission.reserved` and `run.after_spawn` (`/proc/<runner>/fd` on Linux, `proc_pidinfo` on macOS). The runner starts with its standard streams only (section 13.3), so a socket it holds is one it opened. The listener runs on Linux, where the tests' confinement gives them a loopback of their own; the macOS sandbox denies the test every TCP socket, and the runner too, so a listener could neither bind nor see a connection there, and the proxy variables name a closed port. There the check rests on the two socket snapshots, which miss a socket the runner opened and closed between them.
- C13 fixtures put protected data deep inside a regenerable directory (`target/a/b/fuzz/corpus/seed`), so a check that stops at the top level fails. Refusal tests pair each refusal with a control that passes every check (`dry_run`), including `reclaim_dir` of `node_modules` in a managed worktree (refused) next to the same in an unmanaged one (`dry_run`).
- The shipped `examples/claude/settings.json` is parsed as JSON, not searched as text: no `permissions.allow` or `permissions.ask` entry may match a runner invocation, and `defaultMode` may not skip approval (section 6.5). The shipped launchd plist and systemd units are parsed too: they run `cardinal reconcile --trigger timer` and nothing else.
- CI runs the whole suite on macOS, Linux, and Windows. Unix-only tests (process groups, signals, zsh, rewriting, descriptor-relative snapshots) are `#[cfg(unix)]`; on Windows the hook's no-rewrite rule, the fail-open runner, and `act` as a dry run are tested; nothing is skipped silently through features. The Linux bind-mount tests need a mount namespace of their own (`Scope::mount_namespace`, section 13.3): where the host forbids it they are skipped with the reason written to stderr past the test harness's capture, and where `CI` is `true` they fail instead; each writes a line to stderr when its assertions ran, which `.config/nextest.toml` shows for a passing test.

### 13.6 Report, disk, and arbiter tests

Each test below records a listing with contents of the worktrees, their repositories' git dirs, and the directories outside them before and after, and asserts it unchanged: phase 1 changes nothing outside cardinal's state dir.

**Filtered paths.** A fixture of its own, observed by the no-write tests (`no_repo_writes`, `observe_readonly`) and the report tests, holds a tracked file whose `filter` attribute names a configured clean filter that writes into the git dir, as Git LFS's does, edited at the same size: the filter never runs; `dirty` is `unknown` with `filtered_paths` and the path, in the observation and the `clean` check; the worktree's plan is refused by `clean` alone; and a `reclaim_dir` plan in it reports `dry_run`. The clean controls of the other tests hold no filtered edit.

**Reports.** For both plan kinds, every caller, and with and without `--dry-run` and `--owner-approved` (from the owner), a plan whose checks pass reports `dry_run` with `execution_not_designed` and a plan whose check fails reports `rejected` with that check, repeated runs reporting the same. A symbolic link in the target to a directory outside, and one as the target; a hard link in the target to a file outside; a pnpm-like `node_modules` (packages linked into `.pnpm`, files hard-linked from a store outside the worktree), whose store files count in `aliased_bytes`; and a mount below the target (bind mount on Linux, `Scope::attach_image` on macOS), refused by `plain_target`.

**Disk.** With the readings fake: hysteresis, prediction, a first reading below the critical line (`critical` at once, and `disk-reclaim` runs), `disk-reclaim`'s order (oldest worktree, then most exclusive bytes: an aliased `node_modules` after a smaller unaliased directory), its stop once the listed bytes would lift the final reading above the recovery line, the shortfall when they would not, a final reading lower than step 4's, the `blocked` bytes per first failed check, an unreadable volume skipped, a `planned` record per listed target, and no `act` and no second observation. On a real small volume (a 64 MiB tmpfs on Linux, a 256 MiB APFS image on macOS, lines set as fractions of its size), with regenerable entries in worktrees, one in use by a paused runner, and protected data: the test writes past the warning line, runs `cardinal reconcile`, and asserts the volume's state and the `disk_low` item's disk report: the final reading, the targets in order, and the in-use worktree's entries and the protected `target/` in `blocked` by worktree and path; a final reading that fails ends no item. **The handoff:** paused at `reconcile.observed`, a pass under pressure has written no disk item; released, its items are complete when the trigger and the page read them, no arbiter pass starts for them, and after the writer's file is deleted `disk_critical` ends at the first pass above the critical line while `disk_low` stays until the recovery line. **The job-end check:** paused at `run.finished`, the runner has made no attempt to start `cardinal reconcile` and `job_finished` is recorded; released, it reaches `run.reconcile_spawning` and a pass starts. A failure injected in the check leaves the stage's output and status as without it.

**Arbiter.** With the fake command: one pass per set of due items, and `reconcile` reports `running` during it; the fake's `cardinal inbox --assigned` lists exactly the pass's items, also when another item becomes due while it runs, its `cardinal inbox resolve KEY --assigned` on an item outside the pass, or on any item after its pass was recorded `abandoned`, is refused (`not_assigned`) and changes nothing, and the launcher writes the outcome into the pass's items only; a launcher killed at `arbiter.recorded` leaves the listed items unassigned and due at the next trigger, while one killed at `arbiter.outcomes` leaves its items' outcomes in place when the pass is recorded `abandoned`, and items given no outcome share one end time and are due `retry_s` after it; an item the fake leaves open (its `act` refused by a reservation) and the items of a fake that exits non-zero are due again after `retry_s`, not before, one it resolves never, and the third failed pass raises `arbiter_failing`; disk items start no pass; a fake that does not exit is killed at the timeout (`timed_out`); items the fake raises wait for the interval. **A launcher killed** with SIGKILL leaves the pass `running` until `timeout_s` after its start; the next `reconcile` records it `abandoned` in `last.json` and its items, which are due after `retry_s`, and nothing signals the fake.

## 14. Work split

Phase 0 was built as units F and #4–#8 of issue #9; their completion tests stay in the suite. Phase 1 is built as the units below, one PR each, after this document merges. The release pipeline (#10) is separate.

### 14.1 Order

```mermaid
flowchart LR
  F1["F1: contracts, config, state paths, CLI, testpoints, test volumes"] --> D["D: detection, disk, reconcile, timers"]
  F1 --> N["N: inbox items"]
  F1 --> L["L: arbiter launch, skill v2"]
  F1 --> U["U: snapshot and local page"]
  D -. "integration gate" .-> N
  N -. "integration gate" .-> L
  D -. "integration gate" .-> U
  N -. "integration gate" .-> U
  L -. "integration gate" .-> U
  D --> R["R: real-data report run"]
  N --> R
  L --> R
  U --> R
  R -. "evidence" .-> X["X (#33): executing regenerable directories"]
```

F1 makes the tree build with every phase 1 contract in place and every existing test passing. It declares every contract type and schema of section 11.3, the configuration of section 3.2 with its validation, the state paths of section 4.1, the CLI of section 11.2 with handlers that call the new modules, the testpoints of section 13.1, and the module table of section 2.2 in `tests/layering.rs`, registers every new module in `src/lib.rs`, and adds `objc2-core-foundation` to the workspace and crate manifests (`Cargo.toml`, `crates/cardinal/Cargo.toml`) and `Cargo.lock`. It also builds the test volumes D uses: `Scope::tmpfs` and `Scope::attach_image` (section 13.3), the empty disk image `scripts/test` creates, and the CI setup they need. For each contract type it changes it migrates the producers, so nothing is left half-built: `PlanTarget.git_dir` in `src/plan/mod.rs`, the new `Worktree` facts in `src/observe/mod.rs` (`unknown` and `null` until D fills them), `ActResult` and `ActOptions.squash_merged` in `src/act/mod.rs`. New entry points get bodies that return `cardinal.error.v1` code `not_implemented`, or `unknown` for a fact: `os::disk` and `os::spawn_detached` in `src/os/mod.rs`, `observe::content_in_base`, the `reconcile` pass of section 10.3 as one stub call per step, and `src/snapshot/`. It prewires the joins between units: `src/inbox/mod.rs` gets the occurrence and assignment entry points as stubs and registers `src/inbox/assign.rs`, and step 6 of the pass hands the disk report to the disk-item raiser, so D, N, and L fill bodies behind calls already in place. It moves the merge-evidence check from `src/plan/checks.rs` to `src/plan/merge.rs` unchanged, and fixes phase 0's observation, which still runs filter drivers and may fetch missing objects: F1 disables both as section 8.1 says, before any unattended pass exists. The other units then fill bodies in the files section 14.2 gives them.

| Unit | Can start | Integration gate |
| --- | --- | --- |
| F1 | Immediately | — |
| D | After F1 | — |
| N | After F1 | After D: items raised from the pass's full observation and disk states |
| L | After F1 | After N: the trigger and the skill meet the new kinds |
| U | After F1 | After D, N, and L: the page shows their real states |
| R | After D, N, L, and U | — |

A unit is done when its own completion tests pass; its integration gate is a follow-up PR by the same unit once the dependency has merged. The Cloudflare push (option B of section 10.6) becomes a unit only once section 14.5 decides it. The first release, units F1 to R, is report-only. **X**, executing regenerable directories, follows later in phase 1 through #33: it starts from a design that meets section 9.7, with R's counts in hand, and brings its own tests (races, crashes, durability, full volumes), its claims (C11, and the execution parts of C10 and C15), and the execution parts of the checks of G2 and G7. Executing whole worktrees, with G3's execution part, stays deferred (section 14.5).

### 14.2 Work units

| Unit | Owns | Completion tests |
| --- | --- | --- |
| **F1** foundation | Section 14.1, and the F1 column below | `tests/contract.rs`, `tests/cli.rs` (`--owner-approved` only with `--caller owner`), configuration validation, `tests/layering.rs`, and every existing test |
| **D** detection and reconcile | Observed facts and squash evidence, the checks and `act`'s report, disk states, `disk-reclaim`, the pass, timers, the job-end check | `tests/observe_worktrees.rs`, `tests/squash.rs`, `tests/plan_act.rs`, `tests/act_report_only.rs`, `tests/act_fs.rs`, `tests/unit_dry_run.rs`, `tests/disk_state.rs`, `tests/disk_reclaim.rs`, `tests/disk_volume.rs`, `tests/reconcile.rs`, `tests/run_disk_trigger.rs`, `tests/timer_examples.rs`, `tests/admission_isolation.rs` |
| **N** inbox | Occurrences, and the item kinds of section 10.1 raised and ended from observation and disk states | `tests/inbox_raisers.rs`, `tests/inbox.rs` |
| **L** arbiter launch | The due rule and the assignment, the trigger and the launcher, skill v2 | `tests/arbiter_launch.rs`, `tests/skill_commands.rs` |
| **U** web UI | The snapshot and the page | `tests/ui.rs`: the page from a fixed snapshot, self-contained, written with no git, gh, or network call |
| **R** real-data report run | No code: section 14.4, run by hand on the development host | Section 14.4's counts, reported on issue #9 |

Every file a unit changes has one owner at a time; a file F1 touches passes to the unit named after it:

| File | F1 | Then |
| --- | --- | --- |
| `Cargo.toml`, `crates/cardinal/Cargo.toml`, `Cargo.lock` | The dependency of section 12 (`objc2-core-foundation`) | A unit that needs another dependency adds it in its own PR and says so |
| `src/lib.rs`, `src/contract/`, `src/config/`, `src/state/mod.rs`, `src/cli/mod.rs`, `src/testpoint.rs` | All of phase 1's additions | Nobody; a contract change found later is a follow-up F PR |
| `src/os/mod.rs` | The entry points and their stubs | D: `src/os/disk.rs` and `src/os/detached.rs` |
| `src/plan/mod.rs` | The `PlanTarget` producer | D: plans for every linked worktree and every entry matching a regenerable pattern (section 9.2), where phase 0's producer plans a whole worktree only when it is a candidate |
| `src/plan/checks.rs`, `src/plan/walk.rs` | The merge-evidence check moved out | D: `not_in_use` over the whole worktree, `plain_target`'s mount reads, `regenerable` as a pattern match |
| `src/plan/merge.rs` | Moved, unchanged | D: `--squash-merged` with `content_in_base` |
| `src/act/` | `ActResult`, `ActOptions` | D: `execution_not_designed` |
| `src/observe/` | The new facts as `unknown` and `null`; filter drivers and lazy fetches disabled (section 8.1) | D |
| `src/run/mod.rs`, `src/sensor.rs` | — | D: the job-end check, free space |
| `src/reconcile/` | The pass as one stub call per step, step 6 handing the disk report to the disk-item raiser | D, except `raise.rs`: N, and `arbiter.rs`: L |
| `src/inbox/mod.rs` | The occurrence and assignment entry points as stubs; `assign.rs` registered | N |
| `src/inbox/assign.rs` | Stubs | L |
| `src/snapshot/` | The skeleton | U |
| `crates/test-support/src/scope.rs`, `scope/mount.rs` | `Scope::tmpfs`, `Scope::attach_image` | A unit that needs another typed operation adds it in its own PR and says so |
| `scripts/test`, `.github/workflows/ci.yml` | The disk image and the CI setup the test volumes need; a new action also goes into `.github/infra.yaml` | Nobody; a change found later is a follow-up F PR |
| `examples/launchd/`, `examples/systemd/` | — | D |
| `integrations/claude-code/` | — | L: skill v2; the README says the timer starts the arbiter |

N fills the occurrences in `src/inbox/mod.rs` (section 10.1) and raises the new kinds, the disk items included, from `src/reconcile/raise.rs`; L fills the due rule and the assignment of section 10.5, which the launcher and `cardinal inbox --assigned` share, in `src/inbox/assign.rs`. Every work unit's completion command is `mise exec -- scripts/test`.

### 14.3 Arbiter skill v2 contract

The subagent runs headless, one at a time, and only through the CLI:

1. On a pass the timer started, `cardinal inbox --assigned`, the items assigned to this pass (section 10.5), and no others; every `cardinal inbox resolve` of that pass carries `--assigned`. On the owner's request, `cardinal inbox`. Then `cardinal explain inbox KEY` for each item listed, and on the owner's request `cardinal inventory` once.
2. Plans: on a pass the timer started, `cardinal plan reclaim-worktree PATH` for the worktree of each assigned `reclaimable` and `merge_undecided` item, and no other plan; on the owner's request, `cardinal plan`. A whole-worktree plan with `invariants_ok: true` and `approval: "none"`: `cardinal act PLAN --caller arbiter`, which in phase 1 reports `dry_run` with `execution_not_designed` (section 9.3). A `reclaim_dir` plan only when the request that started the session names it (an owner's request, as in G10's check), never on a pass the timer started: regenerable data is `disk-reclaim`'s, under disk pressure (section 10.4). A plan needing approval whose `merge_evidence` facts show `content_in_base: "yes"`: judge from the evidence (an empty `content_diff`, `closed_pr`, the branch and pull-request history in the item) whether HEAD was squash-merged, and if so run `cardinal act PLAN --caller arbiter --squash-merged`; otherwise, and for every other plan needing approval, report it for the owner. Before it acts on or resolves an assigned item, a pass the timer started compares the item's subject (path, HEAD, branch) with the fresh plan's target; on a mismatch it leaves the item open, and the next `reconcile` pass ends it and raises the new state.
3. Disk items are the owner's and never assigned to a pass (section 10.5). On the owner's request, report them from their disk report, never from `inventory`'s `disk`.
4. Items: `reclaimable` by acting on its plan as in step 2, resolved with the result as its note once that `act` reports `dry_run`, which is the report the item asks for (the worktree stays until the owner removes it), and left open when it is refused; `merge_undecided` as in step 2, resolved with the judgment once its `act` reports `dry_run`, and escalated otherwise; `closed_pr_dirty` and `managed_merged` (to be removed in its app) escalated with their paths; `unclassifiable_data` escalated with its paths and the question whether each is `regenerable` or `protected` in the repository's profile; `recovery_needed` and `outlived_processes` as in v1. A verified `recovery_needed` stays open, to be read again after `retry_s`: cardinal releases its reservation once the group has ended, and nothing signals the group.
5. `act` results: `dry_run` is "would reclaim" (`execution_not_designed` in phase 1); `rejected`, `expired`, and `already_executed` are refused with their `reasons`; `executed`, `partial`, and anything else go under errors, since phase 1 executes nothing.
6. Never `--owner-approved` or `--caller owner`; never `rm`, `git worktree remove`, `kill`, or edits in repositories.
7. The report adds a "Reclaimable" section (plans that report `dry_run`, with their bytes) to v1's sections.

`tests/skill_commands.rs` parses every `cardinal …` line of the skill with the clap definitions and checks that the skill's tables cover exactly the statuses of section 11.3 and the inbox kinds of section 10.1, and that a pass the timer started runs no bare `cardinal plan`.

### 14.4 First run on real data

Phase 1 first runs on a macOS development host against the worktrees of a real TypeScript monorepo (design §12). Nothing is deleted: the run checks the reports and measures what the execution design starts from.

1. **Install.** The binary (#10, or `cargo install --path crates/cardinal`), the hook, the arbiter files, and the launchd agent. `host.toml`: `[observe] repos` with the monorepo's main worktree; `[observe] managed` with T3 Code's worktree root. A profile for the monorepo whose `[data] regenerable` lists `node_modules` and the build and cache outputs it uses (such as `.turbo`, `dist`, `.wrangler/tmp`) and whose `protected` lists local state that cannot be regenerated (such as `.wrangler/state`, local D1, KV, and R2 data); anything unlisted is unclassifiable and never deleted.
2. **Check the classification.** `cardinal inventory --sizes` and `cardinal plan`. First, every worktree under T3 Code's root must show `managed_by: "t3code"`; without that, they look like any other worktree and could be reclaimed. Then compare, worktree by worktree, cardinal's verdict (candidate, failing checks, `content_in_base`) with what is known about each worktree, and record the counts: worktrees, candidates, needing approval, refused per check, bytes, and worktrees whose `dirty` is `unknown` because of filtered paths (section 8.2). This answers design §14's question whether every existing worktree is classified correctly.
3. **A report week.** The timer and the arbiter run for at least a week. Once, with the lines of every watched volume set above its free space for one pass, record how long a `reconcile` pass under pressure takes (observation with sizes and plans, with each target's entry count). The warning line is set from these: the room between the warning and critical lines must exceed the growth over one interval and two such passes (section 10.4). Each `planned` record and `dry_run` result is a deletion that would have happened and is reviewed on the page (section 10.6); a target that had to be kept, or a planned target used again later, is a false positive and is recorded with its cause.

Issue #9 gets the counts: worktrees, candidates, refusals per check, bytes, pass times, entry counts, and false positives with their causes. They are what the execution design starts from (section 9.7, section 14.5); real paths and branch names stay on the host.

### 14.5 Open decisions

Each row is decided outside this document: it involves an account, money, behaviour outside the host, a change to design.md, or a host's own settings. Until it is decided, the proposal holds; a decided row says so.

| Decision | Options | Proposed |
| --- | --- | --- |
| Where the web UI is shown (section 10.6) | A local page; B a Cloudflare Worker and Durable Object, which needs the account, a plan (Workers Free, or Paid from $5 a month), Access with a card on file, a hostname, per-host tokens, and agreement on what leaves the host; C a static page on Cloudflare | A now, B after this decision |
| The arbiter's squash-merge judgment (section 9.3, section 17 item 13) | Accept `--squash-merged` from the arbiter, checked against `content_in_base`; or require owner approval for every merge without merge evidence, so G3's squash case needs approval. In phase 1 it changes reports only | Accept, with the design §7 change of section 17 item 13 |
| Regenerable data in app-managed worktrees (section 17 item 16) | Keep them report-only; or let `reclaim_dir` run in them, a design §4 change | Report-only; revisit with the counts of section 14.4 |
| Whole-worktree execution and G3 (section 9.7, section 17 item 17) | (A) Defer executing whole worktrees, and G3's execution part, to a later phase: phase 1 reports reclaimable worktrees and the owner removes them. (B) Keep it in phase 1 and design now what section 9.7 requirements 15–17 list | Decided: A |
| A scratch GitHub repository for the real-host checks of section 15 | A private repository created for the checks and deleted after them; or no real-host check of the pull-request cases of G3, G6, and G8 | Create one |
| Enabling execution (section 9.7) | Which callers execute, on which hosts, after which evidence (section 14.4), accepting which residual costs of section 9.7 (requirements 5, 6, and 10), and whether worktrees that containers or VMs use are listed under `[observe] managed` first | Pending the execution design (#33); the first release executes nothing |
| Unattended model usage | Each arbiter pass costs model usage. Passes run only for due items, at most one per `min_interval_s`; an item is tried again only after `retry_s`, so items left open or failing cost at most one pass per `retry_s`; disk items start none | `min_interval_s` 30 minutes, `retry_s` 2 hours |
| Disk lines and the timer interval (section 10.4) | Defaults: warning, recovery, and critical at 20, 30, and 5 GiB; 5 minutes. The warning line must leave room for the growth over one interval plus two passes under pressure (section 10.4); section 14.4 measures these, and none is derived here | Set per host from section 14.4's measurements |
| Installing the timer | cardinal installs its launchd agent or systemd timer; or it is copied by hand from `examples/` | By hand: cardinal writes nothing to the user's service configuration |

## 15. Purpose layer

Real-host checks run on macOS with Claude Code and the hook installed as in section 6.2, a scratch state and config dir, and scratch repositories outside real projects. Phase 1's checks also use a scratch GitHub repository, for the pull-request facts (section 14.5), and a 2 GiB APFS disk image attached at a scratch path, for disk pressure. Every refused case sits next to a control that is reclaimed in the same run (design §10); in phase 1 the control reports `dry_run` instead. Windows real-host checks are phase 3.

| ID | Phase | Condition (design §10) | Check on a real host |
| --- | --- | --- | --- |
| G2 | 1 (detection and report); execution pending (section 14.5) | When free space drops below the warning line while reclaimable outputs exist, they are reclaimed before space runs out, and reclamation stops above the recovery line | On the image: four scratch worktrees with 300 MiB of regenerable `target/` and `node_modules` each and different last activity, one in use by a paused classified command, one with `fuzz/corpus` inside its `target/`. Lines for the image: warning 600 MiB, recovery 900 MiB, critical 200 MiB; the launchd agent of `examples/launchd` with `StartInterval` 60. A writer outside cardinal grows a file on the image by 4 MiB/s until 400 MiB are free. Within one interval and two pass lengths of the crossing (the bound of section 10.4, with the pass lengths measured in this run), a pass publishes a `disk_low` item and the page shows it marked for the owner, with the pass's final reading, the targets `disk-reclaim` lists in the order of section 10.4 down to the recovery line or the shortfall, and, in `blocked` by worktree and path, the in-use worktree's entries and the protected `target/`; no arbiter pass starts for it, and nothing is deleted. The same with the writer stopping 100 MiB below the critical line: `disk_critical` is raised too, and the report runs there as well; once the writer's file is deleted, `disk_critical` ends at the next pass and `disk_low` above the recovery line. Then, with the interval at one hour, a classified command that writes past the warning line starts a pass when it ends |
| G3 | 1 (report); execution pending (section 14.5) | A worktree meeting every reclamation condition is reclaimed without the owner or an orchestrator; in dry-run, reported | In the scratch GitHub repository, after a `git fetch` in the main worktree: clean, pushed worktrees whose pull request was merged with a merge commit, whose pull request was squash-merged, whose branch was squash-merged into the base without a pull request (`git merge --squash`, pushed), and one not merged. Only the timer runs. The first reports `dry_run` with merge evidence `base_ancestor` (the merge commit makes HEAD an ancestor of the base), the second with `merged_pr`, both with `execution_not_designed`, and both raise `reclaimable`, which the arbiter resolves with that result; the third reports `dry_run` after the arbiter judged it (`--squash-merged`, section 9.3), and the unmerged one is refused (`merge_evidence`). Nothing is removed and every branch stays |
| G4 | 0 | Before the child starts, a missing, crashed, or unresponsive cardinal, or a runner that cannot start, leaves the command running exactly once with the same result; after start, no rerun and an infrastructure failure | A classified probe prints its args, `pwd`, sorted `env`, writes to stdout and stderr, appends one line to a counter file outside the repository, and exits 7. An agent runs it inside `cd sub && probe 2>&1 \| tail -5 \|\| echo fallback` (a) without the hook, (b) with the runner path not executable, (c) with the state dir unwritable, (d) with `admission.lock` held by another process past `lock_timeout_ms`, (e) with the hook's deadline forced to be missed, (f) with a failure injected before the start boundary. All outputs equal (a), and the counter grows by one each time. Permissions, with the shipped settings: in `default` and `acceptEdits` modes each rewritten call prompts once for all its stages, also with an allow rule for the probe; a deny rule for the probe still denies the rewritten call; with `Bash(probe)` denied, an agent's direct `R run -- probe` does not run without approval; a session without an approval surface refuses the rewritten call; `bypassPermissions` runs it without a prompt, as it runs the original; in `auto` mode nothing is rewritten. After start: SIGKILL the runner of a long probe; the counter shows one start, the exit status is 137, and `cardinal explain job` reports `runner_died`. The accepted zero-run cases (runner killed before the spawn, Bash call cancelled before the spawn) end with no run |
| G5 | 0, 1 | No files or instructions are added to target repositories | Snapshot `git status --porcelain --ignored --untracked-files=all`, a recursive listing (ignored files included) of a real worktree, and a listing of its repository's common dir; install, run classified commands that write nothing, run `inventory`, `plan`, `act` on the plans of this worktree and of a sibling, and a `reconcile` pass; compare. Nothing was added or removed. Also confirm no repository file mentions cardinal |
| G6 | 1 (report); the control's reclamation pending with execution | Worktrees with uncommitted changes, unpushed commits, an open PR, no upstream, a failed observation, or in use (a waiting job included) are not reclaimed, and a reclaimable worktree beside them is | In the scratch GitHub repository, next to a merged, clean, pushed control: a worktree with uncommitted changes, one with an unpushed commit, one with an open pull request, one whose branch has no upstream, one running a classified command, and one whose classified command waits because the budget of one job is taken by a job in another worktree; plus a worktree of a second repository whose `origin` names a GitHub repository gh cannot read. The timer and the arbiter run. In `cardinal plan` only the control's plan has `invariants_ok: true`; its `act` by the arbiter reports `dry_run` (`execution_not_designed`) and `reclaimable` is raised for it alone. Every other plan fails its check there (`invariants_ok: false`, the check named in `checks`), the arbiter runs no `act` on it, and its files, HEAD, branch, and status are unchanged |
| G7 | 1 (report); execution pending (section 14.5) | Protected data and the worktree containing it are never deleted, and regenerable directories beside them are | On the image, under the pressure of G2: a merged, clean worktree holding `fuzz/corpus`, `fuzz/artifacts`, and `proptest-regressions`, with `target/a/b/fuzz/corpus/seed` in its `target/`, next to a control worktree's `target/`. The control is among `disk-reclaim`'s listed targets, with a `planned` record, and an explicit `act` on its plan, the recheck, reports `dry_run`; the worktree's plan is refused (`no_protected`), its `target/` is refused and counted in `blocked` under `no_protected`, and every protected file is unchanged byte for byte |
| G8 | 1 | The arbiter resolves what it can, and only decisions only the owner can make reach the owner | With the timer running: the branch squash-merged without a pull request (G3), a dirty worktree whose pull request was closed without merging, and a session that ends leaving a background dev server running. Nobody asks for a pass: the timer starts the arbiter, which judges the squash-merged worktree (its `act --squash-merged` reports `dry_run`, and its `merge_undecided` item is resolved with the judgment) and escalates `closed_pr_dirty` and `outlived_processes`. Those two are the only items in the report's owner section and marked for the owner on the page; the dirty worktree and the dev server are untouched |
| G10 | 0, 1 (report); the controls' reclamation pending with execution | With valid plans, safe operations succeed and only G6/G7 operations are refused | Prepare: a merged clean worktree with `node_modules` (control), the `target/` of a dirty worktree (control), a worktree with `fuzz/corpus`, a `target/` with protected data deep inside, a dirty worktree, a worktree with an unpushed commit, a worktree without upstream, a worktree with a waiting job, and `node_modules` of an app-managed worktree. Ask the arbiter to reclaim all of them. Both controls report `dry_run` (`execution_not_designed`); each other one is refused with the matching check, and the protected worktree's clean `target/` reports `dry_run` on its own |
| G14 | 0 | After a runner crash, reservation and in-use state remain until the group stops, then are released | Budget of one job. Start a classified command whose script starts background children; SIGKILL the runner. Immediately: `inventory` shows `recovery_needed` and `in_use`; `act` on that worktree's plan is refused (`not_in_use`). Start another classified command: it waits while the group lives (recovery never signals it). End the group's members by their PIDs; the waiting command then runs. Afterwards `ps -g <pgid>` is empty and the reservation is gone |

## 16. Plan layer

Claims from design §11 that phases 0 and 1 implement, restated, plus new claims A1–A24. EARS, one line each; test names in files that phase 1 adds are planned (section 14.2).

| ID | Claim (EARS) | Source | Test |
| --- | --- | --- | --- |
| C1 | When a classified stage reaches the PreToolUse hook, the hook shall rewrite it to run through `cardinal run` and shall not create a reservation. | G14 | `hook_claude::rewrite_takes_no_reservation`, `hook_claude::rewrite_runs_through_the_real_runner` |
| C2 | When the runner decides admission, it shall decide inside the admission lock from the live and recovery-needed reservations and the new job's resources against the host budgets, and shall reserve all resources or none. (Sensors: phase 2.) | G1 | `admission_concurrency::*`, `reservation_lock::*` |
| C5 | If the runner path is not executable, the shell fallback shall run the original stage; if the runner fails in any observable way before `spawn()` returns a child, the runner shall run the stage once; after that, cardinal shall not rerun it and shall report an infrastructure failure; if the hook misses its deadline, the command shall not be rewritten. | G4 | `run_start_boundary::*`, `run_infra_failure::*`, `hook_claude::deadline_no_rewrite`, `rewrite_equivalence::runner_missing` |
| C6 | When the hook rewrites a command, the other Bash input fields shall be unchanged, only classified stages shall be wrapped, and the command shall keep its meaning, including lists, pipelines, shell options, cwd, persisted cwd, environment, and output. (No injection in phases 0 and 1.) | G4 | `rewrite_equivalence::*`, `hook_claude::other_fields_preserved`, `run_wait::no_output_added` |
| C8 | When a runner dies, or its stage's direct child exits with group members left, its reservation and in-use state shall count as reserved and in use until its process group is empty or the boot changes. | G14 | `recovery::*`, `inventory::budget_reserved_and_reservations`, `inventory::jobs_make_a_worktree_in_use`, `inbox::recovery_needed_raised_and_cleared` |
| C9 | On SubagentStop and SessionEnd, cardinal shall record the end and shall not stop processes; processes that outlive their session shall appear in the inbox at the next `reconcile` pass or `cardinal inbox`. | G8 | `session_end::*`, `inbox::outlived_processes_raised`, `reconcile::refresh_raises_and_clears` |
| C10 | When a watched volume is under pressure (`warning` or `critical`), `disk-reclaim` shall list, from one set of checks and exclusive sizes evaluated in the same pass, that volume's `reclaim_dir` plans that pass every check without approval, oldest worktree first and the most exclusive bytes first, skipping targets that free nothing, until their exclusive bytes would lift the final reading above the recovery line, and shall report any shortfall. (Executing them: section 9.7.) | G2 | `disk_reclaim::order_and_stop`, `disk_reclaim::exclusive_bytes_first`, `disk_reclaim::in_use_and_protected_skipped`, `disk_reclaim::runs_in_critical`, `disk_reclaim::shortfall_against_the_final_reading`, `disk_reclaim::one_set_of_checks`, `disk_volume::reported_before_full` |
| C12 | A whole worktree shall pass without owner approval only when it has merge evidence (`base_ancestor`, `merged_pr`, or `content_in_base` with `--squash-merged`), is clean, has no unpushed commits, no open PR, an upstream, is not in use or managed, contains no protected, unclassifiable, or nested-repository data, is a plain target, and observation succeeded; in phase 1 such a plan is reported, not executed (A10). | G3, G6, G7 | `plan_act::candidates::*` (commits after merged PR, reused branch name, no upstream, gh failure, control), `squash::*` |
| C13 | When `act` evaluates a plan, it shall inspect the whole target including descendants and reject it if inspection or observation identifies protected or unclassifiable ignored data. | G7, G10 | `plan_act::protected_deep_in_target_refused`, `plan_act::protected_worktree_refused_target_dir_passes` |
| C14 | When `act` evaluates a plan, it shall require a plan ID, report `expired` for an observed bound-identity mismatch, apply the same checks for every caller, and leave the plan unclaimed. | G10 | `plan_act::identity::*`, `act_snapshot::branch_switch_expires_the_plan`, `act_single::*`, `act_report_only::repeated_act_changes_nothing` |
| C15 | The runner shall hold the worktree's in-use lock from acceptance until the stage exits on every path where it can take it, and `act` shall count the worktree in use while the lock is held, a live or recovery-needed reservation names it, or another process has its working directory in it. (What execution needs beyond the lock: section 9.7, section 17 item 6.) | G6 | `in_use::waiting_and_running_jobs_block_act`, `in_use::held_on_fail_open_and_tty`, `plan_act::working_directory_in_the_worktree_refused` |
| C16 | While a unit's `act` is false, `act` called as that unit shall leave the target unchanged and record the planned operation. | G3 | `unit_dry_run::*` |
| C17 | The admission path shall not use the network. | G11 | `layering::admission_has_no_observe_dependency`, `admission_isolation::no_network` |
| C18 | The admission path shall not call the arbiter or wait for it, and the runner's job-end disk check shall start a pass only detached, after the stage's status is fixed. | G1, G4 | `layering::admission_has_no_inbox_dependency`, `admission_isolation::no_arbiter`, `run_disk_trigger::*` |
| C19 | When an item is raised, the inbox shall not duplicate an existing key and shall not reopen resolved or dismissed items, and every item a `reconcile` pass raises shall carry the observations behind it. | G8 | `inbox::*`, `inbox_raisers::*`, `inbox_raisers::unclassifiable_data_once_per_set`, `inbox::occurrence_ends_and_recurs` |
| C20 | cardinal shall create no files inside target repositories. | G5 | `no_repo_writes::*`, `act_report_only::repeated_act_changes_nothing` |
| A1 | When the wait reaches the wait cap, the runner shall start the stage once over budget and record `overflow`. | Interim rule until phase 2: G4, and the seam for C4 | `run_wait::overflow_runs_once` |
| A2 | When cardinal runs git or gh, it shall pass a constructed environment from which repository-location variables inherited from the caller are absent. | G5 | `observe_env::inherited_git_vars_ignored` |
| A3 | Observation shall not write inside any repository, including its `.git` directory, shall run no filter driver a repository configures, and shall fetch no object. | G5, C20 | `observe_readonly::index_and_refs_unchanged`, `observe_readonly::configured_filters_never_run` (a same-size edit of a filtered file: the filter's marker absent, `dirty` `unknown`), `observe_readonly::filter_added_during_observation` (a driver configured at `observe.filters_listed`: `filters_changed`), `observe_readonly::no_lazy_fetch` (a local `--filter=tree:0` clone whose HEAD moved to a commit whose trees it lacks: its packs unchanged, `dirty` `unknown`), `observe::tests::promisor_below_2_45_1_reads_no_objects`, `observe_readonly::an_old_git_reads_no_objects_of_a_partial_clone` (a git reporting 2.44 that drops `GIT_NO_LAZY_FETCH`: no pack, no object read, the reason reported; the same git reporting 2.54 fetches), `observe_readonly::rename_detection_fetches_nothing` (a staged rename with an edit in a `--filter=blob:none` clone whose HEAD's blob is missing: no pack with the git under test, and none with a git reporting 2.45.0), `observe_readonly::a_partial_clone_filter_alone_makes_a_promisor`, `observe_readonly::a_worktree_config_promisor_is_checked_in_that_worktree` |
| A4 | When a reservation's runner is dead, cardinal shall release it only when its process group is empty or the boot changed, and recovery shall never signal the group. | G14 | `recovery::crashed_runner_group_is_kept_until_empty`, `recovery::unverified_group_not_signalled`, `recovery::reused_pgid_*`, `recovery::previous_boot_released` |
| A5 | If the hook runs on Windows, fails, reads malformed input, misses its deadline, or sees `permission_mode` `auto`, it shall exit 0 with empty stdout. | G4 | `hook_claude::never_blocks::*`, `hook_claude::auto_mode_not_rewritten`, `hook_claude::windows_not_rewritten` |
| A6 | When `act` runs, it shall compute the need for owner approval from the checks it evaluates, and shall reject a plan that needs it unless `--owner-approved` is given with `--caller owner`. | design §5 | `plan_act::approval_recomputed_when_merge_evidence_disappears` |
| A7 | `act` shall refuse a target that is a symbolic link or a mount point, that is on another device or mount than its parent or has a directory below it that is, or that contains another repository below its root. | G7 | `plan_act::nested_repository_refused`, `plan_act::mount_point_refused`, `act_snapshot::*`, `act_fs::mount_below_the_target_refused` |
| A8 | `act` shall refuse both plan kinds for a worktree under a managed root or locked by git. | design §4 | `plan_act::managed_worktree_refused`, `plan_act::managed_reclaim_dir_refused` |
| A9 | If the in-use lock cannot be taken within the wait cap, the runner shall run the stage and record `in_use_unprotected`. | G4 | `in_use::unprotected_run_recorded` |
| A10 | `act` shall perform no target deletion or relocation and shall leave worktree contents, Git linkage and Git administrative metadata unchanged, regardless of caller, flags, approval or unit configuration, and shall report a plan whose checks pass and whose approval is met as `dry_run` with `execution_not_designed`. | G5, G7 | `act_report_only::*`, `act_report_only::execution_not_designed_for_every_caller`, `act_single::act_neither_claims_nor_deletes`, `unit_dry_run::unit_with_act_true_is_recorded_as_act` |
| A11 | When stdin is a terminal, the runner shall run the stage without a process group or admission, holding the in-use lock, and record `tty_passthrough`. | G4 | `run_tty::passthrough` |
| A12 | If the runner's parent exits, the runner shall not start the stage, or, after the spawn, shall stop the group (on the fail-open and TTY paths, the stage, once the runner is not stopped, unless a signal the runner does not handle ends it first), and the reservation shall remain until the group is empty. | G14 | `run_parent_death::*` |
| A13 | When the stage dies by a signal, the runner shall die by the same signal. | G4 | `run_signals::same_signal_death` |
| A14 | `act` shall count `--squash-merged` as merge evidence only when its own observation at its checks finds `content_in_base` to be `yes`. | G3, G8 | `squash::*`, `squash::content_in_base_read_at_the_checks` (the plan made with `content_in_base: "yes"`; at `act.snapshotted` only the local base ref moves to a commit that changes a file the branch changed; `rejected` by `merge_evidence`, while the same run without the move reports `dry_run`) |
| A15 | If `--owner-approved` is given without `--caller owner`, the CLI shall exit 2 without evaluating the plan. | design §3 | `cli::owner_approved_needs_the_owner` |
| A16 | When a watched volume's free space is unknown, `disk-reclaim` shall not run on it, its disk state and disk items shall stay as they were, and cardinal shall record the failure. | design §7 | `disk_reclaim::unreadable_volume_skipped` (with an eligible control on a volume whose reading is known, which is reported in the same pass), `disk_state::failed_final_reading_keeps_the_items` |
| A17 | A watched volume shall enter `warning` below the warning line or when the last hour's readings predict the critical line within two timer intervals, enter `critical` below the critical line from either state, a first reading included, and leave pressure only above the recovery line; `disk-reclaim` shall run in `warning` and `critical` alike. | G2 | `disk_state::*`, `disk_state::first_reading_below_critical`, `disk_reclaim::runs_in_critical` |
| A18 | When the runner's job ends below the warning line and no pass ran in the last 60 s, it shall start `cardinal reconcile` detached only after the stage's status is read and recorded, and a failure of the check shall not change the stage's output or status. | G2, C18 | `run_disk_trigger::starts_after_the_status_is_recorded` (the attempt at `run.reconcile_spawning` observed, and only after `run.finished`), `run_disk_trigger::failure_changes_nothing` |
| A19 | One `reconcile` pass and one arbiter launcher shall run at a time; an arbiter pass shall start only for due items (never assigned to a pass, or assigned to one that ended at least `retry_s` ago) and at most once per `min_interval_s`, and shall assign them to itself durably before its session starts; with `--assigned`, the session shall list and answer only items assigned to its own pass, and none once that pass has ended, and the launcher shall write the pass's outcome only into them; a launcher that dies while assigning or recording shall strand no item: an item never assigned stays due, and an assigned one keeps the outcome written into it or gets `abandoned`. | G8 | `reconcile::one_pass_at_a_time`, `arbiter_launch::due_items_only`, `arbiter_launch::assigned_items_only`, `arbiter_launch::retried_after_the_cooldown`, `arbiter_launch::killed_before_assigning`, `arbiter_launch::outcomes_kept_when_abandoned` |
| A20 | While its launcher lives, an arbiter session shall be killed at `timeout_s`; a pass whose launcher died shall count as running until `timeout_s` after its start and then be recorded `abandoned`, with nothing signalled; three failed passes in a row shall raise `arbiter_failing`. | G8 | `arbiter_launch::timeout_kills_the_session`, `arbiter_launch::launcher_killed`, `arbiter_launch::failing_raised` |
| A21 | The web UI page shall be written from the state cardinal already holds, without git, gh, or network calls, and shall open with no network. | design §12 | `ui::*` |
| A22 | When observation finds unclassifiable ignored data in a worktree, or cannot read part of its ignored data, `reconcile` shall raise one item per worktree, set of entries, and occurrence, with the paths and the observation errors, and end it when the worktree is gone or its set has changed; a set found again after its occurrence ended shall raise a new item and leave the old one as it was. | design §7, G8 | `inbox_raisers::unclassifiable_data_once_per_set`, `inbox_raisers::unclassifiable_data_cleared`, `inbox_raisers::unclassifiable_data_superseded`, `inbox_raisers::unclassifiable_data_set_returns` (sets A, B, A: three items, a dismissal of the first kept) |
| A23 | When a `reconcile` pass finds a watched volume under pressure, it shall write that volume's disk items with its disk report (the last successful reading, the time of the checks, the targets in order, any shortfall, and the blocked entries by worktree and path) before it runs the arbiter trigger or writes the page; disk items shall go to the owner and start no arbiter pass, and `disk_critical` shall end when a successful reading finds the volume out of `critical`. | G2, G8 | `reconcile::disk_items_complete_before_the_trigger`, `arbiter_launch::disk_items_start_no_pass`, `disk_state::critical_ends_above_the_critical_line` |
| A24 | When `cardinal plan` runs without a target, or a `reconcile` pass builds its plans, it shall make a plan for every linked worktree and every ignored entry matching a regenerable pattern, whatever its checks find. | G6, G7, G8 | `plan_act::bulk_plans_every_worktree`, `plan_act::protected_target_dir_planned_and_refused` (refused by `no_protected` alone) |

Out of scope for phases 0 and 1: C3 and C4 (phase 2, memory pressure and accepted jobs), C7 (phase 3, WSL), C11 (with executing reclamation, section 9.7), C21 (phase 2, measurement kinds).

## 17. Issues with the design

1. **Permission rules see the rewritten command.** Claude Code evaluates permission rules against `updatedInput`, not the original command ([hooks reference](https://code.claude.com/docs/en/hooks)), which design §6 did not account for. In Claude Code 2.1.322 the `if … then … else … fi` form is split per branch, so deny rules on the original still match its fallback copy. An allow rule for the runner would also let an agent run any command directly through it, so cardinal ships none and delegates approval to Claude Code: in `default` and `acceptEdits` modes each rewritten call needs one approval, and `auto` mode is not rewritten (section 6.5).
2. **Running the whole command string in a child shell changed its meaning.** Claude Code keeps the Bash tool's working directory across calls and applies aliases, functions, and options captured from the user's rc file ([tools reference](https://code.claude.com/docs/en/tools-reference)). A child shell drops the persisted `cd`, rc options such as `pipefail`, and the short-circuit structure around the heavy command, and changes the command's first word, on which timeout handling depends. Resolution: only classified stages are wrapped, in place (section 6.4); design §6 "Command capture" and C6 now say so. Residual: an rc alias or function shadowing a classified tool (section 7.5).
3. **The Bash timeout does not kill.** design §6 bounds the wait by the Bash tool timeout. Claude Code moves a foreground command that reaches its timeout to the background instead of stopping it ([tools reference](https://code.claude.com/docs/en/tools-reference)), so an overrun costs the agent a background task, not the command. The bound is kept (section 5.4); design §14's first question is answered by section 6.1.
4. **Stopping "the recorded process group" needs an identity that survives a crash.** design §6 says admission or a reconciler stops a dead runner's group before releasing it. A child-led group's PGID is unknown until after the spawn, and a PGID can be reused once its group empties. Resolution: the runner leads the group, so the PGID is recorded before the reservation exists; the reservation is released only when the group is empty or the boot changed; recovery never signals a dead runner's group, because its ID can pass to an unrelated group between a check and a signal (section 4.5). design §6 and C8 now say so.
5. **G4 has zero-run cases.** A runner killed between its start and the spawn, and a runner path that is executable but cannot be loaded, run the stage zero times. They are accepted because they end with a visible non-zero status, never run the stage twice, and never report success; a shell-side fallback on the exit status would run the stage twice after a real failure (section 5.3).
6. **Reclaim safety cannot depend on the in-use lock, nor on what cardinal can see.** design §7 and C15 tie reclamation to the in-use lock, but G4 requires a stage to run even when its runner cannot take that lock (section 4.6), and a process that holds no lock is invisible to it. Reading working directories and open descriptors extends what cardinal sees, but not to every way a process uses a directory (section 9.7, requirement 5): for regenerable data a missed process costs a rebuild, for a worktree it loses work. Resolution: phase 1 executes nothing (item 17), and the in-use lock only marks a worktree in use for reports; the execution design must establish that a target is unused without relying on the lock alone. Proposed for design §7 and C15: name the lock as what makes commands wait, not what makes reclamation safe, name the boundary of what cardinal can see, and treat worktrees used through containers or VMs as managed.
7. **No accepted jobs before phase 2.** design §6's step for a wait that reaches the cap is an accepted job, which is phase 2. Until then the runner starts the stage over budget instead (A1). This weakens G1 until phase 2, which phases 0 and 1 do not claim.
8. **`git status` writes.** Without `--no-optional-locks`, `git status` refreshes and rewrites the index of the worktree it observes. Observation always disables optional locks (A3) so that observing does not change repositories.
9. **"No upstream" is an invariant.** G6 and G10 make a branch without upstream unreclaimable, so `act` refuses it even with owner approval. A worktree whose work was never pushed has to be pushed or removed by hand. This is intended: it also protects freshly created worktrees.
10. **Windows needs its own contract.** Git Bash converts arguments for native programs, Rust does not resolve extensionless `.cmd` shims, and Claude Code on Windows without Git Bash runs a separate PowerShell tool, which design §9 does not name. Phase 0 does not rewrite on Windows; phase 3 defines the contract and adds Job Objects.
11. **Hook defaults are unsafe for cardinal.** Command hooks default to a 600 s timeout, and several hooks returning `updatedInput` have no defined order. The installation sets explicit timeouts and requires cardinal to be the only Bash-rewriting hook (section 6.2).
12. **Process groups do not track processes that leave them.** A tool that calls `setsid` or `setpgid` for its own children takes them out of the job's group; the reservation can be released while they run.
13. **A squash merge has no evidence the arbiter may act on.** design §7 leaves inferring a squash merge to the arbiter, design §5 lets the arbiter reclaim without approval only worktrees that meet every reclamation condition, the first of which is evidence that HEAD was merged (an ancestor of the base, or the head of a merged pull request), and G3 and G8 expect a squash-merged worktree reclaimed without the owner. A squash merge whose pull request head is not the worktree's HEAD (the branch got commits on GitHub after the last pull, from "Update branch" or a bot) or that was made without a pull request (`git merge --squash`) has neither, so every such worktree would need the owner. Resolution: the arbiter's judgment counts as evidence only through `--squash-merged`, which `act` accepts only when its own observation finds every file the branch changed identical in the base (`content_in_base`, section 9.3); every invariant still applies, and the branch is kept. Proposed for design §7: "or the arbiter judged a squash merge and every file the branch changed is identical in the base". In phase 1 the judgment changes reports only, because phase 1 executes nothing (item 17). Section 14.5 lists it as an open decision.
14. **G2 has no immediate trigger.** design §9 rules out a resident process, so a crossing of the warning line is noticed at the end of a cardinal job and at each OS timer pass (section 10.3); growth inside a long job that cardinal runs is noticed only by the timer until the runner samples in phase 2. A pass that finds another one running exits, so the report of a crossing can come one timer interval and two pass lengths after it (section 10.4), and executing would add its deletion time. G2's "before space runs out" therefore holds when the room between the warning and critical lines exceeds the growth over that time; the prediction of section 10.4 moves the crossing earlier when growth is fast. Proposed for design §7 and G2's verification: name the growth rate, interval, and pass length the warning line is set for (the check of section 15 uses 4 MiB/s, 60 s, and 400 MiB).
15. **APFS keeps deleted blocks for local snapshots** (design §14, third question). Blocks a local snapshot (Time Machine) still references are freed only when the snapshot goes, so a deletion can free nothing while one exists; important-usage capacity counts purgeable space, which the system frees only when a write needs it. On one host's volume without local snapshots, important usage read about 10% more than `statfs` and opportunistic usage about 30% less. cardinal measures with important usage, the space a write can get, and never deletes snapshots (`tmutil` does). The question stays open for a host with local snapshots, where section 14.4's run records the numbers. On such a volume deleting a reserve file frees nothing either; the execution design answers that for C11 (section 9.7, requirement 10).
16. **Regenerable data in app-managed worktrees is never reclaimed.** design §4 makes worktrees created by T3 Code report-only, and A8 refuses `reclaim_dir` inside them as well as the whole worktree, because the app runs its sessions and servers there. On a host where most worktrees are T3 Code's, their `node_modules` and `target/` are out of reach, and G2 can fall short even with reclaimable data on the volume. Deleting a regenerable directory changes no state the app keeps, and `not_in_use` already refuses a worktree with a process inside it. Open (section 14.5): whether `reclaim_dir` may run in managed worktrees; design §4 would then name removing the worktree, not its regenerable data, as the operation left to the app. Until then they stay report-only.
17. **Phase 1's first release executes no reclamation.** design §12 lists G2 and G3 among phase 1's conditions, and design §7 has cardinal reclaim. Executing safely needs what section 9.7 lists: every deletion bound to what was checked, git's classification included; users of the target that no check sees; recovery that deletes nothing written later; space to finish on a full volume; and, for whole worktrees, an idle state every user honours, the worktree's git ID serialized against git, and its per-worktree git state classified as disposable. No design in this document meets them. Resolution: the first release detects and reports; `act` evaluates every plan and reports `execution_not_designed`, `reconcile` raises `reclaimable` and disk items, and the owner reclaims. G2 is met as detection, report, and inbox, G3 in its report part ("in dry-run, they report it"), and the controls of G6, G7, and G10 report `dry_run` instead of being reclaimed. #33 adds executing regenerable directories later in phase 1, which completes G2 and G7; executing whole worktrees, and with it G3's execution part, stays deferred (section 14.5). Proposed for design §12: move G3's execution part to the phase that executes whole worktrees, and keep its report part in phase 1.
