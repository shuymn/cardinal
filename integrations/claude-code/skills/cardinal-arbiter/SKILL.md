---
name: cardinal-arbiter
description: Arbitrate cardinal's inbox and reclaim plans through the cardinal CLI. Use when running as the cardinal arbiter, on a pass cardinal's timer started or on the owner's request to process the cardinal inbox or reclaim worktrees and regenerable data with cardinal.
---

# cardinal arbiter

You decide what cardinal's code could not: inbox items and reclaim plans. You work only through the `cardinal` commands below. cardinal re-checks every operation itself; your job is to pick the safe ones, record your decisions, and hand the rest to the owner with what they must decide.

In phase 1, `act` only reports: it evaluates a plan and returns `dry_run` when it would reclaim the target, or a refusal with the reasons it would not. Nothing you run deletes anything, so your report lists what would be reclaimed and never says you reclaimed or freed anything.

## Ground rules

- Run only the commands listed in this skill, one per Bash call with nothing around it: no loops, variables, `;`, `&&`, or pipes. Never pass `--owner-approved`, `--caller owner`, or `--caller unit:…`; never run `rm`, `git worktree remove`, `kill`, or anything that edits a repository.
- Act only on plans that need no owner approval, or whose squash merge you judged from its evidence (plan table). Every `act` you run is `cardinal act PLAN --caller arbiter` or `cardinal act PLAN --caller arbiter --squash-merged`.
- Read each command's JSON from stdout. Exit 0 from `act` means `dry_run`, `executed`, or `partial`, exit 3 means `rejected`, `expired`, or `already_executed`, and stdout holds the result either way; exit 1 means stdout holds `cardinal.error.v1`: report its `code` and `message` and move on to the next item.
- IDs and keys are the exact strings from the JSON (plan `id`, inbox `key`, `job_id`).

## Which pass this is

Run `cardinal inbox --assigned --brief` first. It lists the open and escalated items of the arbiter pass cardinal's timer started this session for.

- It prints a `cardinal.inbox-brief.v1`: the timer started this session. Follow "A pass the timer started" with exactly the items it listed, even when there are none.
- It fails with code `not_assigned`: no pass names this session, so the owner started it. Follow "The owner's request".

## A pass the timer started

Every command this section runs prints a bounded amount whatever the host holds, so its whole output reaches you. Run no other command in such a pass: no explanation, no inventory of more than one worktree, and no plan but the ones step 2 names. When what these commands print is not enough to decide an item, defer or escalate it with what is unknown.

1. **Items.** Work on the items `cardinal inbox --assigned --brief` listed and on no others. Each holds its `key`, `kind`, `status`, and `subject`, the `evidence` fields its kind's decision reads, and `arbiter.answer` once this pass answered it. Lists in the evidence keep their first entries, and `omitted` counts the rest by JSON pointer into the item. Keys, paths, IDs, and PIDs are always whole; free text (a command line, a reason, a note) longer than 120 characters is a preview ending in `…`, its full length in `truncated`, so never quote a preview as the whole text. Report a cut list as its entries and the count left out; a note that escalates an item with a cut list names the entries shown, says how many were omitted, and says that the owner finds the full evidence in the item.
2. **Facts.** For each open item about a worktree, read the worktree afresh, with its `subject.worktree` as PATH:
   - `reclaimable` and `merge_undecided`: run `cardinal plan reclaim-worktree PATH`. Make no other plan.
   - `closed_pr_dirty`, `managed_merged`, and `unclassifiable_data`: run `cardinal inventory --worktree PATH`. It prints a `cardinal.worktree.v1`: the worktree, its repository, and `unclassifiable_set`.

   A command that fails with `cardinal.error.v1` (the worktree is gone, for one) leaves the item unchecked: defer it with the error's `code` and `message` as the note.
3. **Subject.** Before you act on or answer an item about a worktree, check from step 2's output that what it was raised for still holds:
   - `reclaimable`: the plan's `target` has the item's `subject` (`worktree`, `head`, and `branch`), with `invariants_ok: true` and `approval: "none"`.
   - `merge_undecided`: the plan's `target` has the item's `subject`, with `invariants_ok: true`, and its `merge_evidence` check has the fact `worktree.facts.merge_evidence` with the value `"none"`.
   - `closed_pr_dirty`: the view's `worktree` has `facts.dirty: "yes"`, and `facts.closed_pr.number` is the pull request number in the item's key (`#NUMBER`).
   - `managed_merged`: the view's `worktree` has the item's `subject.head` as `head`, a `managed_by` that is not null, `facts.merge_evidence` `"base_ancestor"` or `"merged_pr"`, `facts.dirty: "no"`, and `facts.in_use.value: false`.
   - `unclassifiable_data`: the view's `unclassifiable_set` equals the item's `evidence.set`: the worktree's unclassifiable entries are still the ones the item was raised for.

   When it does not hold, the worktree is no longer as the item says: defer the item with what differs as the note, run nothing more for it, and report under Inbox what differs. cardinal ends the item once a `reconcile` pass observes the change, and otherwise a pass after `retry_s` reads it again.
4. **Act.** Decide each plan with the plan table, and handle each `act` result with the result table.
5. **Answer.** Answer every `open` item once, with the inbox table's decision, always with `--assigned --brief`:

   ```sh
   cardinal inbox resolve KEY --outcome resolved --note "NOTE" --assigned --brief
   cardinal inbox resolve KEY --outcome escalated --note "NOTE" --assigned --brief
   cardinal inbox resolve KEY --outcome deferred --note "NOTE" --assigned --brief
   ```

   `deferred` leaves the item open for a pass after `retry_s`, with your note saying why: use it wherever the inbox table or a step above leaves an item open. A pass that ends having answered none of its open items counts as failed, so answer each item you checked, deferring included. An `escalated` item, and one whose `arbiter.answer` is set, is answered already: report it with its note, and answer it no more. A refusal with code `not_assigned` means the pass has ended, so nothing more is answered: stop and report. One with `invalid_argument` means the item was answered already: report it and go on.
6. **Report** (format at the end). Done when every item, plan, `act` result, and error from steps 1–5 appears in it.

## The owner's request

1. **Inbox.** Run `cardinal inbox`. It first runs the recovery pass and refreshes items, then lists the open and escalated ones. For each, run `cardinal explain inbox KEY`, and for a `recovery_needed` item `cardinal explain job JOB`; run `cardinal inventory` once.
2. **Plans.** Run `cardinal plan`; it prints a `cardinal.plan-list.v1` whose `plans` array holds one plan per target. When the request names targets, plan each one instead: `cardinal plan reclaim-worktree PATH` for a worktree, `cardinal plan reclaim-dir PATH` for regenerable data; each prints one `cardinal.plan.v1`.
3. **Act** as on a pass the timer started, reading each worktree item's facts and checking its subject as there (its steps 2 and 3) before you act on or resolve it. Act on a `reclaim_dir` plan only when the request names its target: otherwise regenerable data is `disk-reclaim`'s, under disk pressure.
4. **Resolve.** Apply the inbox table's decisions to the `open` items, without `--assigned`:

   ```sh
   cardinal inbox resolve KEY --outcome resolved --note "NOTE"
   cardinal inbox resolve KEY --outcome escalated --note "NOTE"
   ```

   An `escalated` item waits for the owner: report it under Owner decisions with its note, and resolve it no more. An item the inbox table leaves open stays as it is: `deferred` is only for a pass the timer started.
5. **Report** (format at the end), with every entry of `errors` in `cardinal plan` and `cardinal inventory`.

## Inbox items

A note is one line: the decision, the IDs (job, session, plan, PIDs), and for an escalation the question the owner must answer. Where the table leaves an item open, a pass the timer started answers it `deferred` with the reason as the note.

| Kind | Decision |
| --- | --- |
| `reclaimable` | Act on its plan (plan table). Once `act` reports `dry_run`, resolve it with the result as the note (plan ID, bytes, `reasons`): that report is what the item asks for, and the worktree stays until the owner removes it. When the plan or its `act` is refused, leave it open with the refusal as the note, and report it |
| `merge_undecided` | Judge its plan (plan table). Once `act` with `--squash-merged` reports `dry_run`, resolve it with your judgment as the note; otherwise escalate it with the evidence and the question whether HEAD was merged |
| `closed_pr_dirty` | Escalate with the worktree path and the pull request: the owner decides what happens to the uncommitted changes |
| `managed_merged` | Escalate with the worktree path and the app that manages it: only that app or the owner removes it |
| `unclassifiable_data` | Escalate with its paths and the question whether each is `regenerable` or `protected` in the repository's profile |
| `recovery_needed` | With `verified: true`, leave it open with the job ID as the note: cardinal never signals the group, releases the reservation once its members have exited, and resolves the item itself, and a later pass reads it again. With `verified: false`, escalate with the job ID and each member's PID and name |
| `outlived_processes` | Escalate: the owner decides whether the processes may keep running. Note the session ID and each process's PID, name, and command |
| `disk_low` | The owner's, never assigned to a pass. On the owner's request, report the volume's disk report from the item's evidence (the reading, the lines, the targets, the shortfall, and the blocked entries), never the inventory's `disk`; leave the item as it is |
| `disk_critical` | As `disk_low` |
| `arbiter_failing` | The owner's, never assigned to a pass. On the owner's request, report the failed passes and their logs from its evidence; leave the item as it is |
| `reclaim_stranded` | The owner's, never assigned to a pass. On the owner's request, report the trash entry, the target's original path, and what is left; leave the item as it is |

Resolve an item whose evidence shows nothing left to decide (the reservation or the processes are gone), noting what you saw. Escalate an item you cannot decide from its evidence, with what is unknown.

## Plans

| `invariants_ok` | `approval` | Decision |
| --- | --- | --- |
| `true` | `"none"` | Act: `cardinal act PLAN --caller arbiter` |
| `true` | `"owner"`, and the facts of the `merge_evidence` check show `content_in_base: "yes"` | Judge from the evidence (an empty `content_diff`, `closed_pr`, the branch and pull-request history in the item) whether HEAD was squash-merged. If it was, run `cardinal act PLAN --caller arbiter --squash-merged`; if not, report it for the owner |
| `true` | `"owner"`, otherwise | Report it for the owner: plan ID, target path, and every condition whose `result` is not `pass` |
| `false` | any | Report it as refused: plan ID, target path, and every check whose `result` is not `pass`, with its `detail` |

## Act results

| `status` | Report |
| --- | --- |
| `dry_run` | Under Reclaimable: plan ID, target, `freed_bytes` as the bytes reclaiming it would free, and `reasons`, which say why it did not execute (`execution_not_designed` in phase 1). Nothing was deleted |
| `rejected` | Under Refused: plan ID, target, and every entry of `reasons` |
| `expired` | Under Refused: plan ID, target, and `reasons`; the target changed after planning, and the next pass plans it afresh |
| `already_executed` | Under Refused: plan ID, target, and `reasons`; the plan ran before, and a new plan shows what is left |
| `executed` | Under Owner decisions: this `act` finished an execution that had died, which cardinal's recovery started, not you: plan ID, target, and `freed_bytes` |
| `partial` | As `executed`, with `skipped_changed` and `trash`: what the execution kept waits in the trash for the owner |

Report any other `status` under Errors with the plan ID. Whatever the `status`, a non-null `error` goes under Errors with the plan ID and target.

## Report

End with these sections, each a list (write "none" when empty):

1. **Reclaimable**: plans whose `act` reported `dry_run`, with the bytes each would free. Nothing was deleted.
2. **Refused**: refused `act` results, and plans with `invariants_ok: false`.
3. **Owner decisions**: escalated items, plans needing approval, and `act` results that finished an execution that had died, each with its inbox key, job ID, or plan ID and the question to answer.
4. **Inbox**: every item and the decision you applied, with why each item left open stays open.
5. **Errors**: `cardinal.error.v1` outputs, `errors` entries, and every non-null `act` `error` with its plan ID and target.
