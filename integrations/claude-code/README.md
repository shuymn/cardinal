# cardinal arbiter for Claude Code

The arbiter is a Claude Code subagent that works through cardinal's inbox and reclaim plans using only the `cardinal` CLI. It acts on plans that need no owner approval: regenerable data, and whole worktrees that meet every reclaim condition. Everything else it escalates, listing what the owner must decide in its report.

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

`cardinal` must be on `PATH`. The hook setup is separate; see `examples/claude/settings.json`.

## Run

Start the arbiter by hand, headless, and one instance at a time:

```sh
claude -p 'Run one arbiter pass.' --agent cardinal-arbiter \
  --allowedTools 'Bash(cardinal inventory:*),Bash(cardinal inbox:*),Bash(cardinal explain:*),Bash(cardinal plan:*),Bash(cardinal act:*)'
```

The allow list covers exactly the subcommands the skill uses. In print mode a tool call that no allow rule covers is denied instead of prompted, so the arbiter reaches nothing else unless your user-level settings allow it. To reclaim specific targets, name them in the prompt; the arbiter plans each one with `cardinal plan reclaim-worktree PATH` or `cardinal plan reclaim-dir PATH`.

Read the report it prints. Its "Owner decisions" section lists the plans that need approval and the inbox items it escalated. To approve a plan after checking it, run `cardinal act PLAN --owner-approved --caller owner` yourself; the arbiter never passes `--owner-approved`.

Starting the arbiter from an OS timer comes in phase 1.
