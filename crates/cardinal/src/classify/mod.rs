//! Classification: the shell tokenizer, stage finder, normalizer, rule matcher, stage
//! rewriting, and the `Classifier` seam (`docs/architecture.md` sections 6.4 and 7).
//!
//! [`find_stages`] splits a command into stages or refuses it; [`select`] picks the profiles
//! of the command's repository; [`decide`] classifies each stage and applies the rules of
//! section 7.2 that keep a stage unwrapped; [`rewrite`] wraps the stages that remain
//! (section 6.4); [`trace`] reports, from the same outcomes, how each stage was classified.
//! The hook and `explain command` both go through these, so they cannot disagree. Not
//! understanding a command only ever leads to a missed rewrite, never a changed one.

mod normalize;
mod rewrite;
mod tokenize;
mod trace;
mod words;

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;
use std::time::Instant;

use crate::config::{Config, Pattern, Profile, Rule, Selection, normalize_remote};
use crate::contract::{
    ClassifyDecision, ClassifyRepo, ClassifyRequest, ClassifyRequestSchema, ClassifyResponse,
    ClassifyResponseSchema, ClassifyTool, Error, ErrorCode, Resources,
};

pub use rewrite::{CallIds, Rewritten, rewrite};
pub use tokenize::Refusal;
pub use trace::{RuleMatch, StageTrace, trace};

use normalize::{Arg, base_name};

/// One simple command of a command string, with its byte span in that string.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent syntax facts the stage rules read"
)]
pub struct Stage {
    /// Position among the command's stages, from 0.
    pub index: u32,
    /// The stage's byte span, the replacement target of a rewrite: from its first word or
    /// redirection to its last, without surrounding blanks or a trailing comment.
    pub span: Range<usize>,
    /// The stage's words as written (unexpanded), redirections excluded.
    pub words: Vec<String>,
    /// Each word after quote removal; `None` for a word that contains an expansion.
    pub values: Vec<Option<String>>,
    /// The number of leading assignment words.
    pub assignments: usize,
    /// The index in `words` of the command word; `None` for a stage of only assignments
    /// and redirections.
    pub command_word: Option<usize>,
    /// The byte offset of the command word in the command string, where a rewrite inserts
    /// the runner.
    pub command_start: Option<usize>,
    /// Stages with the same number are in the same pipeline.
    pub pipeline: u32,
    /// A `|` feeds the stage's stdin.
    pub piped_in: bool,
    /// The stage's stdout feeds a `|`.
    pub piped_out: bool,
    /// The file descriptors the stage's redirections open, sorted.
    pub redirected: Vec<u32>,
    /// The stage's and-or list is followed by `&`.
    pub background: bool,
    /// The stage's pipeline is followed by `&&` or `||`: its and-or list goes on after it.
    pub continued: bool,
    /// The stage expands a parameter whose value the rewrite changes or no two runs share
    /// (`$?`, `$_`, `PIPESTATUS`, `$$`, `RANDOM`, …).
    pub shell_state: bool,
    /// The span contains a newline (inside quotes).
    pub multiline: bool,
}

impl Stage {
    fn args(&self) -> Vec<Arg<'_>> {
        self.words
            .iter()
            .zip(&self.values)
            .map(|(raw, value)| Arg {
                raw,
                value: value.as_deref(),
            })
            .collect()
    }
}

/// A classifier: the built-in one, and from phase 1 external units speaking the same types
/// as JSON. Called once per stage.
pub trait Classifier {
    fn classify(&self, req: &ClassifyRequest, deadline: Instant) -> ClassifyResponse;
}

/// The built-in classifier: the rules of the selected profiles, first match wins.
#[derive(Clone, Copy, Debug)]
pub struct BuiltIn<'s, 'a> {
    pub selection: &'s Selection<'a>,
}

impl Classifier for BuiltIn<'_, '_> {
    fn classify(&self, req: &ClassifyRequest, deadline: Instant) -> ClassifyResponse {
        if Instant::now() >= deadline {
            return pass("the classification deadline passed");
        }
        let Some((_, rule, pattern)) = first_match(self.selection, &req.argv) else {
            return pass("no rule matched");
        };
        ClassifyResponse {
            schema: ClassifyResponseSchema,
            decision: ClassifyDecision::Wrap,
            class: Some(rule.class.clone()),
            rule: Some(rule.qualified_id.clone()),
            resources: rule.resources.clone(),
            reason: format!("rule {} matched `{}`", rule.qualified_id, pattern.text),
        }
    }
}

fn pass(reason: &str) -> ClassifyResponse {
    ClassifyResponse {
        schema: ClassifyResponseSchema,
        decision: ClassifyDecision::Pass,
        class: None,
        rule: None,
        resources: None,
        reason: reason.to_owned(),
    }
}

/// The first rule of `selection` in precedence order that matches `argv`, with its profile
/// and the pattern that matched: what the built-in classifier wraps by.
fn first_match<'a>(
    selection: &Selection<'a>,
    argv: &[String],
) -> Option<(&'a Profile, &'a Rule, &'a Pattern)> {
    selection
        .rules()
        .find_map(|(profile, rule)| rule_match(rule, argv).map(|pattern| (profile, rule, pattern)))
}

/// The pattern of `rule` that matches `argv`: a prefix of it, with `*` matching any one word,
/// unless an `except` word occurs anywhere in `argv`.
fn rule_match<'r>(rule: &'r Rule, argv: &[String]) -> Option<&'r Pattern> {
    if argv.iter().any(|word| rule.except.contains(word)) {
        return None;
    }
    rule.patterns.iter().find(|pattern| {
        pattern.words.len() <= argv.len()
            && pattern
                .words
                .iter()
                .zip(argv)
                .all(|(p, a)| p == "*" || p == a)
    })
}

/// Splits `command` into stages, or refuses the whole command when a stage span cannot be
/// delimited with certainty (section 7.2).
pub fn find_stages(command: &str) -> Result<Vec<Stage>, Error> {
    tokenize::stages(command)
        .map_err(|refusal| Error::new(ErrorCode::InvalidArgument, refusal.to_string()))
}

/// [`find_stages`] with the typed reason for a refusal.
pub fn tokenize(command: &str) -> Result<Vec<Stage>, Refusal> {
    tokenize::stages(command)
}

/// Classifies one stage with the built-in classifier against the rules of `selection`.
pub fn classify(
    req: &ClassifyRequest,
    selection: &Selection<'_>,
    deadline: Instant,
) -> Result<ClassifyResponse, Error> {
    Ok(BuiltIn { selection }.classify(req, deadline))
}

/// The candidate normalized argvs of a stage (section 7.3), in the order classification
/// tries them: one, or one per simple command of `sh -c STRING`. Empty when the stage
/// cannot be classified.
#[must_use]
pub fn normalize(stage: &Stage, wrappers: &[Vec<String>]) -> Vec<Vec<String>> {
    normalize::candidates(&stage.args(), wrappers)
}

/// Why a stage stays unwrapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// Only assignments and redirections.
    NoCommand,
    /// The command word contains an expansion.
    NotLiteral,
    /// A builtin, keyword, or precommand modifier.
    ShellCommand,
    /// `sleep`, on which the agent's timeout handling depends.
    Sleep,
    /// In `[classify] skip`.
    Skipped,
    /// No classifier wrapped it.
    Unclassified,
    /// Expands a parameter whose value the rewrite changes or no two runs share.
    ShellState,
    /// Has assignment prefixes.
    Assignments,
    /// Runs asynchronously.
    Background,
    /// Its span contains a newline.
    Multiline,
    /// Its pipeline contains another classified stage.
    SharedPipeline,
    /// It redirects a file descriptor its pipeline connects; zsh (`MULTIOS`) would then
    /// send the output, or read the input, both ways for the stage as written, but not for
    /// the wrapped stage, whose pipe belongs to the surrounding `if`.
    PipedRedirect,
    /// It runs in the calling shell (alone or last in its pipeline), and a status it does not
    /// produce can decide `errexit`: a later command of its and-or list, or with `pipefail`
    /// an earlier stage of its pipeline. Once an `if` has run in the calling shell, zsh
    /// ignores `errexit` for both.
    Errexit,
}

impl Skip {
    /// The reason as a `snake_case` string, for records and explanations.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoCommand => "no_command",
            Self::NotLiteral => "not_literal",
            Self::ShellCommand => "shell_command",
            Self::Sleep => "sleep",
            Self::Skipped => "skipped",
            Self::Unclassified => "unclassified",
            Self::ShellState => "shell_state",
            Self::Assignments => "assignments",
            Self::Background => "background",
            Self::Multiline => "multiline",
            Self::SharedPipeline => "shared_pipeline",
            Self::PipedRedirect => "piped_redirect",
            Self::Errexit => "errexit",
        }
    }
}

/// What the stages of a command are classified in.
#[derive(Clone, Copy, Debug)]
pub struct Context<'a> {
    pub command: &'a str,
    pub cwd: &'a str,
    pub repo: Option<&'a ClassifyRepo>,
    pub tool: &'a ClassifyTool,
}

/// The decision for one stage.
#[derive(Clone, Debug, PartialEq)]
pub struct StageOutcome {
    pub stage: Stage,
    /// The normalized argv the stage was classified by.
    pub argv: Option<Vec<String>>,
    /// The classifier's answer; `None` when the stage was not offered to the classifier.
    pub response: Option<ClassifyResponse>,
    /// Why the stage stays unwrapped; `None` when it is wrapped.
    pub skip: Option<Skip>,
}

impl StageOutcome {
    #[must_use]
    pub fn wrap(&self) -> bool {
        self.skip.is_none()
    }

    fn classified(&self) -> bool {
        self.response
            .as_ref()
            .is_some_and(|r| r.decision == ClassifyDecision::Wrap)
    }

    /// The resources of a wrapped stage: its rule's, else `default`.
    #[must_use]
    pub fn resources(&self, default: &Resources) -> Resources {
        self.response
            .as_ref()
            .and_then(|r| r.resources.clone())
            .unwrap_or_else(|| default.clone())
    }
}

/// Classifies each stage and decides which are wrapped (sections 7.2 to 7.4): stages that
/// run in the calling shell are never offered to the classifier, and classified stages that
/// the rewrite could change stay unwrapped.
pub fn decide(
    stages: Vec<Stage>,
    ctx: &Context<'_>,
    selection: &Selection<'_>,
    classifier: &dyn Classifier,
    deadline: Instant,
) -> Vec<StageOutcome> {
    let skip = selection.skip();
    let wrappers = selection.wrappers();
    let mut outcomes: Vec<StageOutcome> = stages
        .into_iter()
        .map(|stage| {
            let unwrapped = |stage, reason| StageOutcome {
                stage,
                argv: None,
                response: None,
                skip: Some(reason),
            };
            let Some(index) = stage.command_word else {
                return unwrapped(stage, Skip::NoCommand);
            };
            let Some(word) = stage.values[index].clone() else {
                return unwrapped(stage, Skip::NotLiteral);
            };
            if words::is_shell_command(&word) {
                return unwrapped(stage, Skip::ShellCommand);
            }
            if base_name(&word) == "sleep" {
                return unwrapped(stage, Skip::Sleep);
            }
            let candidates = normalize(&stage, wrappers);
            if skip.contains(word.as_str())
                || candidates
                    .iter()
                    .any(|argv| skip.contains(argv[0].as_str()))
            {
                return unwrapped(stage, Skip::Skipped);
            }

            let stage_text = &ctx.command[stage.span.clone()];
            let mut first: Option<(Vec<String>, ClassifyResponse)> = None;
            for argv in candidates {
                let req = ClassifyRequest {
                    schema: ClassifyRequestSchema,
                    command: ctx.command.to_owned(),
                    stage: stage_text.to_owned(),
                    argv,
                    cwd: ctx.cwd.to_owned(),
                    repo: ctx.repo.cloned(),
                    tool: ctx.tool.clone(),
                };
                let response = classifier.classify(&req, deadline);
                let wrap = response.decision == ClassifyDecision::Wrap;
                if wrap || first.is_none() {
                    first = Some((req.argv, response));
                }
                if wrap {
                    break;
                }
            }
            let Some((argv, response)) = first else {
                return unwrapped(stage, Skip::Unclassified);
            };
            let skip = if response.decision != ClassifyDecision::Wrap {
                Some(Skip::Unclassified)
            } else if stage.shell_state {
                Some(Skip::ShellState)
            } else if stage.assignments > 0 {
                Some(Skip::Assignments)
            } else if stage.background {
                Some(Skip::Background)
            } else if stage.multiline {
                Some(Skip::Multiline)
            } else if (stage.piped_out && stage.redirected.contains(&1))
                || (stage.piped_in && stage.redirected.contains(&0))
            {
                Some(Skip::PipedRedirect)
            } else if !stage.piped_out && (stage.continued || stage.piped_in) {
                Some(Skip::Errexit)
            } else {
                None
            };
            StageOutcome {
                stage,
                argv: Some(argv),
                response: Some(response),
                skip,
            }
        })
        .collect();

    // A stage waiting for admission could block a running one through the pipe.
    let mut per_pipeline: BTreeMap<u32, usize> = BTreeMap::new();
    for outcome in outcomes.iter().filter(|o| o.classified()) {
        *per_pipeline.entry(outcome.stage.pipeline).or_default() += 1;
    }
    for outcome in &mut outcomes {
        if outcome.wrap() && per_pipeline.get(&outcome.stage.pipeline) > Some(&1) {
            outcome.skip = Some(Skip::SharedPipeline);
        }
    }
    outcomes
}

/// What a caller read of a repository's `origin` remote for profile matching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Url(String),
    /// No origin, git failed or could not start, or its output was unusable.
    Unknown,
    /// The caller's deadline passed before git answered.
    TimedOut,
}

impl Origin {
    /// What `git remote get-url origin` said: one non-empty line from a git that exited 0,
    /// else nothing usable.
    #[must_use]
    pub fn from_output(code: Option<i32>, stdout: &[u8]) -> Self {
        let url = String::from_utf8_lossy(stdout).trim().to_owned();
        if code == Some(0) && !url.is_empty() && !url.contains('\n') {
            Self::Url(url)
        } else {
            Self::Unknown
        }
    }
}

/// The profiles that apply to `stages` run in the repository whose canonical git common dir
/// is `common_dir` (section 3.3): profiles matched by path, then by the origin remote, then
/// the generic ones. `origin` reads the remote, and is called only inside a repository, when
/// some profile could match by remote but not by path, and when a rule could match a stage
/// at all. Returns the selection with the origin URL it read, or `None` when reading the
/// origin missed the caller's deadline. The hook and `explain command` both select through
/// this function, so they classify a command the same way.
pub fn select<'c>(
    config: &'c Config,
    stages: &[Stage],
    common_dir: Option<&Path>,
    origin: impl FnOnce() -> Origin,
) -> Option<(Selection<'c>, Option<String>)> {
    let path_matched =
        |profile: &Profile| common_dir.is_some_and(|dir| profile.matches_common_dir(dir));
    let remote_needed = common_dir.is_some()
        && config.profiles.iter().any(|profile| {
            profile
                .matcher
                .as_ref()
                .is_some_and(|m| !m.remotes.is_empty())
                && !path_matched(profile)
        })
        && rules_could_match(stages, &config.profiles);
    let remote = if remote_needed {
        match origin() {
            Origin::Url(url) => Some(url),
            Origin::Unknown => None,
            Origin::TimedOut => return None,
        }
    } else {
        None
    };
    let selection = config.select(|profile| {
        path_matched(profile)
            || remote
                .as_deref()
                .is_some_and(|url| profile.matches_remote(url))
    });
    Some((selection, remote))
}

/// The repository of a classification request: its common dir, its main worktree (the
/// parent of a `.git` common dir, else the common dir itself, a bare repository), and its
/// normalized origin.
#[must_use]
pub fn request_repo(common_dir: &Path, remote: Option<&str>) -> ClassifyRepo {
    let main_worktree = match (common_dir.file_name(), common_dir.parent()) {
        (Some(name), Some(parent)) if name == ".git" => parent,
        _ => common_dir,
    };
    ClassifyRepo {
        common_dir: common_dir.to_string_lossy().into_owned(),
        main_worktree: main_worktree.to_string_lossy().into_owned(),
        remote: remote.and_then(normalize_remote),
    }
}

/// Whether any rule of `profiles` could match a stage, under any profile's wrappers. The hook
/// runs git for remote matching only when this holds (section 3.3).
#[must_use]
pub fn rules_could_match(stages: &[Stage], profiles: &[Profile]) -> bool {
    let mut wrapper_sets: Vec<&[Vec<String>]> = vec![&[]];
    wrapper_sets.extend(profiles.iter().filter_map(|p| p.wrappers.as_deref()));
    stages.iter().any(|stage| {
        let Some(word) = stage.command_word.and_then(|i| stage.values[i].as_deref()) else {
            return false;
        };
        if words::is_shell_command(word) {
            return false;
        }
        wrapper_sets.iter().any(|wrappers| {
            normalize(stage, wrappers).iter().any(|argv| {
                profiles
                    .iter()
                    .flat_map(|p| &p.rules)
                    .any(|rule| rule_match(rule, argv).is_some())
            })
        })
    })
}

#[cfg(test)]
mod tests;
