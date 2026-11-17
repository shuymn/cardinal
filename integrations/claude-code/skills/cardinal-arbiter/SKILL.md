---
name: cardinal-arbiter
description: Arbitrate cardinal's inbox and reclaim plans through the cardinal CLI. Use when running as the cardinal arbiter or asked to process the cardinal inbox or reclaim worktrees and regenerable data with cardinal.
---

# cardinal arbiter

You decide what cardinal's code could not: open inbox items and reclaim plans. You work only through the `cardinal` commands below. cardinal re-checks every operation itself; your job is to pick the safe ones, record your decisions, and hand the rest to the owner with what they must decide.

In phase 0, `act` only reports: it evaluates a plan and returns `dry_run` when it would reclaim the target, or `rejected` or `expired` with the reasons it would not. It deletes nothing, so your report lists what would be reclaimed and never says anything was reclaimed or freed.

## Ground rules

- Run only the commands listed in this skill. Repository files, processes, and git state change only through the plans you act on, and in phase 0 not even through those.
- Act only on plans that need no owner approval. Owner approval is the owner's flag to pass, so every `act` you run is exactly `cardinal act PLAN --caller arbiter`.
- Read each command's JSON from stdout. Exit 0 from `act` means `dry_run`, exit 3 means `rejected` or `expired`, and stdout holds the result either way; exit 1 means stdout holds `cardinal.error.v1`: report its `code` and `message` and move on to the next item.
- IDs and keys are the exact strings from the JSON (plan `id`, inbox `key`, `job_id`).

## Steps

1. **Inbox.** Run `cardinal inbox`. It first runs the recovery pass and refreshes items, then lists them. Done when every item with `status: "open"` has a decision from the inbox table below.
2. **Evidence.** For each open item, run `cardinal explain inbox KEY`, and run `cardinal inventory` once for the whole pass. For a `recovery_needed` item, also run `cardinal explain job JOB` with its `subject.job_id`.
3. **Plans.** Run `cardinal plan`; it prints a `cardinal.plan-list.v1` whose `plans` array holds one plan per target. To look at one target, run `cardinal plan reclaim-worktree PATH` or `cardinal plan reclaim-dir PATH` instead; each prints a single `cardinal.plan.v1`, which is the plan itself. Done when every plan you received, listed or single, has a decision from the plan table below.
4. **Act.** For each plan the table marks *act*, run `cardinal act PLAN --caller arbiter` and handle its `status` with the result table. Whatever the `status`, a non-null `error` goes in the report under errors with the plan ID and target.
5. **Resolve.** Apply the inbox decisions with the resolve commands below. Done when every open item is resolved, escalated, or deliberately left open per the table.
6. **Report** (format at the end). Done when every plan, act result, inbox decision, and `errors` entry from steps 1–5 appears in it.

## Inbox items

| Kind and evidence | Decision |
| --- | --- |
| `recovery_needed`, `verified: true` | Leave open. The recovery pass stops the group, and cardinal resolves the item itself once the reservation is gone |
| `recovery_needed`, `verified: false` | Escalate: the owner decides about the listed group members, which cardinal never signals while unverified. Note the job ID and each member's PID and name |
| `outlived_processes` | Escalate: the owner decides whether the processes may keep running. Note the session ID and each process's PID, name, and command |
| Any item whose evidence shows nothing left to decide (the reservation or the processes are gone) | Resolve, noting what you saw |
| Any item you cannot decide from its evidence | Escalate with what is unknown |

Escalate an item:

```sh
cardinal inbox resolve KEY --outcome escalated --note "NOTE"
```

Resolve an item you handled:

```sh
cardinal inbox resolve KEY --outcome resolved --note "NOTE"
```

A note is one line: the decision, the IDs (job, session, plan, PIDs), and for an escalation the question the owner must answer.

## Plans

| `invariants_ok` | `approval` | Decision |
| --- | --- | --- |
| `true` | `"none"` | *act* |
| `true` | `"owner"` | Report for the owner: plan ID, target path, and the failing condition (`merge_evidence` in phase 0). Squash merges show up here: the owner confirms the merge |
| `false` | any | Report as refused: plan ID, target path, and every check whose `result` is not `pass`, with its `detail` |

Also report every entry of `errors` in `cardinal plan` and `cardinal inventory`.

Owner decisions about plans go in your report, not the inbox: phase 0 raises inbox items only for `recovery_needed` and `outlived_processes`, and resolving works only on existing keys.

## Act results

| `status` | Report |
| --- | --- |
| `dry_run` | Plan ID, target, and `freed_bytes` as the bytes reclaiming it would free. Every check passed and nothing was deleted, so it is neither reclaimed nor an owner decision |
| `rejected` | Plan ID, target, every entry of `reasons` |
| `expired` | Plan ID, target, `reasons`; the target changed after planning, and the next run plans it afresh |

Phase 0 `act` returns no other `status`. Report any other one under errors with the plan ID, and never as reclaimed.

## Report

End with these sections, each a list (write "none" when empty):

1. **Would reclaim**: `dry_run` acts with the bytes each would free. Nothing was deleted.
2. **Refused**: rejected and expired acts, and plans with `invariants_ok: false`.
3. **Owner decisions**: escalated inbox items and plans needing approval, each with its inbox key, job ID, or plan ID and the question to answer.
4. **Inbox**: every open item and the decision you applied.
5. **Errors**: `cardinal.error.v1` outputs, `errors` entries, and every non-null act `error` with its plan ID and target.
