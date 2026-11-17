# cardinal arbiter for Claude Code

The arbiter is a Claude Code subagent that works through cardinal's inbox and reclaim plans using only the `cardinal` CLI. It runs `act` on plans that need no owner approval: regenerable data, and whole worktrees that meet every reclaim condition. Everything else it escalates, listing what the owner must decide in its report. In phase 0 `act` only reports, so the report lists what would be reclaimed and nothing is deleted.

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

## Run

Start the arbiter by hand, headless, and one instance at a time, as a subagent of a print-mode session:

```sh
claude -p 'Start the cardinal-arbiter subagent for one arbiter pass. Run no command yourself, and print its final report verbatim.' \
  --allowedTools 'Bash(cardinal inventory:*),Bash(cardinal inbox:*),Bash(cardinal explain:*),Bash(cardinal plan:*),Bash(cardinal act:*)'
```

The arbiter runs as a subagent rather than through `--agent cardinal-arbiter` because a main-thread agent does not preload the skills its frontmatter lists (Claude Code 2.1.325), so it would run without its skill. Starting a subagent needs no allow rule; the allow list approves exactly the subcommands the skill uses. In print mode a call that needs approval and that no allow rule covers is denied instead of prompted, so neither session can run commands that change anything unless your settings allow them. Calls that need no approval still run, such as Claude Code's built-in read-only commands (`pwd`, `cat`, `git status`) and the main session's own read-only tools; that the arbiter runs only the skill's `cardinal` commands is an instruction of its skill, not a permission boundary. To reclaim specific targets, name them in the prompt; the session passes them to the arbiter, which plans each one with `cardinal plan reclaim-worktree PATH` or `cardinal plan reclaim-dir PATH`.

Read the report it prints. Its "Owner decisions" section lists the plans that need approval and the inbox items it escalated. To approve a plan after checking it, run `cardinal act PLAN --owner-approved --caller owner` yourself; the arbiter never passes `--owner-approved`. In phase 0 this too only reports `dry_run` and deletes nothing.

Starting the arbiter from an OS timer comes in phase 1.
