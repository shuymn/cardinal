//! The command line: clap definitions for every phase 0 command and flag
//! (`docs/architecture.md` section 11.2) and thin handlers that print JSON.
//!
//! Exit codes (section 11.1): 0 success, 1 error with `cardinal.error.v1` on stdout, 2 usage
//! error, 3 `act` did not execute. `run` exits with the stage's status (section 5.6), and
//! `hook` always exits 0, also on a usage error or a panic, because a non-zero hook exit can
//! block the agent's tool call.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::act::{self, ActOptions};
use crate::adapter::claude;
use crate::config::{self, Config};
use crate::contract::{Caller, Error, ErrorCode, InboxStatus, PlanKind, ResolutionOutcome};
use crate::explain::{self, Subject};
use crate::inbox::{self, StatusFilter};
use crate::observe::{self, InventoryOptions};
use crate::os::child;
use crate::plan;
use crate::run;
use crate::state::StateDir;

/// Local compute-resource arbitration and housekeeping for parallel coding agents.
#[derive(Debug, Parser)]
#[command(name = "cardinal", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one stage under admission; rewritten commands call this
    Run(RunArgs),
    /// Handle an agent hook event
    Hook(HookArgs),
    /// Report reservations, repositories, worktrees, and outlived processes
    Inventory(InventoryArgs),
    /// List pending decisions, or resolve one
    Inbox(InboxArgs),
    /// Propose reclaim operations
    Plan(PlanArgs),
    /// Execute a plan after checking every invariant again
    Act(ActArgs),
    /// Show where a decision came from
    Explain(ExplainArgs),
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Unpadded base64url of a cardinal.run-spec.v1 document
    #[arg(long = "spec-b64", value_name = "SPEC")]
    pub spec_b64: Option<String>,
    /// The stage to run, after `--`
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct HookArgs {
    #[command(subcommand)]
    pub agent: HookAgent,
}

#[derive(Debug, Subcommand)]
pub enum HookAgent {
    /// Claude Code hooks
    Claude {
        #[command(subcommand)]
        event: ClaudeEvent,
    },
}

#[derive(Clone, Copy, Debug, Subcommand)]
pub enum ClaudeEvent {
    /// Classify a Bash call and rewrite its heavy stages
    PreToolUse,
    /// Record that a subagent stopped
    SubagentStop,
    /// Record that a session ended and snapshot its processes
    SessionEnd,
}

#[derive(Debug, Args)]
pub struct InventoryArgs {
    /// A main worktree to observe in addition to the configured ones
    #[arg(long = "repo", value_name = "PATH")]
    pub repos: Vec<PathBuf>,
    /// Walk worktrees to report allocated bytes
    #[arg(long)]
    pub sizes: bool,
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct InboxArgs {
    #[command(subcommand)]
    pub command: Option<InboxCommand>,
    /// Which items to list
    #[arg(long, value_enum, value_name = "STATUS")]
    pub status: Option<StatusArg>,
    /// List without running the refresh first
    #[arg(long)]
    pub no_refresh: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StatusArg {
    Open,
    Escalated,
    Resolved,
    Dismissed,
    All,
}

#[derive(Debug, Subcommand)]
pub enum InboxCommand {
    /// Resolve, escalate, or dismiss an item
    Resolve(ResolveArgs),
}

#[derive(Debug, Args)]
pub struct ResolveArgs {
    /// The item key
    pub key: String,
    /// The outcome
    #[arg(long, value_enum)]
    pub outcome: OutcomeArg,
    /// A note for the owner or the record
    #[arg(long, value_name = "TEXT")]
    pub note: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutcomeArg {
    Resolved,
    Escalated,
    Dismissed,
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct PlanArgs {
    #[command(subcommand)]
    pub command: Option<PlanCommand>,
    /// A main worktree to plan for in addition to the configured ones
    #[arg(long = "repo", value_name = "PATH")]
    pub repos: Vec<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum PlanCommand {
    /// Plan the reclamation of a linked worktree
    ReclaimWorktree {
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
    /// Plan the reclamation of a regenerable ignored entry
    ReclaimDir {
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
}

#[derive(Debug, Args)]
pub struct ActArgs {
    /// The plan ID, or a unique prefix of at least 4 characters
    #[arg(value_name = "PLAN")]
    pub plan: String,
    /// Run every check without claiming or deleting
    #[arg(long)]
    pub dry_run: bool,
    /// The owner approved a plan whose conditions fail
    #[arg(long)]
    pub owner_approved: bool,
    /// Who calls: arbiter, owner, or unit:NAME
    #[arg(long, value_name = "CALLER", default_value = "arbiter")]
    pub caller: Caller,
}

#[derive(Debug, Args)]
pub struct ExplainArgs {
    #[command(subcommand)]
    pub subject: ExplainSubject,
}

#[derive(Debug, Subcommand)]
pub enum ExplainSubject {
    /// Explain a job
    Job {
        #[arg(value_name = "ID")]
        id: String,
    },
    /// Explain a plan
    Plan {
        #[arg(value_name = "ID")]
        id: String,
    },
    /// Explain an inbox item
    Inbox {
        #[arg(value_name = "KEY")]
        key: String,
    },
    /// Trace the classification and rewrite of a command without running it
    Command {
        /// The directory the command would run in
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
        /// The command string, after `--`
        #[arg(last = true, required = true, value_name = "STRING")]
        command: String,
    },
}

/// Parses the process arguments and runs the command.
pub fn main() -> ExitCode {
    main_with(&std::env::args_os().collect::<Vec<_>>())
}

/// Parses `args` (with the program name first) and runs the command.
pub fn main_with(args: &[OsString]) -> ExitCode {
    let is_hook = args.get(1).is_some_and(|arg| arg == "hook");
    match Cli::try_parse_from(args) {
        Ok(cli) => dispatch(cli.command),
        // A hook must never fail the agent's tool call, not even on a usage error.
        Err(err) if is_hook && err.use_stderr() => ExitCode::SUCCESS,
        Err(err) => err.exit(),
    }
}

fn dispatch(command: Command) -> ExitCode {
    match command {
        // The runner prints nothing of its own: its stdout and stderr are the stage's.
        Command::Run(args) => run::run(args.spec_b64.as_deref(), &args.command),
        Command::Hook(args) => hook(&args),
        Command::Inventory(args) => {
            // Runs git and gh, so it sets their cleanup up while this is the only thread.
            child::prepare();
            respond(context().and_then(|ctx| {
                let options = InventoryOptions {
                    repos: args.repos,
                    sizes: args.sizes,
                };
                observe::inventory(&ctx.config, &ctx.state, &options)
            }))
        }
        Command::Inbox(args) => match args.command {
            None => respond(context().and_then(|ctx| {
                let status = args.status.map(|status| match status {
                    StatusArg::Open => StatusFilter::Only(InboxStatus::Open),
                    StatusArg::Escalated => StatusFilter::Only(InboxStatus::Escalated),
                    StatusArg::Resolved => StatusFilter::Only(InboxStatus::Resolved),
                    StatusArg::Dismissed => StatusFilter::Only(InboxStatus::Dismissed),
                    StatusArg::All => StatusFilter::All,
                });
                inbox::list(&ctx.config, &ctx.state, status, !args.no_refresh)
            })),
            Some(InboxCommand::Resolve(resolve)) => respond(context().and_then(|ctx| {
                let outcome = match resolve.outcome {
                    OutcomeArg::Resolved => ResolutionOutcome::Resolved,
                    OutcomeArg::Escalated => ResolutionOutcome::Escalated,
                    OutcomeArg::Dismissed => ResolutionOutcome::Dismissed,
                };
                inbox::resolve(
                    &ctx.config,
                    &ctx.state,
                    &resolve.key,
                    outcome,
                    resolve.note.as_deref(),
                )
            })),
        },
        Command::Plan(args) => match args.command {
            None => respond(
                context().and_then(|ctx| plan::create_all(&ctx.config, &ctx.state, &args.repos)),
            ),
            Some(PlanCommand::ReclaimWorktree { path }) => respond(context().and_then(|ctx| {
                plan::create(&ctx.config, &ctx.state, PlanKind::ReclaimWorktree, &path)
            })),
            Some(PlanCommand::ReclaimDir { path }) => respond(context().and_then(|ctx| {
                plan::create(&ctx.config, &ctx.state, PlanKind::ReclaimDir, &path)
            })),
        },
        Command::Act(args) => {
            let options = ActOptions {
                dry_run: args.dry_run,
                owner_approved: args.owner_approved,
                caller: args.caller,
            };
            match context()
                .and_then(|ctx| act::execute(&ctx.config, &ctx.state, &args.plan, &options))
            {
                Ok(result) => {
                    let refused = result.status.is_refusal();
                    print_json(&result);
                    if refused {
                        ExitCode::from(3)
                    } else {
                        ExitCode::SUCCESS
                    }
                }
                Err(err) => fail(&err),
            }
        }
        Command::Explain(args) => {
            let subject = match args.subject {
                ExplainSubject::Job { id } => Subject::Job(id),
                ExplainSubject::Plan { id } => Subject::Plan(id),
                ExplainSubject::Inbox { key } => Subject::Inbox(key),
                ExplainSubject::Command { cwd, command } => Subject::Command { cwd, command },
            };
            respond(context().and_then(|ctx| explain::explain(&ctx.config, &ctx.state, &subject)))
        }
    }
}

/// Runs a hook handler. Always exits 0. The handler's output is buffered and written only
/// when it succeeds, so an error or a panic, `not_implemented` included, leaves stdout empty
/// and the agent's tool call proceeds unchanged.
fn hook(args: &HookArgs) -> ExitCode {
    let HookAgent::Claude { event } = args.agent;
    let mut output = Vec::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut stdin = std::io::stdin().lock();
        match event {
            ClaudeEvent::PreToolUse => claude::pre_tool_use(&mut stdin, &mut output),
            ClaudeEvent::SubagentStop => claude::subagent_stop(&mut stdin),
            ClaudeEvent::SessionEnd => claude::session_end(&mut stdin),
        }
    }));
    if let Ok(Ok(())) = result {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(&output);
        let _ = stdout.flush();
    }
    ExitCode::SUCCESS
}

/// The loaded configuration and opened state directory.
struct Context {
    config: Config,
    state: StateDir,
}

fn context() -> Result<Context, Error> {
    let config = Config::load(&config::config_dir()?)?;
    let state_dir = config::state_dir()
        .map_err(|err| Error::new(ErrorCode::StateUnavailable, err.to_string()))?;
    let state = StateDir::open(&state_dir)?;
    Ok(Context { config, state })
}

fn respond<T: Serialize>(result: Result<T, Error>) -> ExitCode {
    match result {
        Ok(doc) => {
            print_json(&doc);
            ExitCode::SUCCESS
        }
        Err(err) => fail(&err),
    }
}

/// Prints the error to stderr for people and as `cardinal.error.v1` to stdout; exit 1.
fn fail(err: &Error) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "cardinal: {}", err.message);
    print_json(&err.to_doc());
    ExitCode::FAILURE
}

/// One JSON document on one line. A closed stdout is ignored.
fn print_json<T: Serialize>(doc: &T) {
    let mut out = std::io::stdout().lock();
    if serde_json::to_writer(&mut out, doc).is_ok() {
        let _ = writeln!(out);
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{Cli, Command, ExplainSubject, InboxCommand, PlanCommand, StatusArg};
    use crate::contract::Caller;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("cardinal").chain(args.iter().copied()))
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// Every command and flag of `docs/architecture.md` section 11.2, as paths of subcommand
    /// names followed by the long flags each takes.
    #[test]
    fn every_documented_command_and_flag_exists() {
        let expected: &[(&[&str], &[&str])] = &[
            (&["run"], &["spec-b64"]),
            (&["hook", "claude", "pre-tool-use"], &[]),
            (&["hook", "claude", "subagent-stop"], &[]),
            (&["hook", "claude", "session-end"], &[]),
            (&["inventory"], &["repo", "sizes"]),
            (&["inbox"], &["status", "no-refresh"]),
            (&["inbox", "resolve"], &["outcome", "note"]),
            (&["plan"], &["repo"]),
            (&["plan", "reclaim-worktree"], &[]),
            (&["plan", "reclaim-dir"], &[]),
            (&["act"], &["dry-run", "owner-approved", "caller"]),
            (&["explain", "job"], &[]),
            (&["explain", "plan"], &[]),
            (&["explain", "inbox"], &[]),
            (&["explain", "command"], &["cwd"]),
        ];
        let root = Cli::command();
        assert!(root.get_version().is_some());
        for (path, flags) in expected {
            let mut command = &root;
            for name in *path {
                command = command
                    .find_subcommand(name)
                    .unwrap_or_else(|| panic!("missing command {path:?}"));
            }
            for flag in *flags {
                assert!(
                    command
                        .get_arguments()
                        .any(|arg| arg.get_long() == Some(flag)),
                    "missing --{flag} on {path:?}"
                );
            }
        }
        let top: Vec<&str> = root
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect();
        assert_eq!(
            top,
            [
                "run",
                "hook",
                "inventory",
                "inbox",
                "plan",
                "act",
                "explain"
            ]
        );
    }

    #[test]
    fn run_takes_the_stage_after_the_separator_verbatim() {
        let cli = parse(&[
            "run",
            "--spec-b64",
            "e30",
            "--",
            "pnpm",
            "test",
            "--watch",
            "--",
            "-x",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.spec_b64.as_deref(), Some("e30"));
        assert_eq!(args.command, ["pnpm", "test", "--watch", "--", "-x"]);
        assert!(
            parse(&["run", "pnpm", "test"]).is_err(),
            "the stage needs `--`"
        );
        assert!(parse(&["run", "--"]).is_err(), "the stage is required");
    }

    #[test]
    fn subcommand_shapes() {
        let cli = parse(&["act", "abcd", "--caller", "unit:disk", "--dry-run"]).unwrap();
        let Command::Act(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.caller, Caller::Unit("disk".into()));
        assert!(args.dry_run && !args.owner_approved);
        let cli = parse(&["act", "abcd"]).unwrap();
        let Command::Act(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.caller, Caller::Arbiter);
        assert!(parse(&["act", "abcd", "--caller", "unit:"]).is_err());

        let cli = parse(&[
            "explain",
            "command",
            "--cwd",
            "/w",
            "--",
            "cd x && pnpm test",
        ])
        .unwrap();
        let Command::Explain(args) = cli.command else {
            panic!()
        };
        assert!(
            matches!(args.subject, ExplainSubject::Command { ref command, .. } if command == "cd x && pnpm test")
        );
        assert!(
            parse(&["explain", "command", "--", "a", "b"]).is_err(),
            "one string"
        );

        let cli = parse(&["inbox", "--status", "all", "--no-refresh"]).unwrap();
        let Command::Inbox(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.status, Some(StatusArg::All));
        assert!(args.no_refresh && args.command.is_none());
        let cli = parse(&[
            "inbox",
            "resolve",
            "recovery_needed:abc",
            "--outcome",
            "escalated",
            "--note",
            "why",
        ])
        .unwrap();
        let Command::Inbox(args) = cli.command else {
            panic!()
        };
        assert!(
            matches!(args.command, Some(InboxCommand::Resolve(ref r)) if r.key == "recovery_needed:abc")
        );
        assert!(
            parse(&["inbox", "resolve", "k"]).is_err(),
            "--outcome is required"
        );
        assert!(
            parse(&[
                "inbox",
                "--status",
                "open",
                "resolve",
                "k",
                "--outcome",
                "resolved"
            ])
            .is_err()
        );

        let cli = parse(&["plan", "--repo", "/a", "--repo", "/b"]).unwrap();
        let Command::Plan(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.repos.len(), 2);
        let cli = parse(&["plan", "reclaim-dir", "/w/node_modules"]).unwrap();
        let Command::Plan(args) = cli.command else {
            panic!()
        };
        assert!(matches!(args.command, Some(PlanCommand::ReclaimDir { .. })));
    }

    #[test]
    fn reserved_phase_2_commands_are_not_registered() {
        for name in ["wait", "cancel", "logs", "exec"] {
            assert!(parse(&[name]).is_err(), "{name}");
        }
    }
}
