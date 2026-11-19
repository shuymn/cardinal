//! The command line: clap definitions for every command and flag of
//! `docs/architecture.md` section 11.2 and thin handlers that print JSON.
//!
//! Exit codes (section 11.1): 0 success, 1 error with `cardinal.error.v1` on stdout, 2 usage
//! error (`--owner-approved` without `--caller owner` included), 3 `act` reports a refusal.
//! `run` exits with the stage's status (section 5.6), and `hook` always exits 0, also on a
//! usage error or a panic, because a non-zero hook exit can block the agent's tool call.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::act::{self, ActOptions};
use crate::adapter::claude;
use crate::config::{self, Config};
use crate::contract::{
    AnswerOutcome, Caller, Error, ErrorCode, Inbox, InboxBrief, InboxBriefSchema, InboxItem,
    InboxSchema, InboxStatus, PlanKind, Trigger,
};
use crate::explain::{self, Subject};
use crate::inbox::{self, StatusFilter};
use crate::observe::{self, InventoryOptions};
use crate::os::child;
use crate::plan;
use crate::reconcile;
use crate::run;
use crate::snapshot;
use crate::state::StateDir;

/// The environment variable that names the arbiter pass a session runs for (section 10.5).
const ARBITER_PASS_VAR: &str = "CARDINAL_ARBITER_PASS";

/// Local compute-resource arbitration and housekeeping for parallel coding agents.
#[derive(Debug, Parser)]
// The package version, then the source commit when the build had one (`build.rs`).
#[command(name = "cardinal", version = env!("CARDINAL_VERSION"))]
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
    /// Evaluate a plan again and report what it would reclaim
    Act(ActArgs),
    /// Run one pass: inbox refresh, disk states, observation, reports, and the arbiter's start
    Reconcile(ReconcileArgs),
    /// Run the arbiter
    Arbiter(ArbiterArgs),
    /// Write the read-only web page and print its path
    Ui,
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
    /// Observe only the worktree at PATH and print it as cardinal.worktree.v1
    #[arg(long, value_name = "PATH", conflicts_with_all = ["repos", "sizes"])]
    pub worktree: Option<PathBuf>,
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
    /// List the items assigned to the arbiter pass `CARDINAL_ARBITER_PASS` names
    #[arg(long)]
    pub assigned: bool,
    /// Print each item in a bounded size, as cardinal.inbox-brief.v1
    #[arg(long)]
    pub brief: bool,
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
    /// Refuse an item not assigned to the running arbiter pass `CARDINAL_ARBITER_PASS` names
    #[arg(long)]
    pub assigned: bool,
    /// Print the item in a bounded size, as cardinal.inbox-brief.v1
    #[arg(long)]
    pub brief: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutcomeArg {
    Resolved,
    Escalated,
    Dismissed,
    /// Leave the item open, answered by the arbiter pass (only with --assigned)
    Deferred,
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
    /// Report what a granted caller would execute, and execute nothing
    #[arg(long)]
    pub dry_run: bool,
    /// The owner approved a plan whose conditions fail (only with --caller owner)
    #[arg(long)]
    pub owner_approved: bool,
    /// The caller judged HEAD squash-merged; counts when every changed file is in the base
    #[arg(long)]
    pub squash_merged: bool,
    /// Who calls: arbiter, owner, or unit:NAME
    #[arg(long, value_name = "CALLER", default_value = "arbiter")]
    pub caller: Caller,
}

#[derive(Debug, Args)]
pub struct ReconcileArgs {
    /// What started the pass
    #[arg(long, value_enum, value_name = "TRIGGER", default_value = "manual")]
    pub trigger: TriggerArg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum TriggerArg {
    Timer,
    JobEnd,
    Manual,
}

#[derive(Debug, Args)]
pub struct ArbiterArgs {
    #[command(subcommand)]
    pub command: ArbiterCommand,
}

#[derive(Clone, Copy, Debug, Subcommand)]
pub enum ArbiterCommand {
    /// Run one arbiter pass for the due inbox items, or report the running one
    Run,
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
    match Cli::try_parse_from(args).and_then(validate) {
        Ok(cli) => dispatch(cli.command),
        // A hook must never fail the agent's tool call, not even on a usage error.
        Err(err) if is_hook && err.use_stderr() => ExitCode::SUCCESS,
        Err(err) => err.exit(),
    }
}

/// The rules clap's definitions do not express, as usage errors: `--owner-approved` only with
/// `--caller owner` (section 9.3), and `--outcome deferred` only with `--assigned`, an answer
/// of an arbiter pass (section 10.5).
fn validate(cli: Cli) -> Result<Cli, clap::Error> {
    if let Command::Inbox(InboxArgs {
        command: Some(InboxCommand::Resolve(args)),
        ..
    }) = &cli.command
        && args.outcome == OutcomeArg::Deferred
        && !args.assigned
    {
        let mut command = Cli::command();
        let resolve = command
            .find_subcommand_mut("inbox")
            .and_then(|inbox| inbox.find_subcommand_mut("resolve"))
            .expect("inbox resolve is defined");
        return Err(resolve.error(
            ErrorKind::ArgumentConflict,
            "--outcome deferred is accepted only with --assigned: only an arbiter pass defers",
        ));
    }
    if let Command::Act(args) = &cli.command
        && args.owner_approved
        && args.caller != Caller::Owner
    {
        let mut command = Cli::command();
        let act = command.find_subcommand_mut("act").expect("act is defined");
        return Err(act.error(
            ErrorKind::ArgumentConflict,
            format!(
                "--owner-approved is accepted only with --caller owner, not --caller {}",
                args.caller
            ),
        ));
    }
    Ok(cli)
}

fn dispatch(command: Command) -> ExitCode {
    match command {
        // The runner prints nothing of its own: its stdout and stderr are the stage's.
        Command::Run(args) => run::run(args.spec_b64.as_deref(), &args.command),
        Command::Hook(args) => hook(&args),
        Command::Inventory(args) => {
            // Runs git and gh, so it sets their cleanup up while this is the only thread.
            child::prepare();
            match args.worktree {
                Some(path) => respond(
                    context()
                        .and_then(|ctx| observe::worktree_view(&ctx.config, &ctx.state, &path)),
                ),
                None => respond(context().and_then(|ctx| {
                    let options = InventoryOptions {
                        repos: args.repos,
                        sizes: args.sizes,
                    };
                    observe::inventory(&ctx.config, &ctx.state, &options)
                })),
            }
        }
        Command::Inbox(args) => inbox_command(args),
        Command::Plan(args) => {
            // Observes through git and gh, so it sets their cleanup up while this is the only
            // thread.
            child::prepare();
            match args.command {
                None => respond(
                    context()
                        .and_then(|ctx| plan::create_all(&ctx.config, &ctx.state, &args.repos)),
                ),
                // A command a timer pass runs: its list facts are cut (section 10.5).
                Some(PlanCommand::ReclaimWorktree { path }) => respond(context().and_then(|ctx| {
                    let mut plan =
                        plan::create(&ctx.config, &ctx.state, PlanKind::ReclaimWorktree, &path)?;
                    plan::bound_facts(&mut plan.checks);
                    Ok(plan)
                })),
                Some(PlanCommand::ReclaimDir { path }) => respond(context().and_then(|ctx| {
                    plan::create(&ctx.config, &ctx.state, PlanKind::ReclaimDir, &path)
                })),
            }
        }
        Command::Act(args) => act_command(args),
        Command::Reconcile(args) => {
            // Observes through git and gh, so it sets their cleanup up while this is the only
            // thread.
            child::prepare();
            let trigger = match args.trigger {
                TriggerArg::Timer => Trigger::Timer,
                TriggerArg::JobEnd => Trigger::JobEnd,
                TriggerArg::Manual => Trigger::Manual,
            };
            respond(context().and_then(|ctx| reconcile::run(&ctx.config, &ctx.state, trigger)))
        }
        Command::Arbiter(args) => match args.command {
            ArbiterCommand::Run => {
                // Runs the session through `os::child`, as `reconcile` runs git.
                child::prepare();
                respond(context().and_then(|ctx| reconcile::run_arbiter(&ctx.config, &ctx.state)))
            }
        },
        Command::Ui => {
            match context().and_then(|ctx| snapshot::write_page(&ctx.config, &ctx.state)) {
                Ok(page) => {
                    let mut out = std::io::stdout().lock();
                    let _ = writeln!(out, "{}", page.display());
                    let _ = out.flush();
                    ExitCode::SUCCESS
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

/// `cardinal inbox` and `cardinal inbox resolve`, with `--brief` printing
/// `cardinal.inbox-brief.v1`.
fn inbox_command(args: InboxArgs) -> ExitCode {
    let brief = match &args.command {
        Some(InboxCommand::Resolve(resolve)) => resolve.brief,
        None => args.brief,
    };
    let print = |items: Result<Inbox, Error>| {
        if brief {
            respond(items.map(|inbox| InboxBrief {
                schema: InboxBriefSchema,
                items: inbox.items.iter().map(inbox::brief).collect(),
            }))
        } else {
            respond(items)
        }
    };
    let print_item = |item: Result<InboxItem, Error>| {
        if brief {
            print(item.map(|item| Inbox {
                schema: InboxSchema,
                items: vec![item],
            }))
        } else {
            respond(item)
        }
    };
    match args.command {
        None if args.assigned => print(context().and_then(|ctx| {
            inbox::list_assigned(&ctx.config, &ctx.state, arbiter_pass().as_deref())
        })),
        None => print(context().and_then(|ctx| {
            let status = args.status.map(|status| match status {
                StatusArg::Open => StatusFilter::Only(InboxStatus::Open),
                StatusArg::Escalated => StatusFilter::Only(InboxStatus::Escalated),
                StatusArg::Resolved => StatusFilter::Only(InboxStatus::Resolved),
                StatusArg::Dismissed => StatusFilter::Only(InboxStatus::Dismissed),
                StatusArg::All => StatusFilter::All,
            });
            inbox::list(&ctx.config, &ctx.state, status, !args.no_refresh)
        })),
        Some(InboxCommand::Resolve(resolve)) => print_item(context().and_then(|ctx| {
            let answer = match resolve.outcome {
                OutcomeArg::Resolved => AnswerOutcome::Resolved,
                OutcomeArg::Escalated => AnswerOutcome::Escalated,
                OutcomeArg::Dismissed => AnswerOutcome::Dismissed,
                OutcomeArg::Deferred => AnswerOutcome::Deferred,
            };
            match inbox::closing(answer) {
                Some(outcome) if !resolve.assigned => inbox::resolve(
                    &ctx.config,
                    &ctx.state,
                    &resolve.key,
                    outcome,
                    resolve.note.as_deref(),
                ),
                // `validate` accepts `deferred` only with `--assigned`.
                _ => inbox::resolve_assigned(
                    &ctx.config,
                    &ctx.state,
                    arbiter_pass().as_deref(),
                    &resolve.key,
                    answer,
                    resolve.note.as_deref(),
                ),
            }
        })),
    }
}

/// `cardinal act`: exit 3 when it reports a refusal.
fn act_command(args: ActArgs) -> ExitCode {
    // Re-observes the worktree through git and gh, as `plan` does.
    child::prepare();
    let options = ActOptions {
        dry_run: args.dry_run,
        owner_approved: args.owner_approved,
        squash_merged: args.squash_merged,
        caller: args.caller,
    };
    match context().and_then(|ctx| act::execute(&ctx.config, &ctx.state, &args.plan, &options)) {
        Ok(mut result) => {
            let refused = result.status.is_refusal();
            // A command a timer pass runs: its list facts are cut (section 10.5).
            plan::bound_facts(&mut result.checks);
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

/// The pass `CARDINAL_ARBITER_PASS` names, if it is set and not empty.
fn arbiter_pass() -> Option<String> {
    std::env::var(ARBITER_PASS_VAR)
        .ok()
        .filter(|pass| !pass.is_empty())
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

    use super::{
        ArbiterCommand, Cli, Command, ExplainSubject, InboxCommand, OutcomeArg, PlanCommand,
        StatusArg, TriggerArg, validate,
    };
    use crate::contract::Caller;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("cardinal").chain(args.iter().copied()))
            .and_then(validate)
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
            (&["inventory"], &["repo", "sizes", "worktree"]),
            (&["inbox"], &["status", "assigned", "no-refresh", "brief"]),
            (
                &["inbox", "resolve"],
                &["outcome", "note", "assigned", "brief"],
            ),
            (&["plan"], &["repo"]),
            (&["plan", "reclaim-worktree"], &[]),
            (&["plan", "reclaim-dir"], &[]),
            (
                &["act"],
                &["dry-run", "owner-approved", "squash-merged", "caller"],
            ),
            (&["reconcile"], &["trigger"]),
            (&["arbiter", "run"], &[]),
            (&["ui"], &[]),
            (&["explain", "job"], &[]),
            (&["explain", "plan"], &[]),
            (&["explain", "inbox"], &[]),
            (&["explain", "command"], &["cwd"]),
        ];
        let root = Cli::command();
        let version = root.get_version().expect("a version");
        let package = env!("CARGO_PKG_VERSION");
        assert!(
            version == package || version.starts_with(&format!("{package} (")),
            "{version}"
        );
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
                "reconcile",
                "arbiter",
                "ui",
                "explain"
            ]
        );
    }

    #[test]
    fn owner_approval_only_from_the_owner() {
        let cli = parse(&["act", "abcd", "--caller", "owner", "--owner-approved"]).unwrap();
        let Command::Act(args) = cli.command else {
            panic!()
        };
        assert!(args.owner_approved);
        for caller in [
            &[][..],
            &["--caller", "arbiter"],
            &["--caller", "unit:disk"],
        ] {
            let mut args = vec!["act", "abcd", "--owner-approved"];
            args.extend_from_slice(caller);
            let err = parse(&args).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{args:?}"
            );
            assert_eq!(err.exit_code(), 2, "{args:?}");
        }
        let cli = parse(&["act", "abcd", "--squash-merged"]).unwrap();
        let Command::Act(args) = cli.command else {
            panic!()
        };
        assert!(args.squash_merged && !args.owner_approved);
    }

    #[test]
    fn phase_1_command_shapes() {
        let cli = parse(&["reconcile"]).unwrap();
        let Command::Reconcile(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.trigger, TriggerArg::Manual);
        for (text, trigger) in [
            ("timer", TriggerArg::Timer),
            ("job-end", TriggerArg::JobEnd),
            ("manual", TriggerArg::Manual),
        ] {
            let cli = parse(&["reconcile", "--trigger", text]).unwrap();
            let Command::Reconcile(args) = cli.command else {
                panic!()
            };
            assert_eq!(args.trigger, trigger);
        }
        assert!(parse(&["reconcile", "--trigger", "job_end"]).is_err());
        assert!(matches!(
            parse(&["arbiter", "run"]).unwrap().command,
            Command::Arbiter(super::ArbiterArgs {
                command: ArbiterCommand::Run
            })
        ));
        assert!(parse(&["arbiter"]).is_err());
        assert!(matches!(parse(&["ui"]).unwrap().command, Command::Ui));

        let cli = parse(&["inbox", "--assigned"]).unwrap();
        let Command::Inbox(args) = cli.command else {
            panic!()
        };
        assert!(args.assigned && args.command.is_none());
        let cli = parse(&[
            "inbox",
            "resolve",
            "k",
            "--outcome",
            "resolved",
            "--assigned",
        ])
        .unwrap();
        let Command::Inbox(args) = cli.command else {
            panic!()
        };
        assert!(matches!(args.command, Some(InboxCommand::Resolve(ref r)) if r.assigned));
    }

    /// A pass's bounded commands: the brief inbox, one worktree's facts, and the deferral, which
    /// only an arbiter pass gives (a usage error without `--assigned`).
    #[test]
    fn bounded_pass_commands() {
        let cli = parse(&["inbox", "--assigned", "--brief"]).unwrap();
        let Command::Inbox(args) = cli.command else {
            panic!()
        };
        assert!(args.assigned && args.brief);
        let cli = parse(&["inventory", "--worktree", "/w"]).unwrap();
        let Command::Inventory(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.worktree.as_deref(), Some(std::path::Path::new("/w")));
        for other in [&["--repo", "/r"][..], &["--sizes"]] {
            let mut args = vec!["inventory", "--worktree", "/w"];
            args.extend_from_slice(other);
            assert!(parse(&args).is_err(), "{args:?}");
        }
        let deferred = [
            "inbox",
            "resolve",
            "k",
            "--outcome",
            "deferred",
            "--note",
            "n",
        ];
        let err = parse(&deferred).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        assert_eq!(err.exit_code(), 2);
        let mut assigned = deferred.to_vec();
        assigned.extend(["--assigned", "--brief"]);
        let cli = parse(&assigned).unwrap();
        let Command::Inbox(args) = cli.command else {
            panic!()
        };
        assert!(matches!(
            args.command,
            Some(InboxCommand::Resolve(ref r)) if r.assigned && r.brief && r.outcome == OutcomeArg::Deferred
        ));
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
