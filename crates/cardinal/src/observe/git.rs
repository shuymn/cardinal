//! Parsers for the git commands of `docs/architecture.md` section 8.2. Every parser refuses
//! output it does not understand, so the facts built from it become `unknown`.

use std::path::PathBuf;

/// One entry of `git worktree list --porcelain -z`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListedWorktree {
    pub path: PathBuf,
    pub head: Option<String>,
    /// The short branch name (`refs/heads/` removed); `None` when detached.
    pub branch: Option<String>,
    pub bare: bool,
    pub locked: bool,
    pub prunable: bool,
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
            b"prunable" => entry.prunable = true,
            // Attributes added by later git versions carry no fact cardinal relies on.
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
    /// Any entry other than an ignored one.
    pub dirty: bool,
    /// Worktree-relative paths of ignored entries, without a trailing `/`.
    pub ignored: Vec<PathBuf>,
}

/// Parses porcelain v2 status output with `-z`.
pub fn parse_status(bytes: &[u8]) -> Result<Status, String> {
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
            b'1' | b'u' | b'?' => status.dirty = true,
            b'2' => {
                status.dirty = true;
                // A rename or copy is followed by its original path as a separate field.
                fields
                    .next()
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

/// Splits one `git ls-files -z --stage` entry into `"<mode> <object> <stage>"` and its
/// path.
pub fn parse_stage_entry(entry: &[u8]) -> Result<(String, PathBuf), String> {
    let tab = entry
        .iter()
        .position(|&b| b == b'\t')
        .ok_or("ls-files entry without a path")?;
    let meta = std::str::from_utf8(&entry[..tab]).map_err(|_| "ls-files entry is not UTF-8")?;
    Ok((meta.to_owned(), path_from_bytes(&entry[tab + 1..])?))
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

#[cfg(unix)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the same signature as on Windows, where a path from git can fail to decode"
)]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
    std::str::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|_| "a path from git is not UTF-8".to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{ListedWorktree, is_oid, parse_status, parse_worktree_list};

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
                    prunable: true,
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
        assert!(!status.dirty);
        assert_eq!(
            status.ignored,
            [PathBuf::from("target"), PathBuf::from(".DS_Store")]
        );

        let dirty = format!(
            "# branch.oid {A}\0# branch.head (detached)\0\
             2 R. N... 100644 100644 100644 {A} {A} R100 new\0old\0"
        );
        let status = parse_status(dirty.as_bytes()).unwrap();
        assert!(status.dirty);
        assert_eq!(status.branch, None);
        assert_eq!(status.upstream, None);
        assert!(
            status.ignored.is_empty(),
            "the original path is not an entry"
        );

        let untracked = format!("# branch.oid {A}\0# branch.head main\0? new.txt\0");
        assert!(parse_status(untracked.as_bytes()).unwrap().dirty);

        let initial = "# branch.oid (initial)\0# branch.head main\0";
        assert_eq!(parse_status(initial.as_bytes()).unwrap().head, None);

        assert!(parse_status(b"").is_err(), "no branch header");
        assert!(parse_status(b"# branch.oid x\0garbage\0").is_err());
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

    #[test]
    fn object_ids() {
        assert!(is_oid(A));
        assert!(!is_oid("main"));
        assert!(!is_oid(&A[1..]));
    }
}
