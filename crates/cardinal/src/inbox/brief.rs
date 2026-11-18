//! The brief view of an item (`cardinal.inbox-brief.v1`, `docs/architecture.md` section
//! 11.3): what a pass the timer started reads and answers with, in a size that does not grow
//! with the host. An item's evidence keeps only the fields its kind's decision reads (the
//! skill's inbox table), and in them every list keeps its first [`LIST_ENTRIES`] entries,
//! counted in `omitted` by JSON pointer. Identities (keys, paths, IDs, set fingerprints,
//! PIDs) are never cut, so a value always names what it names; only free text (a command
//! line, a reason, a message, and the notes of the answer and the resolution) is cut, to a
//! preview of at most [`TEXT_CHARS`] characters ending in `…`, its full length in
//! `truncated` by JSON pointer. The full item stays in `cardinal inbox` and `cardinal explain
//! inbox KEY`.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::contract::{
    AnswerOutcome, BriefItem, InboxBrief, InboxBriefSchema, InboxItem, InboxKind, ItemAnswer,
    ItemArbiter, Resolution, ResolutionOutcome,
};

/// How many entries a list in a brief item keeps.
pub const LIST_ENTRIES: usize = 3;

/// How many characters a free-text string in a brief item has at most, the `…` that ends a
/// cut one included.
pub const TEXT_CHARS: usize = 120;

/// The most bytes `cardinal inbox --assigned --brief` prints for a pass's items, and so any
/// `inbox resolve --brief` of one of them: half of the 30,000 characters Claude Code shows of a
/// command's output by default (section 10.5).
pub const LISTING_BYTES: usize = 15_000;

/// The names of the free-text fields of evidence, wherever they are: every other string is an
/// identity or a state and is kept whole.
const FREE_TEXT: &[&str] = &["command", "detail", "message", "reason"];

/// The brief view of `item`.
#[must_use]
pub fn brief(item: &InboxItem) -> BriefItem {
    let mut evidence = match fields(item.kind) {
        Some(pointers) => project(&item.evidence, pointers),
        None => item.evidence.clone(),
    };
    if item.kind == InboxKind::UnclassifiableData
        && let Some(set) = unclassifiable_set(&item.key)
    {
        evidence.insert("set".into(), Value::String(set.to_owned()));
    }
    let mut cuts = Cuts::default();
    for (name, value) in &mut evidence {
        cuts.cut(value, &format!("/evidence/{}", escape(name)), false);
    }
    let mut arbiter = item.arbiter.clone();
    if let Some(answer) = arbiter.as_mut().and_then(|a| a.answer.as_mut()) {
        cuts.shorten(&mut answer.note, "/arbiter/answer/note");
    }
    let mut resolution = item.resolution.clone();
    if let Some(resolution) = &mut resolution {
        cuts.shorten(&mut resolution.note, "/resolution/note");
    }
    BriefItem {
        key: item.key.clone(),
        kind: item.kind,
        status: item.status,
        subject: item.subject.clone(),
        evidence,
        omitted: cuts.omitted,
        truncated: cuts.truncated,
        arbiter,
        resolution,
    }
}

/// The keys of the items a pass is assigned: the first of `due`, in its order, while they are
/// at most `max_items` and their brief listing, each item answered with the longest notes,
/// takes at most [`LISTING_BYTES`]; always the first, so every item is assigned in its turn.
#[must_use]
pub fn batch(due: &[InboxItem], max_items: usize) -> Vec<String> {
    let empty = InboxBrief {
        schema: InboxBriefSchema,
        items: Vec::new(),
    };
    // The listing, its newline included, and a comma before every item but the first.
    let mut bytes = json_len(&empty) + 1;
    let mut keys = Vec::new();
    for item in due.iter().take(max_items) {
        let size = answered_len(item) + usize::from(!keys.is_empty());
        if !keys.is_empty() && bytes.saturating_add(size) > LISTING_BYTES {
            break;
        }
        bytes = bytes.saturating_add(size);
        keys.push(item.key.clone());
    }
    keys
}

/// The bytes `item`'s brief view takes at most while a pass holds it: assigned, answered, and
/// closed, with notes whose previews are as long as UTF-8 makes them.
fn answered_len(item: &InboxItem) -> usize {
    // A four-byte character, the widest that JSON does not escape.
    let note = "\u{1d11e}".repeat(TEXT_CHARS + 1);
    let mut answered = item.clone();
    answered.arbiter = Some(ItemArbiter {
        pass: "0".repeat(16),
        assigned_ms: u64::MAX,
        answer: Some(ItemAnswer {
            outcome: AnswerOutcome::Escalated,
            note: note.clone(),
            at_ms: u64::MAX,
        }),
        outcome: None,
        ended_ms: None,
    });
    answered.resolution = Some(Resolution {
        outcome: ResolutionOutcome::Escalated,
        by: super::BY_CLI.to_owned(),
        note,
        at_ms: u64::MAX,
    });
    json_len(&brief(&answered))
}

/// The length of `value` as compact JSON; [`usize::MAX`] when it cannot be serialized.
fn json_len(value: &impl serde::Serialize) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

/// The evidence fields a decision on an item of `kind` reads, as JSON pointers; `None` keeps
/// them all (a kind whose evidence is small, or not yet raised).
fn fields(kind: InboxKind) -> Option<&'static [&'static str]> {
    Some(match kind {
        InboxKind::RecoveryNeeded => &[
            "/job_id",
            "/derived",
            "/verified",
            "/members",
            "/worktree/path",
            "/session/id",
        ],
        InboxKind::OutlivedProcesses => &[
            "/session_id",
            "/reason",
            "/ended_ms",
            "/agent",
            "/processes",
        ],
        InboxKind::Reclaimable => &["/plan_id", "/observed_ms"],
        InboxKind::MergeUndecided => &[
            "/plan_id",
            "/observed_ms",
            "/merge_evidence",
            "/content_in_base",
            "/content_diff",
            "/closed_pr",
        ],
        InboxKind::ClosedPrDirty => &["/observed_ms", "/closed_pr", "/facts/dirty"],
        InboxKind::ManagedMerged => &[
            "/observed_ms",
            "/managed_by",
            "/facts/merge_evidence",
            "/facts/merged_pr",
            "/facts/dirty",
            "/facts/in_use/value",
        ],
        InboxKind::UnclassifiableData => &["/observed_ms", "/entries", "/paths"],
        InboxKind::DiskLow | InboxKind::DiskCritical => &[
            "/volume",
            "/mount",
            "/disk/state",
            "/report/reading",
            "/report/shortfall_bytes",
            "/report/targets",
            "/report/blocked",
            "/report/incomplete",
        ],
        InboxKind::ArbiterFailing => &["/logs"],
        InboxKind::ReclaimStranded => return None,
    })
}

/// The `<set>` of an `unclassifiable_data` key, `unclassifiable:<worktree>:<set>@<since_ms>`.
fn unclassifiable_set(key: &str) -> Option<&str> {
    let (condition, _) = super::occurrence_of(key);
    let (_, set) = condition.rsplit_once(':')?;
    (!set.is_empty()).then_some(set)
}

/// The values at `pointers` in `evidence`, at the same places; a pointer with no value is
/// left out.
fn project(evidence: &Map<String, Value>, pointers: &[&str]) -> Map<String, Value> {
    let source = Value::Object(evidence.clone());
    let mut out = Map::new();
    for pointer in pointers {
        let Some(value) = source.pointer(pointer) else {
            continue;
        };
        let names: Vec<String> = pointer.split('/').skip(1).map(unescape).collect();
        let Some((last, parents)) = names.split_last() else {
            continue;
        };
        if let Some(at) = parents.iter().try_fold(&mut out, |at, name| {
            match at
                .entry(name.clone())
                .or_insert_with(|| Value::Object(Map::new()))
            {
                Value::Object(next) => Some(next),
                _ => None,
            }
        }) {
            at.insert(last.clone(), value.clone());
        }
    }
    out
}

/// What a brief item left out, by JSON pointer into the item: the entries of each cut list,
/// and the full length in characters of each cut text.
#[derive(Default)]
struct Cuts {
    omitted: BTreeMap<String, u64>,
    truncated: BTreeMap<String, u64>,
}

impl Cuts {
    /// Cuts every list below `value`, at pointer `at`, to [`LIST_ENTRIES`] entries, and every
    /// free-text string, which `text` says `value` is, to a preview.
    fn cut(&mut self, value: &mut Value, at: &str, text: bool) {
        match value {
            Value::Array(entries) => {
                if entries.len() > LIST_ENTRIES {
                    self.omitted
                        .insert(at.to_owned(), (entries.len() - LIST_ENTRIES) as u64);
                    entries.truncate(LIST_ENTRIES);
                }
                for (i, entry) in entries.iter_mut().enumerate() {
                    self.cut(entry, &format!("{at}/{i}"), text);
                }
            }
            Value::Object(fields) => {
                for (name, field) in fields {
                    let text = FREE_TEXT.contains(&name.as_str());
                    self.cut(field, &format!("{at}/{}", escape(name)), text);
                }
            }
            Value::String(value) if text => self.shorten(value, at),
            _ => {}
        }
    }

    /// Cuts `text`, at pointer `at`, to its first [`TEXT_CHARS`] characters, the last of them
    /// a `…`, recording its full length; a text that fits is left whole.
    fn shorten(&mut self, text: &mut String, at: &str) {
        let chars = text.chars().count();
        if chars > TEXT_CHARS
            && let Some((end, _)) = text.char_indices().nth(TEXT_CHARS - 1)
        {
            text.truncate(end);
            text.push('…');
            self.truncated.insert(at.to_owned(), chars as u64);
        }
    }
}

/// A name as a JSON pointer token (RFC 6901).
fn escape(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}

/// A JSON pointer token as the name it stands for (RFC 6901).
fn unescape(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{LIST_ENTRIES, LISTING_BYTES, TEXT_CHARS, batch, brief};
    use crate::contract::{
        AnswerOutcome, InboxItem, InboxItemSchema, InboxKind, InboxStatus, InboxSubject,
        ItemAnswer, ItemArbiter,
    };

    fn item(kind: InboxKind, key: &str, evidence: &serde_json::Value) -> InboxItem {
        InboxItem {
            schema: InboxItemSchema,
            key: key.into(),
            kind,
            status: InboxStatus::Open,
            subject: InboxSubject::default(),
            evidence: evidence.as_object().cloned().unwrap(),
            created_ms: 1,
            updated_ms: 1,
            seen: 1,
            ended_ms: None,
            arbiter: None,
            resolution: None,
        }
    }

    /// A reclaimable item keeps its plan ID and drops the plan's checks, which the pass reads
    /// from a fresh plan.
    #[test]
    fn evidence_keeps_what_the_decision_reads() {
        let reclaimable = item(
            InboxKind::Reclaimable,
            "reclaimable:/w@h@1",
            &json!({"plan_id": "p", "observed_ms": 1, "repo": "/r", "checks_ms": 1,
                   "checks": [{"name": "clean"}]}),
        );
        let view = brief(&reclaimable);
        assert_eq!(
            serde_json::Value::Object(view.evidence),
            json!({"plan_id": "p", "observed_ms": 1})
        );
        let managed = item(
            InboxKind::ManagedMerged,
            "managed_merged:/w@h@1",
            &json!({"observed_ms": 1, "repo": "/r", "managed_by": "t3code",
                   "facts": {"dirty": "no", "merge_evidence": "merged_pr",
                             "merged_pr": {"number": 7, "url": "u"},
                             "content_diff": ["a"], "in_use": {"value": false, "jobs": []}}}),
        );
        assert_eq!(
            serde_json::Value::Object(brief(&managed).evidence),
            json!({"observed_ms": 1, "managed_by": "t3code",
                   "facts": {"merge_evidence": "merged_pr",
                             "merged_pr": {"number": 7, "url": "u"}, "dirty": "no",
                             "in_use": {"value": false}}})
        );
    }

    /// Lists keep their first entries, counted by pointer; free text and notes are cut to a
    /// preview of at most [`TEXT_CHARS`] characters, marked with their full length.
    #[test]
    fn lists_and_free_text_are_cut() {
        let processes: Vec<_> = (0..40)
            .map(
                |pid| json!({"pid": pid, "start_s": 1, "name": "node", "command": "x".repeat(500)}),
            )
            .collect();
        let mut outlived = item(
            InboxKind::OutlivedProcesses,
            "outlived:s@1",
            &json!({"session_id": "s", "reason": "other", "ended_ms": 1,
                   "agent": {"pid": 1, "start_s": 1, "name": "claude"},
                   "processes": processes}),
        );
        outlived.arbiter = Some(ItemArbiter {
            pass: "p".into(),
            assigned_ms: 1,
            answer: Some(ItemAnswer {
                outcome: AnswerOutcome::Deferred,
                note: "n".repeat(1000),
                at_ms: 2,
            }),
            outcome: None,
            ended_ms: None,
        });
        let view = brief(&outlived);
        let kept = view.evidence["processes"].as_array().unwrap();
        assert_eq!(kept.len(), LIST_ENTRIES);
        assert_eq!(
            view.omitted["/evidence/processes"],
            40 - LIST_ENTRIES as u64
        );
        let command = kept[0]["command"].as_str().unwrap();
        assert_eq!(command.chars().count(), TEXT_CHARS);
        assert!(command.ends_with('…'));
        assert_eq!(view.truncated["/evidence/processes/0/command"], 500);
        let note = &view.arbiter.unwrap().answer.unwrap().note;
        assert_eq!(note.chars().count(), TEXT_CHARS);
        assert_eq!(view.truncated["/arbiter/answer/note"], 1000);
        assert_eq!(view.truncated.len(), LIST_ENTRIES + 1);
    }

    /// Identities are kept whole however long: a path, a key's set, a URL; a free-text string
    /// that fits is not marked.
    #[test]
    fn identities_are_never_cut() {
        let long = format!("/w/{}", "d".repeat(400));
        let disk = item(
            InboxKind::DiskLow,
            "disk_low:/v",
            &json!({"volume": long, "report": {"incomplete": [
                {"worktree": long, "reason": "r".repeat(TEXT_CHARS)},
            ]}}),
        );
        let view = brief(&disk);
        assert_eq!(view.evidence["volume"], long.as_str());
        let entry = &view.evidence["report"]["incomplete"][0];
        assert_eq!(entry["worktree"], long.as_str());
        assert_eq!(
            entry["reason"].as_str().unwrap().chars().count(),
            TEXT_CHARS
        );
        assert!(view.truncated.is_empty(), "{:?}", view.truncated);
        let paths: Vec<_> = (0..2)
            .map(|i| json!({"path": format!("{long}/{i}")}))
            .collect();
        let key = format!("unclassifiable:{long}:0123456789abcdef@17");
        let view = brief(&item(
            InboxKind::UnclassifiableData,
            &key,
            &json!({"paths": paths}),
        ));
        assert_eq!(view.key, key);
        assert_eq!(view.evidence["paths"][1]["path"], format!("{long}/1"));
        assert!(view.truncated.is_empty());
    }

    /// An unclassifiable item carries the `<set>` of its key, which a pass compares with
    /// `cardinal inventory --worktree`'s `unclassifiable_set`.
    #[test]
    fn unclassifiable_items_name_their_set() {
        let paths: Vec<_> = (0..50)
            .map(|i| json!({"path": format!("/w/data-{i}"), "allocated_bytes": 1}))
            .collect();
        let unclassifiable = item(
            InboxKind::UnclassifiableData,
            "unclassifiable:/w:with:colons:0123456789abcdef@17",
            &json!({"observed_ms": 1, "entries": 80, "paths": paths, "errors": []}),
        );
        let view = brief(&unclassifiable);
        assert_eq!(view.evidence["set"], "0123456789abcdef");
        assert_eq!(view.evidence["entries"], 80);
        assert_eq!(view.omitted["/evidence/paths"], 50 - LIST_ENTRIES as u64);
    }

    /// A batch keeps the due order and stops at `max_items` or before the item that would take
    /// the listing past [`LISTING_BYTES`], answers included; its first item is taken whatever
    /// its size, so none waits forever.
    #[test]
    fn a_batch_stops_at_the_budget() {
        let small: Vec<_> = (0..30)
            .map(|i| {
                item(
                    InboxKind::Reclaimable,
                    &format!("reclaimable:/w{i:02}@h@1"),
                    &json!({}),
                )
            })
            .collect();
        let keys = |items: &[InboxItem]| items.iter().map(|i| i.key.clone()).collect::<Vec<_>>();
        assert_eq!(batch(&small, 4), keys(&small[..4]));
        let taken = batch(&small, 30);
        assert!(taken.len() < 30 && taken.len() > 4, "{}", taken.len());
        assert_eq!(taken, keys(&small[..taken.len()]));
        // Each answered with the longest notes, the batch still lists within the budget.
        let answered: Vec<_> = small[..taken.len()]
            .iter()
            .map(|item| {
                let mut item = item.clone();
                let note = "\u{1d11e}".repeat(500);
                item.arbiter = Some(ItemArbiter {
                    pass: "0123456789abcdef".into(),
                    assigned_ms: 1_700_000_000_000,
                    answer: Some(ItemAnswer {
                        outcome: AnswerOutcome::Dismissed,
                        note: note.clone(),
                        at_ms: 1_700_000_000_001,
                    }),
                    outcome: None,
                    ended_ms: None,
                });
                item.resolution = Some(crate::contract::Resolution {
                    outcome: crate::contract::ResolutionOutcome::Dismissed,
                    by: "cli".into(),
                    note,
                    at_ms: 1_700_000_000_001,
                });
                brief(&item)
            })
            .collect();
        let listing = serde_json::to_string(&crate::contract::InboxBrief {
            schema: crate::contract::InboxBriefSchema,
            items: answered,
        })
        .unwrap();
        assert!(listing.len() < LISTING_BYTES, "{}", listing.len());

        let huge = item(
            InboxKind::UnclassifiableData,
            &format!(
                "unclassifiable:/{}:0123456789abcdef@1",
                "w".repeat(LISTING_BYTES)
            ),
            &json!({}),
        );
        let mut due = vec![huge.clone()];
        due.extend(small);
        assert_eq!(batch(&due, 30), std::slice::from_ref(&huge.key));
    }

    /// Every kind has a view, and a kind that keeps its whole evidence still has it cut.
    #[test]
    fn owners_kinds_are_cut_too() {
        let logs: Vec<_> = (0..9).map(|i| json!(format!("/l/{i}.log"))).collect();
        let failing = item(
            InboxKind::ArbiterFailing,
            "arbiter_failing:p@1",
            &json!({"passes": [{"id": "p"}], "logs": logs}),
        );
        let view = brief(&failing);
        assert_eq!(view.evidence.keys().collect::<Vec<_>>(), ["logs"]);
        assert_eq!(view.omitted["/evidence/logs"], 9 - LIST_ENTRIES as u64);
        let stranded = item(
            InboxKind::ReclaimStranded,
            "reclaim_stranded:p@1",
            &json!({"entries": (0..9).collect::<Vec<_>>(), "a/b": [1, 2, 3, 4, 5, 6]}),
        );
        let view = brief(&stranded);
        assert_eq!(view.omitted["/evidence/entries"], 9 - LIST_ENTRIES as u64);
        assert_eq!(view.omitted["/evidence/a~1b"], 6 - LIST_ENTRIES as u64);
    }
}
