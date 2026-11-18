//! The page of the read-only web UI (`docs/architecture.md` section 10.6): the snapshot
//! rendered as HTML, with every value from it escaped as text, and the snapshot itself
//! embedded as JSON. Nothing is computed in the browser but the times: the inline script
//! rewrites each into the viewer's local time with its age and marks an observation time
//! that has gone stale since the page was written. Without the script, or where it fails,
//! the page keeps its UTC times and the stale marks of when it was written.
//!
//! Every part shows when it was observed, and a part observed longer ago than
//! [`PageOptions::stale_after_ms`] is marked stale; records are events, shown with when they
//! happened, and the part that lists them shows when they were read.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::contract::{
    ArbiterPass, DiskReport, DiskState, Error, ErrorCode, InboxItem, InboxStatus, InboxSubject,
    ItemArbiter, Repo, ScopedError, Snapshot, Worktree,
};

/// The page's content security policy: its own inline style and script, nothing loaded or
/// sent, no inline style or script attributes, no base URL, no form target.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'unsafe-inline'; \
     script-src-attr 'none'; style-src 'unsafe-inline'; style-src-attr 'none'; \
     base-uri 'none'; form-action 'none'";

const STYLE: &str = include_str!("page.css");
const SCRIPT: &str = include_str!("page.js");

/// The last time an ISO 8601 date with a four-digit year shows, 9999-12-31T23:59:59.999Z;
/// a later one is shown as unknown.
const LAST_MS: u64 = 253_402_300_799_999;

/// How the page presents a snapshot; not part of `cardinal.snapshot.v1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageOptions {
    /// A part observed longer ago than this is marked stale: when the page is written, and
    /// again by the script when it is viewed.
    pub stale_after_ms: u64,
}

/// The page for `snapshot`: one self-contained HTML document.
pub fn render(snapshot: &Snapshot, options: PageOptions) -> Result<String, Error> {
    let data = script_json(snapshot)?;
    let mut page = Page {
        out: String::with_capacity(64 * 1024 + data.len()),
        generated_ms: snapshot.generated_ms,
        stale_after_ms: options.stale_after_ms,
        next_id: 0,
        errors: &snapshot.errors,
    };
    page.raw("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    page.raw("<meta http-equiv=\"Content-Security-Policy\" content=\"");
    page.raw(CONTENT_SECURITY_POLICY);
    page.raw("\">\n<meta name=\"referrer\" content=\"no-referrer\">\n");
    page.raw("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    page.raw("<meta name=\"color-scheme\" content=\"light dark\">\n");
    let host = &snapshot.host;
    let host_name = if host.hostname.is_empty() {
        &host.id
    } else {
        &host.hostname
    };
    page.el("title", &[], |p| p.text(&format!("cardinal: {host_name}")));
    page.raw("\n<style>\n");
    page.raw(STYLE);
    page.raw("</style>\n</head>\n");
    let stale_after = options.stale_after_ms.to_string();
    page.open("body", &[("data-stale-after-ms", &stale_after)]);
    page.header(snapshot);
    page.el("main", &[], |p| {
        let (owner, arbiter): (Vec<&InboxItem>, Vec<&InboxItem>) = (
            snapshot
                .inbox
                .iter()
                .filter(|i| i.needs_owner)
                .map(|i| &i.item)
                .collect(),
            snapshot
                .inbox
                .iter()
                .filter(|i| !i.needs_owner)
                .map(|i| &i.item)
                .collect(),
        );
        p.owner_section(&owner);
        p.disk_section(&snapshot.disk);
        p.arbiter_items_section(&arbiter);
        p.worktree_section(snapshot);
        p.reservation_section(snapshot);
        p.arbiter_section(snapshot.arbiter.as_ref());
        p.record_section(snapshot);
    });
    page.raw("\n<script type=\"application/json\" id=\"cardinal-snapshot\">");
    page.out.push_str(&data);
    page.raw("</script>\n<script>\n");
    page.raw(SCRIPT);
    page.raw("</script>\n</body>\n</html>\n");
    Ok(page.out)
}

/// `value` as JSON that can sit inside a `<script>` element: `<`, `>`, and `&` are written
/// as escapes, so no string in it can close the element or open a comment.
fn script_json<T: Serialize>(value: &T) -> Result<String, Error> {
    let json = serde_json::to_string(value)
        .map_err(|err| Error::new(ErrorCode::Internal, err.to_string()))?;
    Ok(json
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026"))
}

/// The HTML being written. Markup comes only from string literals in this file; every value
/// from the snapshot goes through [`Page::text`] or an attribute value, both escaped.
struct Page<'a> {
    out: String,
    generated_ms: u64,
    stale_after_ms: u64,
    next_id: u32,
    /// The parts of the snapshot that could not be read, or were read only in part.
    errors: &'a [ScopedError],
}

impl Page<'_> {
    fn raw(&mut self, html: &'static str) {
        self.out.push_str(html);
    }

    fn text(&mut self, text: &str) {
        escape_into(&mut self.out, text);
    }

    fn open(&mut self, tag: &'static str, attrs: &[(&'static str, &str)]) {
        self.out.push('<');
        self.out.push_str(tag);
        for (name, value) in attrs {
            self.out.push(' ');
            self.out.push_str(name);
            self.out.push_str("=\"");
            escape_into(&mut self.out, value);
            self.out.push('"');
        }
        self.out.push('>');
    }

    fn close(&mut self, tag: &'static str) {
        self.out.push_str("</");
        self.out.push_str(tag);
        self.out.push('>');
    }

    fn el(
        &mut self,
        tag: &'static str,
        attrs: &[(&'static str, &str)],
        body: impl FnOnce(&mut Self),
    ) {
        self.open(tag, attrs);
        body(self);
        self.close(tag);
    }

    fn el_text(&mut self, tag: &'static str, attrs: &[(&'static str, &str)], text: &str) {
        self.el(tag, attrs, |p| p.text(text));
    }

    fn id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}-{}", self.next_id)
    }

    fn badge(&mut self, text: &str, class: &str) {
        let class = format!("badge {class}");
        self.el_text("span", &[("class", &class)], text);
    }

    fn notice(&mut self, text: &str) {
        self.el_text("p", &[("class", "notice"), ("role", "note")], text);
    }

    /// Shows each error of the part `scope` as a notice; whether there was any, in which case
    /// the part is incomplete and an empty part is not "nothing".
    fn incomplete(&mut self, scope: &str) -> bool {
        let errors = self.errors;
        let mut any = false;
        for error in errors.iter().filter(|e| e.scope == scope) {
            any = true;
            self.el("p", &[("class", "notice"), ("role", "note")], |p| {
                p.el_text("strong", &[], "Not read in full: ");
                p.text(&error.message);
            });
        }
        any
    }

    /// The empty part's message: `nothing` when the part was read in full, `unread` when not.
    fn empty(&mut self, incomplete: bool, nothing: &str, unread: &str) {
        self.el_text("p", &[], if incomplete { unread } else { nothing });
    }

    /// A path, key, or ID, which may break after each `/`, `:`, or `@`, so a table column
    /// narrows to its longest segment rather than to one character.
    fn code(&mut self, text: &str) {
        self.open("code", &[]);
        let mut rest = text;
        while let Some(at) = rest.find(['/', ':', '@']) {
            self.text(&rest[..=at]);
            self.raw("<wbr>");
            rest = &rest[at + 1..];
        }
        self.text(rest);
        self.close("code");
    }

    /// When something happened.
    fn time(&mut self, ms: u64) {
        self.time_with(ms, false);
    }

    /// When a part was observed: marked stale when that was longer ago than the threshold
    /// when the page was written.
    fn observed(&mut self, ms: u64) {
        self.time_with(ms, true);
    }

    fn time_with(&mut self, ms: u64, observed: bool) {
        let Some((iso, text)) = utc(ms) else {
            self.badge("unknown time", "unknown");
            return;
        };
        let ms_text = ms.to_string();
        let class = if observed { "observed" } else { "event" };
        self.el_text(
            "time",
            &[("class", class), ("datetime", &iso), ("data-ms", &ms_text)],
            &text,
        );
        if observed && self.generated_ms.saturating_sub(ms) > self.stale_after_ms {
            self.raw(" ");
            self.badge("stale", "stale");
        }
    }

    fn section(&mut self, id: &'static str, title: &str, body: impl FnOnce(&mut Self)) {
        let heading = format!("{id}-heading");
        self.el(
            "section",
            &[("id", id), ("aria-labelledby", &heading)],
            |p| {
                p.el_text("h2", &[("id", &heading)], title);
                body(p);
            },
        );
    }

    /// A data table in a focusable scroller, with its column headers.
    fn table(&mut self, caption: &str, headers: &[&str], rows: impl FnOnce(&mut Self)) {
        let id = self.id("caption");
        self.el(
            "div",
            &[
                ("class", "table-wrap"),
                ("tabindex", "0"),
                ("role", "region"),
                ("aria-labelledby", &id),
            ],
            |p| {
                p.el("table", &[], |p| {
                    p.el_text("caption", &[("id", &id)], caption);
                    p.el("thead", &[], |p| {
                        p.el("tr", &[], |p| {
                            for header in headers {
                                p.el_text("th", &[("scope", "col")], header);
                            }
                        });
                    });
                    p.el("tbody", &[], rows);
                });
            },
        );
    }

    /// One row: its header cell, then the data cells.
    fn row(&mut self, header: impl FnOnce(&mut Self), cells: impl FnOnce(&mut Self)) {
        self.el("tr", &[], |p| {
            p.el("th", &[("scope", "row")], header);
            cells(p);
        });
    }

    fn td(&mut self, body: impl FnOnce(&mut Self)) {
        self.el("td", &[], body);
    }

    fn td_text(&mut self, text: &str) {
        self.el_text("td", &[], text);
    }

    fn pair(&mut self, term: &str, body: impl FnOnce(&mut Self)) {
        self.el("div", &[], |p| {
            p.el_text("dt", &[], term);
            p.el("dd", &[], body);
        });
    }

    fn json(&mut self, summary: &str, value: &Value) {
        let pretty = serde_json::to_string_pretty(value).unwrap_or_default();
        self.el("details", &[], |p| {
            p.el_text("summary", &[], summary);
            p.el_text("pre", &[], &pretty);
        });
    }

    fn header(&mut self, snapshot: &Snapshot) {
        let host = &snapshot.host;
        self.el("header", &[], |p| {
            let name = if host.hostname.is_empty() {
                "this host"
            } else {
                &host.hostname
            };
            p.el_text("h1", &[], &format!("cardinal on {name}"));
            p.el_text(
                "p",
                &[("class", "muted")],
                &format!(
                    "Host {}, {} {}. This page is read-only and sends nothing.",
                    host.id,
                    name_of(&host.os),
                    host.arch
                ),
            );
            p.el("p", &[], |p| {
                p.text("Generated ");
                p.observed(snapshot.generated_ms);
            });
            p.incomplete("host");
            let mut parts: Vec<&str> = Vec::new();
            for error in &snapshot.errors {
                if !parts.contains(&error.scope.as_str()) {
                    parts.push(&error.scope);
                }
            }
            if !parts.is_empty() {
                p.notice(&format!(
                    "Some parts could not be read in full, and this page shows only what could \
                     be: {}.",
                    parts.join(", ")
                ));
            }
            let owner = snapshot.inbox.iter().filter(|i| i.needs_owner).count();
            let arbiter = snapshot.inbox.len() - owner;
            let read_in_full = !snapshot.errors.iter().any(|e| e.scope == "inbox");
            p.el_text(
                "p",
                &[],
                &if read_in_full {
                    format!("{owner} for the owner, {arbiter} open for the arbiter.")
                } else {
                    format!(
                        "At least {owner} for the owner and {arbiter} open for the arbiter: \
                         the inbox could not be read in full."
                    )
                },
            );
        });
    }

    fn owner_section(&mut self, items: &[&InboxItem]) {
        self.section("owner", "For the owner", |p| {
            let incomplete = p.incomplete("inbox");
            if items.is_empty() {
                p.empty(
                    incomplete,
                    "Nothing waits for the owner.",
                    "No item that could be read waits for the owner.",
                );
            }
            for item in items {
                p.item(item, true);
            }
        });
    }

    fn arbiter_items_section(&mut self, items: &[&InboxItem]) {
        self.section("inbox", "Open items for the arbiter", |p| {
            let incomplete = p.incomplete("inbox");
            if items.is_empty() {
                p.empty(
                    incomplete,
                    "No open item for the arbiter.",
                    "No item that could be read is open for the arbiter.",
                );
            }
            for item in items {
                p.item(item, false);
            }
        });
    }

    /// An inbox item with its evidence; the owner's carry the commands that answer them.
    fn item(&mut self, item: &InboxItem, for_owner: bool) {
        let id = self.id("item");
        let class = if for_owner { "item owner" } else { "item" };
        let report = find_report(&item.evidence);
        self.el(
            "article",
            &[("class", class), ("aria-labelledby", &id)],
            |p| {
                p.el("h3", &[("id", &id)], |p| {
                    p.text(&label(&name_of(&item.kind)));
                    p.raw(" ");
                    let status = name_of(&item.status);
                    p.badge(&status, &status);
                });
                if item.status == InboxStatus::Escalated
                    && let Some(resolution) = &item.resolution
                {
                    p.el("p", &[("class", "question")], |p| {
                        p.el_text("strong", &[], "Question: ");
                        p.text(if resolution.note.is_empty() {
                            "(no note)"
                        } else {
                            &resolution.note
                        });
                    });
                }
                p.el("dl", &[], |p| {
                    p.pair("Key", |p| p.code(&item.key));
                    p.pair("Subject", |p| p.subject(&item.subject));
                    p.pair("Raised", |p| p.time(item.created_ms));
                    p.pair("Last seen", |p| {
                        p.observed(item.updated_ms);
                        p.text(&format!(", {} times", item.seen));
                    });
                    p.pair("Arbiter", |p| p.item_arbiter(item.arbiter.as_ref()));
                    if !matches!(report, Found::Whole(_)) {
                        p.scalars(&item.evidence);
                    }
                });
                match &report {
                    Found::Whole(report) | Found::Nested(report) => p.disk_report(report),
                    Found::Unreadable(why) => p.notice(&format!(
                        "The evidence holds a disk report that could not be read ({why}); \
                     the evidence is shown as it is stored."
                    )),
                    Found::None => {}
                }
                p.json("Evidence", &Value::Object(item.evidence.clone()));
                if for_owner {
                    p.commands(item, &report);
                }
            },
        );
    }

    /// The evidence's plain values, one per line.
    fn scalars(&mut self, evidence: &Map<String, Value>) {
        for (key, value) in evidence {
            let text = match value {
                Value::Null => "none".to_owned(),
                Value::String(s) => s.clone(),
                Value::Bool(_) | Value::Number(_) => value.to_string(),
                Value::Array(_) | Value::Object(_) => continue,
            };
            self.pair(&label(key), |p| p.code(&text));
        }
    }

    fn subject(&mut self, subject: &InboxSubject) {
        let mut any = false;
        if let Some(worktree) = &subject.worktree {
            self.code(worktree);
            any = true;
        }
        if let Some(branch) = &subject.branch {
            self.text(&format!(
                " branch {}",
                branch.as_deref().unwrap_or("(none)")
            ));
            any = true;
        }
        if let Some(head) = &subject.head {
            self.text(" at ");
            self.code(&short(head.as_deref()));
            any = true;
        }
        if let Some(job) = &subject.job_id {
            self.text(" job ");
            self.code(job);
            any = true;
        }
        if let Some(session) = &subject.session_id {
            self.text(" session ");
            self.code(session);
            any = true;
        }
        if !any {
            self.text("none");
        }
    }

    fn item_arbiter(&mut self, arbiter: Option<&ItemArbiter>) {
        let Some(arbiter) = arbiter else {
            self.text("not assigned");
            return;
        };
        self.text("pass ");
        self.code(&arbiter.pass);
        self.text(", assigned ");
        self.time(arbiter.assigned_ms);
        match &arbiter.outcome {
            Some(outcome) => self.text(&format!(", {outcome}")),
            None => self.text(", running"),
        }
    }

    fn commands(&mut self, item: &InboxItem, report: &Found) {
        self.el_text("p", &[], "Answer with one of:");
        let key = quote(&item.key);
        let answers: Vec<String> = ["resolved", "dismissed", "escalated"]
            .iter()
            .map(|outcome| format!("cardinal inbox resolve {key} --outcome {outcome} --note '…'"))
            .collect();
        let answers = answers.join("\n");
        self.el_text("pre", &[], &answers);
        let mut plans: Vec<&str> = Vec::new();
        if let Some(Value::String(plan)) = item.evidence.get("plan_id") {
            plans.push(plan);
        }
        if let Found::Whole(report) | Found::Nested(report) = report {
            plans.extend(report.targets.iter().map(|t| t.plan_id.as_str()));
        }
        plans.dedup();
        if !plans.is_empty() {
            self.el_text(
                "p",
                &[],
                "Recheck a plan (runs its checks again and returns a report):",
            );
            let rechecks: Vec<String> = plans
                .iter()
                .map(|plan| {
                    format!(
                        "cardinal act {} --caller owner --owner-approved",
                        quote(plan)
                    )
                })
                .collect();
            let rechecks = rechecks.join("\n");
            self.el_text("pre", &[], &rechecks);
        }
    }

    fn disk_report(&mut self, report: &DiskReport) {
        self.el_text("h4", &[], "Disk report");
        self.el("dl", &[], |p| {
            p.pair("Volume", |p| p.code(&report.volume));
            p.pair("Reading", |p| {
                p.text(&format!("{} free at ", bytes(report.reading.free_bytes)));
                p.observed(report.reading.at_ms);
            });
            p.pair("Lines (warning / recovery / critical)", |p| {
                p.text(&lines(&report.lines));
            });
            p.pair("Checks evaluated", |p| p.observed(report.checks_ms));
            p.pair("Shortfall", |p| {
                if report.shortfall_bytes == 0 {
                    p.text("none: the listed targets reach the recovery line");
                } else {
                    p.text(&bytes(report.shortfall_bytes));
                }
            });
        });
        if report.targets.is_empty() {
            self.el_text("p", &[], "No target listed.");
        } else {
            self.table(
                "Targets, in reclamation order",
                &[
                    "Target",
                    "Worktree",
                    "Plan",
                    "Exclusive bytes",
                    "Status",
                    "Freed",
                ],
                |p| {
                    for t in &report.targets {
                        p.row(
                            |p| p.code(&t.target),
                            |p| {
                                p.td(|p| p.code(&t.worktree));
                                p.td(|p| p.code(&t.plan_id));
                                p.td_text(&bytes(t.exclusive_bytes));
                                p.td_text(
                                    &t.status
                                        .map_or_else(|| "listed".to_owned(), |s| name_of(&s)),
                                );
                                p.td_text(&match t.freed_bytes {
                                    None => "not executed".to_owned(),
                                    Some(freed) => bytes_or_unknown(freed),
                                });
                            },
                        );
                    }
                },
            );
        }
        if !report.blocked.is_empty() {
            self.table(
                "Blocked",
                &["Target", "Worktree", "First failed check", "Bytes"],
                |p| {
                    for b in &report.blocked {
                        p.row(
                            |p| p.code(&b.target),
                            |p| {
                                p.td(|p| p.code(&b.worktree));
                                p.td_text(&b.check);
                                p.td_text(&bytes(b.bytes));
                            },
                        );
                    }
                },
            );
        }
    }

    fn disk_section(&mut self, disks: &[DiskState]) {
        self.section("disk", "Disk", |p| {
            let incomplete = p.incomplete("disk");
            if disks.is_empty() {
                p.empty(
                    incomplete,
                    "No watched volume has been read yet.",
                    "No disk state could be read.",
                );
                return;
            }
            p.table(
                "Watched volumes",
                &[
                    "Mount",
                    "State",
                    "Since",
                    "Free",
                    "Total",
                    "Last reading",
                    "Lines (warning / recovery / critical)",
                    "Readings",
                ],
                |p| {
                    for disk in disks {
                        p.row(|p| p.code(&disk.mount), |p| p.disk_cells(disk));
                    }
                },
            );
        });
    }

    fn disk_cells(&mut self, disk: &DiskState) {
        let last = disk.readings.iter().rev().find(|r| r.free_bytes.is_some());
        let newest = disk.readings.last();
        self.td(|p| {
            let state = name_of(&disk.state);
            p.badge(&state, &state);
        });
        self.td(|p| p.time(disk.since_ms));
        self.td_text(&bytes_or_unknown(last.and_then(|r| r.free_bytes)));
        self.td_text(&bytes_or_unknown(last.and_then(|r| r.total_bytes)));
        self.td(|p| {
            if let Some(reading) = last {
                p.observed(reading.at_ms);
            } else {
                p.text("none ");
                p.badge("no reading", "unknown");
            }
            if let Some(newest) = newest.filter(|r| r.free_bytes.is_none()) {
                p.raw("<br>");
                p.text("the newest reading failed at ");
                p.time(newest.at_ms);
            }
        });
        self.td_text(&lines(&disk.lines));
        self.td(|p| {
            p.el("details", &[], |p| {
                p.el_text(
                    "summary",
                    &[],
                    &format!(
                        "{} readings, {}",
                        disk.readings.len(),
                        name_of(&disk.measure)
                    ),
                );
                p.el("ol", &[], |p| {
                    for reading in &disk.readings {
                        p.el("li", &[], |p| {
                            p.time(reading.at_ms);
                            p.text(&match reading.free_bytes {
                                Some(free) => format!(": {} free", bytes(free)),
                                None => ": failed".to_owned(),
                            });
                        });
                    }
                });
            });
        });
    }

    fn worktree_section(&mut self, snapshot: &Snapshot) {
        self.section("worktrees", "Worktrees", |p| {
            let incomplete = p.incomplete("observation");
            let Some(observation) = &snapshot.observation else {
                p.empty(
                    incomplete,
                    "No full observation yet.",
                    "The last full observation could not be read.",
                );
                return;
            };
            p.el("p", &[("class", "muted")], |p| {
                p.text("Observed ");
                p.observed(observation.observed_ms);
            });
            if !observation.errors.is_empty() {
                p.el("div", &[("class", "notice"), ("role", "note")], |p| {
                    p.el_text(
                        "strong",
                        &[],
                        "The observation could not observe everything:",
                    );
                    p.el("ul", &[], |p| {
                        for error in &observation.errors {
                            p.el_text("li", &[], &format!("{}: {}", error.scope, error.message));
                        }
                    });
                });
            }
            if observation.repos.is_empty() {
                p.el_text("p", &[], "No repository observed.");
            }
            for repo in &observation.repos {
                p.repo(repo);
            }
        });
    }

    fn repo(&mut self, repo: &Repo) {
        self.el("h3", &[], |p| p.code(&repo.main_worktree));
        self.el("dl", &[], |p| {
            p.pair("Common dir", |p| p.code(&repo.common_dir));
            p.pair("GitHub", |p| {
                p.text(repo.github.as_deref().unwrap_or("none"));
            });
            p.pair("Base", |p| match &repo.base {
                None => p.text("none"),
                Some(base) => {
                    p.text(&format!("{} at ", base.ref_name));
                    p.code(&short(Some(&base.oid)));
                }
            });
            p.pair("gh", |p| {
                p.text(&name_of(&repo.gh));
                if let Some(error) = &repo.gh_error {
                    p.text(&format!(": {error}"));
                }
            });
            p.pair("Profiles", |p| {
                p.text(&if repo.profiles.is_empty() {
                    "none".to_owned()
                } else {
                    repo.profiles.join(", ")
                });
            });
        });
        self.table(
            &format!("Worktrees of {}", repo.main_worktree),
            &[
                "Path",
                "Branch",
                "HEAD",
                "Managed by",
                "Dirty",
                "Unpushed",
                "Pull request",
                "Merge evidence",
                "Content in base",
                "In use",
                "Reclaim",
                "Last activity",
                "Size",
            ],
            |p| {
                for worktree in &repo.worktrees {
                    p.row(
                        |p| {
                            p.code(&worktree.path);
                            if worktree.main {
                                p.text(" (main)");
                            }
                        },
                        |p| p.worktree_cells(worktree),
                    );
                }
            },
        );
        let ignored: Vec<(&Worktree, &crate::contract::IgnoredEntry)> = repo
            .worktrees
            .iter()
            .flat_map(|w| w.ignored.iter().map(move |e| (w, e)))
            .collect();
        if !ignored.is_empty() {
            self.table(
                &format!("Ignored data in {}", repo.main_worktree),
                &[
                    "Entry",
                    "Worktree",
                    "Class",
                    "Regenerable pattern",
                    "Allocated",
                    "Aliased",
                ],
                |p| {
                    for (worktree, entry) in ignored {
                        p.row(
                            |p| p.code(&entry.path),
                            |p| {
                                p.td(|p| p.code(&worktree.path));
                                p.td_text(&name_of(&entry.class));
                                p.td_text(if entry.regenerable_match { "yes" } else { "no" });
                                p.td_text(&bytes_or_unknown(entry.allocated_bytes));
                                p.td_text(&bytes_or_unknown(entry.aliased_bytes));
                            },
                        );
                    }
                },
            );
        }
    }

    fn worktree_cells(&mut self, w: &Worktree) {
        let f = &w.facts;
        self.td_text(w.branch.as_deref().unwrap_or("(detached)"));
        self.td(|p| p.code(&short(w.head.as_deref())));
        self.td_text(w.managed_by.as_deref().unwrap_or("no"));
        self.td(|p| {
            p.text(&name_of(&f.dirty));
            if let Some(unknown) = &f.dirty_unknown {
                p.text(&format!(" (filtered paths: {})", unknown.paths.join(", ")));
            }
        });
        self.td_text(&name_of(&f.unpushed));
        self.td(|p| {
            p.text(&format!("open: {}", name_of(&f.open_pr)));
            if let Some(closed) = &f.closed_pr {
                p.text(&format!("; closed #{}", closed.number));
            }
        });
        self.td(|p| {
            p.text(&name_of(&f.merge_evidence));
            if let Some(merged) = &f.merged_pr {
                p.text(&format!(" #{}", merged.number));
            }
        });
        self.td(|p| {
            p.text(&name_of(&f.content_in_base));
            if !f.content_diff.is_empty() {
                p.text(&format!(" ({} paths differ)", f.content_diff.len()));
            }
        });
        self.el("td", &[("class", "in-use")], |p| {
            if f.in_use.value {
                p.text("yes");
                if !f.in_use.jobs.is_empty() {
                    p.text(&format!(": {}", f.in_use.jobs.join(", ")));
                }
            } else if w.reclaim.unknown.iter().any(|check| check == "not_in_use") {
                p.badge("unknown", "unknown");
            } else {
                p.text("no");
            }
        });
        self.td(|p| {
            if w.reclaim.candidate {
                p.text("candidate");
                return;
            }
            p.text("no");
            if !w.reclaim.failed.is_empty() {
                p.text(&format!("; failed: {}", w.reclaim.failed.join(", ")));
            }
            if !w.reclaim.unknown.is_empty() {
                p.text(&format!("; unknown: {}", w.reclaim.unknown.join(", ")));
            }
        });
        self.td(|p| match w.last_activity_ms {
            Some(ms) => p.time(ms),
            None => p.badge("unknown", "unknown"),
        });
        self.td_text(&bytes_or_unknown(w.allocated_bytes));
    }

    fn reservation_section(&mut self, snapshot: &Snapshot) {
        let reservations = &snapshot.reservations;
        self.section("reservations", "Reservations", |p| {
            p.el("p", &[("class", "muted")], |p| {
                p.text("Read ");
                p.observed(reservations.observed_ms);
            });
            let incomplete = p.incomplete("reservations");
            if reservations.items.is_empty() {
                p.empty(
                    incomplete,
                    "No reservation.",
                    "No reservation could be read.",
                );
                return;
            }
            p.table(
                "Reservations",
                &[
                    "Job",
                    "State",
                    "Derived",
                    "Verified",
                    "Members",
                    "Resources",
                    "Admission",
                    "Class / rule",
                    "Worktree",
                    "Since",
                    "Command",
                ],
                |p| {
                    for view in &reservations.items {
                        let r = &view.reservation;
                        p.row(
                            |p| p.code(&r.job_id),
                            |p| {
                                p.td_text(&name_of(&r.state));
                                p.td(|p| {
                                    let derived = name_of(&view.derived);
                                    let class = if derived == "live" { "ok" } else { "warning" };
                                    p.badge(&derived, class);
                                });
                                p.td_text(if view.verified { "yes" } else { "no" });
                                p.td(|p| {
                                    let members: Vec<String> = view
                                        .members
                                        .iter()
                                        .map(|m| format!("{} {}", m.pid, m.name))
                                        .collect();
                                    p.text(&if members.is_empty() {
                                        "none".to_owned()
                                    } else {
                                        members.join(", ")
                                    });
                                });
                                let resources: Vec<String> = r
                                    .resources
                                    .iter()
                                    .map(|(name, amount)| format!("{name} {amount}"))
                                    .collect();
                                p.td_text(&if resources.is_empty() {
                                    "none".to_owned()
                                } else {
                                    resources.join(", ")
                                });
                                p.td_text(&name_of(&r.admission));
                                p.td_text(&format!(
                                    "{} / {}",
                                    r.class.as_deref().unwrap_or("none"),
                                    r.rule.as_deref().unwrap_or("none")
                                ));
                                p.td(|p| match &r.worktree {
                                    None => p.text("none"),
                                    Some(w) => {
                                        p.code(&w.path);
                                        if !w.protected {
                                            p.text(" (unprotected)");
                                        }
                                    }
                                });
                                p.td(|p| p.time(r.created_ms));
                                p.td(|p| {
                                    let argv: Vec<String> =
                                        r.argv.iter().map(|w| quote(w)).collect();
                                    p.code(&argv.join(" "));
                                    p.text(" in ");
                                    p.code(&r.cwd);
                                });
                            },
                        );
                    }
                },
            );
        });
    }

    fn arbiter_section(&mut self, pass: Option<&ArbiterPass>) {
        self.section("arbiter", "Arbiter", |p| {
            let incomplete = p.incomplete("arbiter");
            let Some(pass) = pass else {
                p.empty(
                    incomplete,
                    "No arbiter pass yet.",
                    "The last arbiter pass could not be read.",
                );
                return;
            };
            p.el("dl", &[], |p| {
                p.pair("Last pass", |p| p.code(&pass.id));
                p.pair("Started", |p| {
                    if pass.ended_ms.is_none() {
                        p.observed(pass.started_ms);
                    } else {
                        p.time(pass.started_ms);
                    }
                });
                p.pair("Ended", |p| match pass.ended_ms {
                    Some(ended) => p.observed(ended),
                    None => p.text("running"),
                });
                p.pair("Outcome", |p| {
                    p.text(
                        &pass
                            .outcome
                            .as_ref()
                            .map_or_else(|| "running".to_owned(), ToString::to_string),
                    );
                });
                p.pair("Items", |p| {
                    if pass.items.is_empty() {
                        p.text("none");
                    }
                    p.el("ul", &[], |p| {
                        for key in &pass.items {
                            p.el("li", &[], |p| p.code(key));
                        }
                    });
                });
                p.pair("Log", |p| p.code(&pass.log));
                p.pair("Failures in a row", |p| {
                    p.text(&pass.failures_in_a_row.to_string());
                });
            });
        });
    }

    fn record_section(&mut self, snapshot: &Snapshot) {
        self.section("records", "Recent records", |p| {
            p.el("p", &[("class", "muted")], |p| {
                p.text("Read when this page was generated, ");
                p.observed(snapshot.generated_ms);
                p.text("; each record shows when it was recorded.");
            });
            let incomplete = p.incomplete("records");
            if snapshot.records.is_empty() {
                p.empty(incomplete, "No record yet.", "No record could be read.");
                return;
            }
            p.table(
                &format!(
                    "The last {} act, planned, and inbox records{}, newest first",
                    snapshot.records.len(),
                    if incomplete {
                        " that could be read"
                    } else {
                        ""
                    }
                ),
                &["Recorded", "Kind", "Subject", "Data"],
                |p| {
                    for record in snapshot.records.iter().rev() {
                        p.row(
                            |p| p.time(record.ts_ms),
                            |p| {
                                p.td_text(&label(&name_of(&record.kind)));
                                p.td(|p| {
                                    if let Some(plan) = &record.plan_id {
                                        p.text("plan ");
                                        p.code(plan);
                                    } else if let Some(key) = &record.inbox_key {
                                        p.code(key);
                                    } else if let Some(job) = &record.job_id {
                                        p.text("job ");
                                        p.code(job);
                                    } else {
                                        p.text("none");
                                    }
                                });
                                p.td(|p| {
                                    p.code(&Value::Object(record.data.clone()).to_string());
                                });
                            },
                        );
                    }
                },
            );
        });
    }
}

/// A disk report in an item's evidence: the evidence itself, or an object in it (up to two
/// levels down) that has a disk report's `reading` and `targets`.
enum Found {
    None,
    Whole(DiskReport),
    Nested(DiskReport),
    /// Something shaped like a report that does not read as one, with why.
    Unreadable(String),
}

fn find_report(evidence: &Map<String, Value>) -> Found {
    let looks_like =
        |m: &Map<String, Value>| m.contains_key("reading") && m.contains_key("targets");
    let read = |m: &Map<String, Value>| {
        serde_json::from_value::<DiskReport>(Value::Object(m.clone())).map_err(|e| e.to_string())
    };
    if looks_like(evidence) {
        return read(evidence).map_or_else(Found::Unreadable, Found::Whole);
    }
    let objects = |m: &Map<String, Value>| -> Vec<Map<String, Value>> {
        m.values().filter_map(|v| v.as_object().cloned()).collect()
    };
    for inner in objects(evidence) {
        let candidates = std::iter::once(inner.clone()).chain(objects(&inner));
        for candidate in candidates {
            if looks_like(&candidate) {
                return read(&candidate).map_or_else(Found::Unreadable, Found::Nested);
            }
        }
    }
    Found::None
}

fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

/// An enum's JSON name, with spaces for underscores.
fn name_of<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(name)) => name.replace('_', " "),
        _ => "unknown".to_owned(),
    }
}

/// `some_name` as `Some name`.
fn label(name: &str) -> String {
    let name = name.replace('_', " ");
    let mut chars = name.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn short(oid: Option<&str>) -> String {
    oid.map_or_else(|| "none".to_owned(), |oid| oid.chars().take(12).collect())
}

/// A word as the shell reads it back: as it is when it holds only safe characters, else in
/// single quotes.
fn quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_/.:@%+=,-".contains(c));
    if safe {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

fn lines(lines: &crate::contract::DiskLines) -> String {
    format!(
        "{} / {} / {}",
        bytes(lines.warning_bytes),
        bytes(lines.recovery_bytes),
        bytes(lines.critical_bytes)
    )
}

fn bytes_or_unknown(n: Option<u64>) -> String {
    n.map_or_else(|| "unknown".to_owned(), bytes)
}

/// Bytes in binary units with one decimal, computed in integers.
fn bytes(n: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut unit = 0;
    let mut divisor: u128 = 1;
    while unit + 1 < UNITS.len() && u128::from(n) >= divisor * 1024 {
        divisor *= 1024;
        unit += 1;
    }
    if unit == 0 {
        return format!("{n} B");
    }
    let tenths = (u128::from(n) * 10 + divisor / 2) / divisor;
    format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[unit])
}

/// `ms` as an ISO 8601 UTC timestamp and as text, or `None` past year 9999.
fn utc(ms: u64) -> Option<(String, String)> {
    if ms > LAST_MS {
        return None;
    }
    let secs = ms / 1000;
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (rest / 3600, rest % 3600 / 60, rest % 60);
    // Civil date from days since 1970-01-01 (Howard Hinnant's `civil_from_days`).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    Some((
        format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
            ms % 1000
        ),
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC"),
    ))
}

#[cfg(test)]
mod tests {
    use super::{bytes, quote, utc};

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0).unwrap().0, "1970-01-01T00:00:00.000Z");
        assert_eq!(
            utc(1_794_960_000_123).unwrap(),
            (
                "2026-11-18T00:00:00.123Z".to_owned(),
                "2026-11-18 00:00:00 UTC".to_owned()
            )
        );
        assert_eq!(utc(951_782_400_000).unwrap().0, "2000-02-29T00:00:00.000Z");
        assert_eq!(utc(super::LAST_MS).unwrap().0, "9999-12-31T23:59:59.999Z");
        assert_eq!(utc(super::LAST_MS + 1), None);
        assert_eq!(utc(8_640_000_000_000_001), None);
    }

    #[test]
    fn byte_units() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(3_221_225_472), "3.0 GiB");
        assert_eq!(bytes(u64::MAX), "16.0 EiB");
    }

    #[test]
    fn shell_words() {
        assert_eq!(quote("plain/path:1"), "plain/path:1");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote(""), "''");
    }
}
