//! Parsers for the git commands of `docs/architecture.md` section 8.2, and of the commands an
//! execution runs bound to the checked worktree (section 9.7). Every parser refuses output it
//! does not understand, so the facts built from it become `unknown` and an execution decides
//! nothing from it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::contract::StatusPath;

/// One entry of `git worktree list --porcelain -z`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListedWorktree {
    pub path: PathBuf,
    pub head: Option<String>,
    /// The short branch name (`refs/heads/` removed); `None` when detached.
    pub branch: Option<String>,
    pub bare: bool,
    pub locked: bool,
}

/// Parses `git worktree list --porcelain -z`: attribute lines end in NUL, and an empty line
/// (a second NUL) ends each worktree.
pub fn parse_worktree_list(bytes: &[u8]) -> Result<Vec<ListedWorktree>, String> {
    let mut worktrees = Vec::new();
    let mut current: Option<ListedWorktree> = None;
    for field in bytes.split(|&b| b == 0) {
        if field.is_empty() {
            if let Some(done) = current.take() {
                worktrees.push(done);
            }
            continue;
        }
        let (key, value) = match field.iter().position(|&b| b == b' ') {
            Some(i) => (&field[..i], Some(&field[i + 1..])),
            None => (field, None),
        };
        if key == b"worktree" {
            if let Some(done) = current.take() {
                worktrees.push(done);
            }
            let path = value.ok_or("worktree line without a path")?;
            current = Some(ListedWorktree {
                path: path_from_bytes(path)?,
                ..ListedWorktree::default()
            });
            continue;
        }
        let entry = current
            .as_mut()
            .ok_or("worktree attribute before a worktree line")?;
        let text = |value: Option<&[u8]>| {
            value
                .and_then(|v| std::str::from_utf8(v).ok())
                .map(str::to_owned)
        };
        match key {
            b"HEAD" => entry.head = Some(text(value).ok_or("HEAD without an object ID")?),
            b"branch" => {
                let full = text(value).ok_or("branch without a name")?;
                entry.branch = Some(
                    full.strip_prefix("refs/heads/")
                        .map_or(full.clone(), str::to_owned),
                );
            }
            b"detached" => entry.branch = None,
            b"bare" => entry.bare = true,
            b"locked" => entry.locked = true,
            // `prunable`, and attributes added by later git versions, carry no fact cardinal
            // relies on.
            _ => {}
        }
    }
    if let Some(done) = current.take() {
        worktrees.push(done);
    }
    if worktrees.is_empty() {
        return Err("git listed no worktrees".into());
    }
    Ok(worktrees)
}

/// What `git status --porcelain=v2 -z --branch --untracked-files=all --ignored=matching`
/// reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// `None` on an unborn branch.
    pub head: Option<String>,
    /// `None` when detached.
    pub branch: Option<String>,
    pub upstream: Option<String>,
    /// Worktree-relative paths of the entries whose only change is a worktree-side
    /// modification (`1 .M`), which a filtered file shows without its filter (section 8.1).
    pub modified: Vec<PathBuf>,
    /// An entry other than an ignored one or a worktree-side modification.
    pub other_changes: bool,
    /// Worktree-relative paths of ignored entries, without a trailing `/`.
    pub ignored: Vec<PathBuf>,
    /// Every path listed as changed, unmerged, or untracked, with whether it is a tracked
    /// deletion, for the quiet period (section 9.8); a rename or copy by its new path.
    pub changed: Vec<StatusPath>,
}

/// Parses porcelain v2 status output with `-z`: every entry ends with a NUL, so output that
/// does not is incomplete.
pub fn parse_status(bytes: &[u8]) -> Result<Status, String> {
    if !bytes.is_empty() && !bytes.ends_with(b"\0") {
        return Err("status output is incomplete: its last entry has no terminator".into());
    }
    let mut status = Status::default();
    let mut saw_oid = false;
    let mut fields = bytes.split(|&b| b == 0);
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let kind = field[0];
        if field.len() < 2 || field[1] != b' ' {
            return Err(format!(
                "unexpected status entry {:?}",
                String::from_utf8_lossy(field)
            ));
        }
        let rest = &field[2..];
        match kind {
            b'#' => {
                let text = std::str::from_utf8(rest).map_err(|_| "status header is not UTF-8")?;
                let (key, value) = text.split_once(' ').unwrap_or((text, ""));
                match key {
                    "branch.oid" => {
                        saw_oid = true;
                        status.head = (value != "(initial)").then(|| value.to_owned());
                    }
                    "branch.head" => {
                        status.branch = (value != "(detached)").then(|| value.to_owned());
                    }
                    "branch.upstream" => status.upstream = Some(value.to_owned()),
                    _ => {}
                }
            }
            b'1' => {
                // `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`: the path is the rest.
                let mut parts = rest.splitn(8, |&b| b == b' ');
                let xy = parts.next().unwrap_or_default();
                let path = parts.nth(6).ok_or("changed entry without a path")?;
                status.changed.push(StatusPath {
                    path: path_from_bytes(path)?,
                    deleted: xy.contains(&b'D'),
                });
                if xy == b".M" {
                    status.modified.push(path_from_bytes(path)?);
                } else {
                    status.other_changes = true;
                }
            }
            b'u' | b'?' => {
                status.other_changes = true;
                // `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`, `? <path>`.
                let path = if kind == b'u' {
                    let mut parts = rest.splitn(10, |&b| b == b' ');
                    parts.nth(9).ok_or("unmerged entry without a path")?
                } else {
                    rest
                };
                status.changed.push(StatusPath {
                    path: path_from_bytes(path)?,
                    deleted: false,
                });
            }
            b'2' => {
                status.other_changes = true;
                // `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>`, followed by its
                // original path as a separate field.
                let mut parts = rest.splitn(9, |&b| b == b' ');
                let xy = parts.next().unwrap_or_default();
                let path = parts.nth(7).ok_or("rename entry without a path")?;
                status.changed.push(StatusPath {
                    path: path_from_bytes(path)?,
                    deleted: xy.contains(&b'D'),
                });
                // The output ends with a NUL, so an entry cut after its new path's NUL leaves an
                // empty last field here, never an original path.
                fields
                    .next()
                    .filter(|original| !original.is_empty())
                    .ok_or("rename entry without its original path")?;
            }
            b'!' => {
                let path = rest.strip_suffix(b"/").unwrap_or(rest);
                status.ignored.push(path_from_bytes(path)?);
            }
            _ => {
                return Err(format!(
                    "unexpected status entry {:?}",
                    String::from_utf8_lossy(field)
                ));
            }
        }
    }
    if !saw_oid {
        return Err("status output has no branch header".into());
    }
    Ok(status)
}

/// The filter drivers configured for a worktree, from `git config -z --name-only
/// --get-regexp '^filter\.'`: the subsection of each `filter.<driver>.<key>` name, which may
/// itself contain dots.
pub fn parse_filter_drivers(bytes: &[u8]) -> Result<BTreeSet<String>, String> {
    let mut drivers = BTreeSet::new();
    for name in bytes.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        let name = std::str::from_utf8(name)
            .map_err(|_| "a filter driver's configuration name is not UTF-8")?;
        let driver = name
            .strip_prefix("filter.")
            .and_then(|rest| rest.rsplit_once('.'))
            .map(|(driver, _key)| driver)
            .ok_or_else(|| format!("unexpected filter configuration name {name:?}"))?;
        drivers.insert(driver.to_owned());
    }
    Ok(drivers)
}

/// The paths of `git check-attr -z filter -- <paths>` output whose `filter` attribute may
/// name one of `drivers`. The output is `<path> NUL filter NUL <value> NUL` per path; the
/// value is `unspecified`, `unset`, `set`, or the driver's name. A value that names a
/// configured driver counts, `set` included: check-attr prints `set` both for a bare
/// attribute and for `filter=set`, so where a driver named `set` exists the two cannot be
/// told apart, and the path counts as filtered (its `dirty` fails toward `unknown`).
pub fn parse_filtered(
    bytes: &[u8],
    drivers: &BTreeSet<String>,
) -> Result<BTreeSet<PathBuf>, String> {
    let fields: Vec<&[u8]> = bytes.split(|&b| b == 0).collect();
    // The output ends in NUL, so the last field is empty (the only one for no output).
    let Some(([], fields)) = fields.split_last() else {
        return Err("check-attr output does not end in NUL".into());
    };
    if fields.len() % 3 != 0 {
        return Err("unexpected check-attr output".into());
    }
    let mut filtered = BTreeSet::new();
    for triple in fields.chunks(3) {
        let [path, attribute, value] = triple else {
            return Err("unexpected check-attr output".into());
        };
        if *attribute != b"filter" {
            return Err("check-attr reported another attribute than filter".into());
        }
        let value = String::from_utf8_lossy(value);
        if drivers.contains(&*value) {
            filtered.insert(path_from_bytes(path)?);
        }
    }
    Ok(filtered)
}

/// The `(major, minor, patch)` version of `git --version` output such as
/// `git version 2.54.0`, `git version 2.39.5 (Apple Git-154)`, or
/// `git version 2.45.1.windows.1`. A patch level without leading digits, or none, reads as
/// 0, the lowest.
pub fn parse_version(bytes: &[u8]) -> Option<(u32, u32, u32)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let version = text.trim().strip_prefix("git version ")?;
    let mut numbers = version.split(['.', ' ']);
    let major = numbers.next()?.parse().ok()?;
    let minor = numbers.next()?.parse().ok()?;
    let patch = numbers.next().map_or(0, |patch| {
        let digits = patch.bytes().take_while(u8::is_ascii_digit).count();
        patch[..digits].parse().unwrap_or(0)
    });
    Some((major, minor, patch))
}

/// What git prints, under `LC_ALL=C`, when the repository has no `origin`.
const NO_ORIGIN: &[u8] = b"error: No such remote 'origin'\n";

/// The answer of `git remote get-url origin`, run under `LC_ALL=C`, from its exit code and
/// output: the URL, `None` when git answers that there is no `origin` (exit 2, nothing on
/// stdout, and exactly git's line for a missing remote on stderr), or an error for any other
/// answer, a message on stderr included, so a read that failed is never taken for an absent
/// remote. An error never quotes the output, which may hold credentials.
pub fn remote_answer(
    code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Option<String>, String> {
    match code {
        Some(0) if stderr.is_empty() => parse_remote_url(stdout).map(Some),
        Some(2) if stdout.is_empty() && stderr == NO_ORIGIN => Ok(None),
        Some(code) => Err(format!(
            "exited with {code} and {} bytes on stderr",
            stderr.len()
        )),
        None => Err("was ended by a signal".into()),
    }
}

/// Parses `git remote get-url origin` output that exited 0: one non-empty line of UTF-8,
/// ending in its line feed. An error does not quote the output.
fn parse_remote_url(bytes: &[u8]) -> Result<String, String> {
    match bytes.strip_suffix(b"\n").map(std::str::from_utf8) {
        Some(Ok(url)) if !url.is_empty() && !url.contains('\n') => Ok(url.to_owned()),
        _ => Err("printed no single URL".into()),
    }
}

/// Whether `git config -z --get-regexp
/// '^(extensions\.partialclone|remote\..*\.(promisor|partialclonefilter))$'` output
/// describes a promisor repository: `extensions.partialClone` or a
/// `remote.<name>.partialCloneFilter` present (git registers a promisor remote from the
/// filter even where `promisor` is false), or a `remote.<name>.promisor` that is not false.
/// A value git would not read as a boolean counts as a promisor, so an unexpected
/// configuration fails closed.
pub fn parse_promisor(bytes: &[u8]) -> bool {
    bytes
        .split(|&b| b == 0)
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            let text = String::from_utf8_lossy(entry);
            let (key, value) = match text.split_once('\n') {
                Some((key, value)) => (key.to_owned(), Some(value.to_ascii_lowercase())),
                // A key without a value is a true boolean.
                None => (text.into_owned(), None),
            };
            !key.ends_with(".promisor")
                || !matches!(value.as_deref(), Some("false" | "no" | "off" | "0" | ""))
        })
}

/// Splits one `git ls-files -z --stage` entry into `"<mode> <object> <stage>"` and its
/// path.
pub fn parse_stage_entry(entry: &[u8]) -> Result<(&str, &Path), String> {
    let tab = entry
        .iter()
        .position(|&b| b == b'\t')
        .ok_or("ls-files entry without a path")?;
    let meta = std::str::from_utf8(&entry[..tab]).map_err(|_| "ls-files entry is not UTF-8")?;
    Ok((meta, path_ref_from_bytes(&entry[tab + 1..])?))
}

/// Whether `name` is a ref name by the rules of `git check-ref-format --allow-onelevel`:
/// no empty, `.`-leading, or `.lock`-ending component, no `..`, `@{`, control characters,
/// space, `~ ^ : ? * [ \\`, no leading `-`, not `@`, and no trailing `/` or `.`. Revision
/// syntax (`:path`, `@{-1}`, `HEAD~2`) is therefore never a ref name.
#[must_use]
pub fn is_ref_name(name: &str) -> bool {
    let bad_char = |c: char| c.is_control() || " ~^:?*[\\".contains(c);
    !name.is_empty()
        && name != "@"
        && !name.starts_with('-')
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("@{")
        && !name.chars().any(bad_char)
        && name.split('/').all(|c| {
            !c.is_empty() && !c.starts_with('.') && !c.to_ascii_lowercase().ends_with(".lock")
        })
}

/// Whether `oid` looks like a full object ID (SHA-1 or SHA-256).
#[must_use]
pub fn is_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One entry of `git ls-files -z -t --sparse`: its tag, and its path as the index stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// `H` for a tracked file, `S` for one marked skip-worktree or a sparse directory, and
    /// the other tags of `ls-files -t`.
    pub tag: u8,
    pub path: PathBuf,
}

impl IndexEntry {
    /// Whether git's own commands see the entry as skip-worktree or as a sparse directory
    /// (`S`), the `sparse_checkout` rule of section 9.7.
    #[must_use]
    pub fn sparse(&self) -> bool {
        self.tag == b'S'
    }
}

/// The tags `git ls-files -t` prints without `-v`.
const LS_FILES_TAGS: &[u8] = b"HSMRCK?U";

/// Parses `git ls-files -z -t --sparse`: `<tag> <path>` fields, each ending in NUL. Output
/// that does not end in NUL, a field without a known tag and a path, and anything else is an
/// error, so a listing cut short decides nothing.
pub fn parse_index_listing(bytes: &[u8]) -> Result<Vec<IndexEntry>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let body = bytes
        .strip_suffix(b"\0")
        .ok_or("ls-files output does not end in NUL")?;
    body.split(|&b| b == 0)
        .map(|field| match field {
            [tag, b' ', path @ ..] if LS_FILES_TAGS.contains(tag) && !path.is_empty() => {
                Ok(IndexEntry {
                    tag: *tag,
                    path: path_from_bytes(path)?,
                })
            }
            _ => Err(format!(
                "unexpected ls-files entry {:?}",
                String::from_utf8_lossy(field)
            )),
        })
        .collect()
}

/// The rule `git check-ignore -v` names for a path: the file it comes from as git names it,
/// its line, and the pattern as written, a negation starting with `!`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IgnoreRule {
    pub source: PathBuf,
    pub line: u64,
    pub pattern: Vec<u8>,
}

impl IgnoreRule {
    /// Whether the rule re-includes what it matches.
    #[must_use]
    pub fn negation(&self) -> bool {
        self.pattern.first() == Some(&b'!')
    }
}

/// Parses `git -c core.quotePath=false check-ignore -v -- <path>` output for the one path
/// `path`: `<source>:<line>:<pattern> TAB <path> LF` when a rule matches, nothing when none
/// does. (`-z` would need `--stdin`, and git runs with stdin closed.) Output for another path
/// or a path git quoted, a source git quoted, a line that is not a number, and a line that
/// splits into source, line, and pattern in more than one way (a source or pattern holding
/// `:<digits>:`) are an error, so a rule is never read from the wrong field.
pub fn parse_check_ignore(bytes: &[u8], path: &Path) -> Result<Option<IgnoreRule>, String> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let named = path.as_os_str().as_encoded_bytes();
    let head = bytes
        .strip_suffix(b"\n")
        .and_then(|rest| rest.strip_suffix(named))
        .and_then(|rest| rest.strip_suffix(b"\t"))
        .ok_or_else(|| {
            format!(
                "check-ignore did not name the path asked about: {:?}",
                String::from_utf8_lossy(bytes)
            )
        })?;
    if head.first() == Some(&b'"') || head.contains(&b'\n') {
        return Err("check-ignore printed a source it quoted, or more than one line".into());
    }
    let colons: Vec<usize> = head
        .iter()
        .enumerate()
        .filter_map(|(i, &b)| (b == b':').then_some(i))
        .collect();
    let splits: Vec<(usize, usize)> = colons
        .windows(2)
        .map(|pair| (pair[0], pair[1]))
        .filter(|&(i, j)| j > i + 1 && head[i + 1..j].iter().all(u8::is_ascii_digit))
        .collect();
    let [(i, j)] = splits[..] else {
        return Err(format!(
            "check-ignore printed a rule that does not split into source, line, and pattern \
             one way: {:?}",
            String::from_utf8_lossy(head)
        ));
    };
    let (source, pattern) = (&head[..i], &head[j + 1..]);
    let line = std::str::from_utf8(&head[i + 1..j])
        .ok()
        .and_then(|line| line.parse().ok())
        .ok_or("check-ignore printed a line that is not a number")?;
    if source.is_empty() || pattern.is_empty() {
        return Err("check-ignore printed a rule without its source or pattern".into());
    }
    Ok(Some(IgnoreRule {
        source: path_from_bytes(source)?,
        line,
        pattern: pattern.to_vec(),
    }))
}

/// The configuration keys whose values decide git's classification of ignored files (section
/// 9.7), as `git config --get-regexp` names them, lowercase.
pub const IGNORE_KEYS: [&str; 4] = [
    "core.excludesfile",
    "core.ignorecase",
    "core.precomposeunicode",
    "core.splitindex",
];

/// The values git resolves in a worktree for [`IGNORE_KEYS`], by key: the last one git reads
/// of each key it lists, `None` for a key without a value (a true boolean). A key git does not
/// list is unset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IgnoreValues {
    pub values: BTreeMap<String, Option<Vec<u8>>>,
}

impl IgnoreValues {
    /// The value of `key`: `None` when unset, `Some(None)` when set without a value.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Option<&[u8]>> {
        self.values.get(key).map(Option::as_deref)
    }

    /// Whether a boolean `key` is set to anything but false (`false`, `no`, `off`, `0`, or an
    /// empty value). A value git would not read as false counts as true.
    #[must_use]
    pub fn truthy(&self, key: &str) -> bool {
        match self.get(key) {
            None => false,
            Some(None) => true,
            Some(Some(value)) => !matches!(
                value.to_ascii_lowercase().as_slice(),
                b"false" | b"no" | b"off" | b"0" | b""
            ),
        }
    }
}

/// Parses `git config --null --type=path --get-regexp` output for [`IGNORE_KEYS`]: `<key> LF
/// <value> NUL`, or `<key> NUL` for a key without a value, the last of a key's values being
/// the one git uses. Output that does not end in NUL, or names another key, is an error.
pub fn parse_ignore_values(bytes: &[u8]) -> Result<IgnoreValues, String> {
    let mut values = BTreeMap::new();
    if bytes.is_empty() {
        return Ok(IgnoreValues { values });
    }
    let body = bytes
        .strip_suffix(b"\0")
        .ok_or("git config output does not end in NUL")?;
    for entry in body.split(|&b| b == 0) {
        let (key, value) = match entry.iter().position(|&b| b == b'\n') {
            Some(at) => (&entry[..at], Some(entry[at + 1..].to_vec())),
            None => (entry, None),
        };
        let key = std::str::from_utf8(key)
            .ok()
            .filter(|key| IGNORE_KEYS.contains(key))
            .ok_or_else(|| {
                format!(
                    "git config listed an unexpected key {:?}",
                    String::from_utf8_lossy(key)
                )
            })?;
        values.insert(key.to_owned(), value);
    }
    Ok(IgnoreValues { values })
}

/// Parses `git rev-parse --git-path <name>`: one path and a newline.
pub fn parse_git_path(bytes: &[u8]) -> Result<PathBuf, String> {
    match bytes.strip_suffix(b"\n") {
        Some(path) if !path.is_empty() && !path.contains(&b'\n') => path_from_bytes(path),
        _ => Err("rev-parse --git-path printed something other than one path".into()),
    }
}

fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
    path_ref_from_bytes(bytes).map(Path::to_path_buf)
}

#[cfg(unix)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the same signature as on Windows, where a path from git can fail to decode"
)]
fn path_ref_from_bytes(bytes: &[u8]) -> Result<&Path, String> {
    use std::os::unix::ffi::OsStrExt;
    Ok(Path::new(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn path_ref_from_bytes(bytes: &[u8]) -> Result<&Path, String> {
    std::str::from_utf8(bytes)
        .map(Path::new)
        .map_err(|_| "a path from git is not UTF-8".to_owned())
}

/// Whether the index file `bytes` is a split index: it carries git's `link` extension, which
/// names the shared index its entries continue (`gitformat-index(5)`). The file is read as
/// bytes, never through git, which refreshes the shared file's mtime on each read of a split
/// index. Entries are walked for versions 2 to 4 with SHA-1 or SHA-256 object names, and the
/// extensions must end exactly at the trailing checksum; an index that does not parse, or
/// parses both ways with different answers, is an error.
pub fn index_is_split(bytes: &[u8]) -> Result<bool, String> {
    let bad = |why: &str| format!("the index does not parse: {why}");
    if bytes.get(..4) != Some(b"DIRC") {
        return Err(bad("no DIRC signature"));
    }
    let word = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let (Some(version), Some(count)) = (word(4), word(8)) else {
        return Err(bad("the header is cut short"));
    };
    if !(2..=4).contains(&version) {
        return Err(bad(&format!("version {version}")));
    }
    let answers: Vec<bool> = [20, 32]
        .into_iter()
        .filter_map(|hash| index_link(bytes, version, count, hash))
        .collect();
    match answers[..] {
        [split] => Ok(split),
        [a, b] if a == b => Ok(a),
        [] => Err(bad("its entries and extensions do not end at its checksum")),
        _ => Err(bad("it reads both as SHA-1 and SHA-256")),
    }
}

/// [`index_is_split`] for object names of `hash` bytes: whether the `link` extension is
/// present, or `None` when the entries and extensions do not end exactly at the checksum.
fn index_link(bytes: &[u8], version: u32, count: u32, hash: usize) -> Option<bool> {
    let end = bytes.len().checked_sub(hash)?;
    let mut at = 12;
    // ctime, mtime, dev, ino, mode, uid, gid, size, the object name, and the flags.
    let fixed = 40 + hash + 2;
    for _ in 0..count {
        let start = at;
        let flags = u16::from_be_bytes([*bytes.get(at + fixed - 2)?, *bytes.get(at + fixed - 1)?]);
        at += fixed;
        if version >= 3 && flags & 0x4000 != 0 {
            at += 2;
        }
        if version == 4 {
            // The prefix length to strip, as git's varint, then the rest of the name.
            while *bytes.get(at)? & 0x80 != 0 {
                at += 1;
            }
            at += 1;
            at += bytes.get(at..end)?.iter().position(|&b| b == 0)? + 1;
        } else {
            let name = bytes.get(at..end)?.iter().position(|&b| b == 0)?;
            // One to eight NULs pad the entry to a multiple of eight bytes.
            at = start + (at - start + name + 8) / 8 * 8;
        }
        if at > end {
            return None;
        }
    }
    let mut split = false;
    while at < end {
        let signature = bytes.get(at..at + 4)?;
        let size = u32::from_be_bytes(bytes.get(at + 4..at + 8)?.try_into().ok()?);
        split |= signature == b"link";
        at = at.checked_add(8 + usize::try_from(size).ok()?)?;
    }
    (at == end).then_some(split)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    /// An index file of `version` with one entry named `name`, the extensions `extensions`,
    /// and a checksum, with SHA-1 object names.
    fn index(version: u32, name: &str, extensions: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut bytes = b"DIRC".to_vec();
        bytes.extend(version.to_be_bytes());
        bytes.extend(1_u32.to_be_bytes());
        let start = bytes.len();
        bytes.extend([0; 40 + 20]);
        bytes.extend(u16::try_from(name.len()).unwrap().to_be_bytes());
        if version == 4 {
            bytes.push(0);
        }
        bytes.extend(name.as_bytes());
        bytes.push(0);
        if version < 4 {
            while !(bytes.len() - start).is_multiple_of(8) {
                bytes.push(0);
            }
        }
        for (signature, data) in extensions {
            bytes.extend(*signature);
            bytes.extend(u32::try_from(data.len()).unwrap().to_be_bytes());
            bytes.extend(*data);
        }
        bytes.extend([7; 20]);
        bytes
    }

    /// `origin`'s URL is one non-empty line of UTF-8 with its line feed; no `origin` is
    /// exit 2 with git's one line for it; anything else is no answer, and no error quotes
    /// the output.
    #[test]
    fn a_remote_answer_is_a_url_or_git_saying_none() {
        let answer = super::remote_answer;
        let url = answer(Some(0), b"git@h:o/r.git\n", b"");
        assert_eq!(url, Ok(Some("git@h:o/r.git".to_owned())));
        assert_eq!(answer(Some(2), b"", super::NO_ORIGIN), Ok(None));
        // Built in parts, so no scanner takes the test's URL for a real credential.
        let secret = [&b"https://u:"[..], b"s3cret", b"@h/o/r.git"].concat();
        let secret = secret.as_slice();
        let failed: [(i32, &[u8], &[u8]); 9] = [
            (0, b"\n", b""),
            (0, b"a\nb\n", b""),
            (0, b"\xff\n", b""),
            (0, secret, b""),
            (
                0,
                b"git@h:o/r.git\n",
                b"warning: reading remote from \"remotes/origin\"\n",
            ),
            (
                2,
                b"",
                b"warning: unable to access '.git/remotes/origin': Permission denied\n\
                  error: No such remote 'origin'\n",
            ),
            (2, b"", b"error: No such remote 'origin'"),
            (2, secret, super::NO_ORIGIN),
            (128, b"", b"fatal: bad config\n"),
        ];
        for (code, stdout, stderr) in failed {
            let err = answer(Some(code), stdout, stderr).unwrap_err();
            assert!(!err.contains("s3cret"), "{err}");
        }
        assert!(answer(None, b"", b"").is_err());
    }

    /// The `link` extension, and only it, makes an index split; a leftover shared file does
    /// not. An index cut short or with trailing bytes decides nothing.
    #[test]
    fn a_split_index_is_its_link_extension() {
        let link: (&[u8; 4], &[u8]) = (b"link", &[1; 20]);
        let tree: (&[u8; 4], &[u8]) = (b"TREE", b"\x00-1 0\n");
        for version in [2, 3, 4] {
            for name in ["a", "src/main.rs", "seven77"] {
                let plain = index(version, name, &[tree]);
                assert_eq!(super::index_is_split(&plain), Ok(false), "{version} {name}");
                let split = index(version, name, &[tree, link]);
                assert_eq!(super::index_is_split(&split), Ok(true), "{version} {name}");
                assert!(super::index_is_split(&split[..split.len() - 1]).is_err());
                let mut longer = split.clone();
                longer.push(0);
                assert!(super::index_is_split(&longer).is_err());
            }
        }
        assert!(super::index_is_split(b"DIRC").is_err());
        assert!(super::index_is_split(&index(5, "a", &[])).is_err());
    }

    use super::{ListedWorktree, is_oid, is_ref_name, parse_status, parse_worktree_list};

    const A: &str = "1111111111111111111111111111111111111111";

    #[test]
    fn worktree_list() {
        let text = format!(
            "worktree /r\0HEAD {A}\0branch refs/heads/main\0\0\
             worktree /w/one\0HEAD {A}\0detached\0locked reason here\0\0\
             worktree /w/two\0HEAD {A}\0branch refs/heads/feat/x\0prunable gitdir file points to non-existent location\0\0"
        );
        let list = parse_worktree_list(text.as_bytes()).unwrap();
        assert_eq!(
            list,
            [
                ListedWorktree {
                    path: "/r".into(),
                    head: Some(A.into()),
                    branch: Some("main".into()),
                    ..ListedWorktree::default()
                },
                ListedWorktree {
                    path: "/w/one".into(),
                    head: Some(A.into()),
                    locked: true,
                    ..ListedWorktree::default()
                },
                ListedWorktree {
                    path: "/w/two".into(),
                    head: Some(A.into()),
                    branch: Some("feat/x".into()),
                    ..ListedWorktree::default()
                },
            ]
        );
        assert!(parse_worktree_list(b"").is_err());
        assert!(parse_worktree_list(b"HEAD abc\0\0").is_err());
        let bare = parse_worktree_list(b"worktree /r.git\0bare\0\0").unwrap();
        assert!(bare[0].bare);
    }

    #[test]
    fn status_headers_entries_and_ignored() {
        let text = format!(
            "# branch.oid {A}\0# branch.head feat\0# branch.upstream origin/feat\0# branch.ab +0 -0\0\
             ! target/\0! .DS_Store\0"
        );
        let status = parse_status(text.as_bytes()).unwrap();
        assert_eq!(status.head.as_deref(), Some(A));
        assert_eq!(status.branch.as_deref(), Some("feat"));
        assert_eq!(status.upstream.as_deref(), Some("origin/feat"));
        assert!(!status.other_changes && status.modified.is_empty());
        assert_eq!(
            status.ignored,
            [PathBuf::from("target"), PathBuf::from(".DS_Store")]
        );

        let dirty = format!(
            "# branch.oid {A}\0# branch.head (detached)\0\
             2 R. N... 100644 100644 100644 {A} {A} R100 new\0old\0"
        );
        let status = parse_status(dirty.as_bytes()).unwrap();
        assert!(status.other_changes);
        assert_eq!(status.branch, None);
        assert_eq!(status.upstream, None);
        assert!(
            status.ignored.is_empty(),
            "the original path is not an entry"
        );

        let untracked = format!("# branch.oid {A}\0# branch.head main\0? new.txt\0");
        assert!(parse_status(untracked.as_bytes()).unwrap().other_changes);

        let initial = "# branch.oid (initial)\0# branch.head main\0";
        assert_eq!(parse_status(initial.as_bytes()).unwrap().head, None);

        assert!(parse_status(b"").is_err(), "no branch header");
        assert!(parse_status(b"# branch.oid x\0garbage\0").is_err());
    }

    /// Section 9.8: output cut before its last terminator is incomplete, never a status; the
    /// same output whole parses (control).
    #[test]
    fn torn_status_is_incomplete() {
        assert!(parse_status(b"# branch.oid (initial)").is_err());
        assert!(parse_status(b"# branch.oid (initial)\0# branch.head main\0? new.tx").is_err());
        let whole = parse_status(b"# branch.oid (initial)\0# branch.head main\0? new.txt\0");
        assert_eq!(whole.unwrap().changed.len(), 1);
        // A rename cut after its new path's NUL, its original path missing, is incomplete
        // too; with the original path it parses (control).
        let rename = format!(
            "# branch.oid {A}\0# branch.head main\0\
             2 R. N... 100644 100644 100644 {A} {A} R100 new.rs\0"
        );
        assert!(parse_status(rename.as_bytes()).is_err());
        let whole = format!("{rename}old.rs\0");
        assert_eq!(parse_status(whole.as_bytes()).unwrap().changed.len(), 1);
    }

    /// Section 9.8: observation keeps every path status lists as changed, unmerged, or
    /// untracked, a rename by its new path, each tracked deletion marked, and no ignored one.
    #[test]
    fn status_keeps_the_changed_paths() {
        let text = format!(
            "# branch.oid {A}\0# branch.head main\0\
             1 .M N... 100644 100644 100644 {A} {A} src/a file.rs\0\
             1 .D N... 100644 100644 000000 {A} {A} gone.rs\0\
             1 D. N... 100644 000000 000000 {A} {A} staged gone.rs\0\
             2 R. N... 100644 100644 100644 {A} {A} R100 new name.rs\0old.rs\0\
             u UU N... 100644 100644 100644 100644 {A} {A} {A} both changed.rs\0\
             ? new dir/untracked.txt\0! target/\0"
        );
        let status = parse_status(text.as_bytes()).unwrap();
        let changed: Vec<(&str, bool)> = status
            .changed
            .iter()
            .map(|p| (p.path.to_str().unwrap(), p.deleted))
            .collect();
        assert_eq!(
            changed,
            [
                ("src/a file.rs", false),
                ("gone.rs", true),
                ("staged gone.rs", true),
                ("new name.rs", false),
                ("both changed.rs", false),
                ("new dir/untracked.txt", false),
            ]
        );
    }

    #[test]
    fn worktree_modifications_are_kept_apart() {
        let text = format!(
            "# branch.oid {A}\0# branch.head main\0\
             1 .M N... 100644 100644 100644 {A} {A} assets/big file.bin\0\
             1 .M N... 100644 100644 100644 {A} {A} b.bin\0"
        );
        let status = parse_status(text.as_bytes()).unwrap();
        assert!(!status.other_changes);
        assert_eq!(
            status.modified,
            [PathBuf::from("assets/big file.bin"), PathBuf::from("b.bin")]
        );
        for other in [
            format!("1 M. N... 100644 100644 100644 {A} {A} staged.txt"),
            format!("1 MM N... 100644 100644 100644 {A} {A} both.txt"),
            format!("1 .D N... 100644 100644 000000 {A} {A} gone.txt"),
            "? new.txt".to_owned(),
        ] {
            let text = format!("# branch.oid {A}\0# branch.head main\0{other}\0");
            let status = parse_status(text.as_bytes()).unwrap();
            assert!(status.other_changes, "{other}");
            assert!(status.modified.is_empty(), "{other}");
        }
    }

    #[test]
    fn filter_drivers_and_filtered_paths() {
        use super::{parse_filter_drivers, parse_filtered};
        let drivers = parse_filter_drivers(
            b"filter.lfs.clean\0filter.lfs.smudge\0filter.lfs.required\0filter.a.b.process\0",
        )
        .unwrap();
        assert_eq!(
            drivers.iter().map(String::as_str).collect::<Vec<_>>(),
            ["a.b", "lfs"]
        );
        assert!(parse_filter_drivers(b"").unwrap().is_empty());
        assert!(parse_filter_drivers(b"filter.lfs\0").is_err());

        let out = b"big.bin\0filter\0lfs\0plain.txt\0filter\0unspecified\0\
                    odd.bin\0filter\0none\0set.bin\0filter\0set\0";
        let filtered = parse_filtered(out, &drivers).unwrap();
        assert_eq!(
            filtered.into_iter().collect::<Vec<_>>(),
            [PathBuf::from("big.bin")]
        );
        assert!(parse_filtered(b"", &drivers).unwrap().is_empty());
        assert!(parse_filtered(b"a\0filter\0", &drivers).is_err());

        // `set` is a bare attribute or a driver named `set`: with such a driver, it counts.
        let set = std::collections::BTreeSet::from(["set".to_owned()]);
        assert_eq!(
            parse_filtered(b"a.bin\0filter\0set\0b.bin\0filter\0unset\0", &set)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            [PathBuf::from("a.bin")]
        );
        assert!(parse_filtered(b"a\0diff\0lfs\0", &drivers).is_err());
    }

    #[test]
    fn versions_and_promisors() {
        use super::{parse_promisor, parse_version};
        assert_eq!(parse_version(b"git version 2.54.0\n"), Some((2, 54, 0)));
        assert_eq!(
            parse_version(b"git version 2.39.5 (Apple Git-154)\n"),
            Some((2, 39, 5))
        );
        assert_eq!(
            parse_version(b"git version 2.45.1.windows.1\n"),
            Some((2, 45, 1))
        );
        assert_eq!(parse_version(b"git version 2.45\n"), Some((2, 45, 0)));
        assert_eq!(parse_version(b"git version 2.45.0-rc1\n"), Some((2, 45, 0)));
        assert_eq!(parse_version(b"hub version 2.14\n"), None);

        assert!(!parse_promisor(b""));
        assert!(parse_promisor(b"extensions.partialclone\norigin\0"));
        assert!(parse_promisor(b"remote.origin.promisor\ntrue\0"));
        assert!(parse_promisor(b"remote.origin.promisor\0"));
        assert!(parse_promisor(b"remote.origin.promisor\nmaybe\0"));
        assert!(!parse_promisor(
            b"remote.origin.promisor\nfalse\0remote.b.promisor\nOff\0"
        ));
        assert!(parse_promisor(
            b"remote.origin.promisor\nfalse\0remote.origin.partialclonefilter\ntree:0\0"
        ));
        assert!(parse_promisor(b"extensions.partialclone\n\0"));
    }

    #[test]
    fn ref_names() {
        use super::is_ref_name;
        for good in [
            "main",
            "origin/main",
            "refs/remotes/origin/main",
            "feat/x-1",
        ] {
            assert!(is_ref_name(good), "{good}");
        }
        for bad in [
            ":missing", "main^", "main~1", "@{-1}", "main@{1}", "a..b", "-x", "@", "a b", "a\tb",
            "a/", "a.", "a//b", ".a", "a/.b", "x.lock", "a:b", "a?", "a*", "a[", "a\\b", "",
        ] {
            assert!(!is_ref_name(bad), "{bad:?}");
        }
    }

    /// The listing of the index keeps each entry's tag and stored spelling, a sparse directory
    /// included, and output cut short or with an unknown tag is an error.
    #[test]
    fn index_listings() {
        use super::parse_index_listing;
        let entries = parse_index_listing(b"H src/a.rs\0S .gitignore\0S dir/\0H a b\0").unwrap();
        let tags: Vec<(char, &str)> = entries
            .iter()
            .map(|e| (char::from(e.tag), e.path.to_str().unwrap()))
            .collect();
        assert_eq!(
            tags,
            [
                ('H', "src/a.rs"),
                ('S', ".gitignore"),
                ('S', "dir/"),
                ('H', "a b")
            ]
        );
        assert_eq!(
            entries
                .iter()
                .map(super::IndexEntry::sparse)
                .collect::<Vec<_>>(),
            [false, true, true, false]
        );
        assert_eq!(parse_index_listing(b"").unwrap(), []);
        for bad in [
            &b"H src/a.rs"[..],
            b"H src/a.rs\0H b",
            b"src/a.rs\0",
            b"h a\0",
            b"H \0",
            b"\0",
        ] {
            assert!(parse_index_listing(bad).is_err(), "{bad:?}");
        }
    }

    /// check-ignore names the rule's source, line, and pattern for the path asked about; a
    /// negation is told by its `!`, and output for another path or cut short is an error.
    #[test]
    fn check_ignore_rules() {
        use super::parse_check_ignore;
        use std::path::Path;
        let rule = parse_check_ignore(b".gitignore:3:/target/\ttarget\n", Path::new("target"))
            .unwrap()
            .unwrap();
        assert_eq!(rule.source, Path::new(".gitignore"));
        assert_eq!(rule.line, 3);
        assert_eq!(rule.pattern, b"/target/");
        assert!(!rule.negation());
        let negation = parse_check_ignore(b"a/.gitignore:1:!target\ttarget\n", Path::new("target"))
            .unwrap()
            .unwrap();
        assert!(negation.negation());
        // A colon in the source or the pattern, with one split that has a number between.
        let colons = parse_check_ignore(
            b"/h/a:b/ignore:12:x:y\tsub/target\n",
            Path::new("sub/target"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(colons.source, Path::new("/h/a:b/ignore"));
        assert_eq!((colons.line, &colons.pattern[..]), (12, &b"x:y"[..]));
        assert_eq!(parse_check_ignore(b"", Path::new("target")).unwrap(), None);
        for bad in [
            &b".gitignore:3:/target/\tother\n"[..],
            b".gitignore:3:/target/\ttarget",
            b".gitignore:x:/target/\ttarget\n",
            b":3:/target/\ttarget\n",
            b".gitignore:3:\ttarget\n",
            b".gitignore:3:/target/\ttarget\nextra\ttarget\n",
            b"\"a\\tb\":3:/target/\ttarget\n",
            b".gitignore:3:a:5:b\ttarget\n",
            b".gitignore:3:/target/\t\"target\"\n",
        ] {
            assert!(
                parse_check_ignore(bad, Path::new("target")).is_err(),
                "{bad:?}"
            );
        }
    }

    /// The values git resolves: the last of each key, a key without a value as true, and an
    /// unset key as unset; another key or output cut short is an error.
    #[test]
    fn ignore_values() {
        use super::parse_ignore_values;
        let values = parse_ignore_values(
            b"core.ignorecase\nfalse\0core.excludesfile\n/h/ignore\0core.ignorecase\ntrue\0core.splitindex\0",
        )
        .unwrap();
        assert_eq!(values.get("core.ignorecase"), Some(Some(&b"true"[..])));
        assert_eq!(
            values.get("core.excludesfile"),
            Some(Some(&b"/h/ignore"[..]))
        );
        assert_eq!(values.get("core.precomposeunicode"), None);
        assert!(values.truthy("core.splitindex"), "set without a value");
        assert!(values.truthy("core.ignorecase"));
        assert!(!values.truthy("core.precomposeunicode"));
        for (value, truthy) in [("Off", false), ("0", false), ("", false), ("maybe", true)] {
            let text = format!("core.splitindex\n{value}\0");
            assert_eq!(
                parse_ignore_values(text.as_bytes())
                    .unwrap()
                    .truthy("core.splitindex"),
                truthy,
                "{value}"
            );
        }
        assert_eq!(
            parse_ignore_values(b"").unwrap(),
            super::IgnoreValues::default()
        );
        assert!(parse_ignore_values(b"core.ignorecase\ntrue").is_err());
        assert!(parse_ignore_values(b"core.editor\nvi\0").is_err());
    }

    #[test]
    fn git_paths() {
        use super::parse_git_path;
        assert_eq!(
            parse_git_path(b"/r/.git/index\n").unwrap(),
            PathBuf::from("/r/.git/index")
        );
        for bad in [&b""[..], b"\n", b"/r/.git/index", b"a\nb\n"] {
            assert!(parse_git_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn object_ids() {
        assert!(is_oid(A));
        assert!(!is_oid("main"));
        assert!(!is_oid(&A[1..]));
    }

    /// Where a configured base is held stricter than `git check-ref-format`, which accepts all
    /// of the refused names below: a component ending in `.lock` in any case is refused, since
    /// on a case-insensitive file system `main.LOCK` is the lock file git takes for `main`, and
    /// so is any Unicode control character, where git refuses only ASCII ones.
    #[test]
    fn ref_names_are_stricter_than_git() {
        for name in [
            "main.LOCK",
            "a/b.Lock",
            "release.lOcK/x",
            "a\u{85}b",
            "a\u{9f}b",
        ] {
            assert!(!is_ref_name(name), "{name:?} is not a ref name");
        }
        for name in ["main.locked", "lock", "a/lock.b", "a\u{a0}b"] {
            assert!(is_ref_name(name), "{name:?} is a ref name");
        }
    }
}
