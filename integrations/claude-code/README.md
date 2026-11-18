# cardinal arbiter for Claude Code

The arbiter is a Claude Code subagent that works through cardinal's inbox and reclaim plans using only the `cardinal` CLI. cardinal's timer starts it for the inbox items that need it, and you can start it yourself. It runs `act` on plans that need no owner approval, judges squash merges from their evidence, and escalates everything else, listing what you must decide in its report. In phase 1 `act` only reports, so the report lists what would be reclaimed and nothing is deleted.

| File | Installs to |
| --- | --- |
| `agents/cardinal-arbiter.md` | `~/.claude/agents/cardinal-arbiter.md` |
| `skills/cardinal-arbiter/SKILL.md` | `~/.claude/skills/cardinal-arbiter/SKILL.md` |

## Install

Copy both files into your user-level Claude Code directory, never into a repository, so cardinal adds nothing to the repositories it manages:

```sh
mkdir -p ~/.claude/agents ~/.claude/skills/cardinal-arbiter
cp integrations/claude-code/agents/cardinal-arbiter.md ~/.claude/agents/
cp integrations/claude-code/skills/cardinal-arbiter/SKILL.md ~/.claude/skills/cardinal-arbiter/
```

`cardinal` must be on `PATH`. The hook setup is separate; see `examples/claude/settings.json`. When a rewritten Bash call asks for approval, Claude Code also offers "Yes, and don't ask again" for the runner's command prefix (`<runner> run *`); accepting it lets an agent run any command through the runner regardless of your permission rules ([architecture §6.5](../../docs/architecture.md#65-permissions)), so answer that prompt with plain "Yes".

## The timer starts the arbiter

The timer starts the arbiter: a launchd agent (macOS) or a systemd user timer (Linux) runs `cardinal reconcile --trigger timer` every few minutes. You install it by hand from `examples/launchd/` or `examples/systemd/` into your user-level service configuration; cardinal never writes it. At the end of each pass, when an inbox item is due and the last arbiter pass started at least `[arbiter] min_interval_s` ago (30 minutes by default), `reconcile` starts `cardinal arbiter run` and does not wait for it.

`cardinal arbiter run` records a pass in `arbiter/last.json` in the state dir, assigns it the due items, and runs `[arbiter] command` of `host.toml`, by default this launch:

```sh
claude -p 'Start the cardinal-arbiter subagent for one arbiter pass. Run no command yourself, and print its final report verbatim.' \
  --allowedTools 'Bash(cardinal inventory:*),Bash(cardinal inbox:*),Bash(cardinal explain:*),Bash(cardinal plan:*),Bash(cardinal act:*)'
```

The session runs in `arbiter/` in the state dir, outside every repository, with the timer's environment and `CARDINAL_ARBITER_PASS` naming its pass, which the Bash calls of its subagent inherit. It is stopped `[arbiter] timeout_s` (20 minutes by default) after the pass started, or as soon as it writes more than 1 MiB to standard output or to standard error. Once it has ended, cardinal writes its log, `arbiter/<pass>.log`: standard output followed by standard error, the first 1 MiB of the two together, then cardinal's own notes. A session that was stopped leaves only those notes, not its output. The arbiter reads its items with `cardinal inbox --assigned` and answers only those. An item is due when no pass has had it, and again `[arbiter] retry_s` (2 hours by default) after its last pass ended, however that pass ended. Disk items, `arbiter_failing`, and `reclaim_stranded` are yours and never start a pass; three failed passes in a row raise `arbiter_failing`. Each pass costs model usage, at most one pass per interval.

The timer's environment must find `claude` and `cardinal` on its `PATH`: the session's Bash calls run `cardinal` from it. Put an absolute path in `[arbiter] command` when `claude` is elsewhere.

## Run it yourself

Start the arbiter by hand with the same launch, headless and one instance at a time. Without `CARDINAL_ARBITER_PASS`, the arbiter follows your request instead of a pass's items: it reads the whole inbox, `cardinal inventory`, and `cardinal plan`. To look at specific targets, name them in the prompt; the session passes them to the arbiter, which plans each one with `cardinal plan reclaim-worktree PATH` or `cardinal plan reclaim-dir PATH`.

The arbiter runs as a subagent rather than through `--agent cardinal-arbiter` because a main-thread agent does not preload the skills its frontmatter lists (Claude Code 2.1.325), so it would run without its skill. Starting a subagent needs no allow rule; the allow list approves exactly the subcommands the skill uses. In print mode a call that needs approval and that no allow rule covers is denied instead of prompted, so neither session can run commands that change anything unless your settings allow them. Calls that need no approval still run, such as Claude Code's built-in read-only commands (`pwd`, `cat`, `git status`) and the main session's own read-only tools; that the arbiter runs only the skill's `cardinal` commands is an instruction of its skill, not a permission boundary.

## Read the report

Read the report it prints, or a timer pass's log. Its "Owner decisions" section lists the plans that need approval and the inbox items it escalated. To approve a plan after checking it, run `cardinal act PLAN --owner-approved --caller owner` yourself; the arbiter never passes `--owner-approved`. In phase 1 this too only reports `dry_run` and deletes nothing.
