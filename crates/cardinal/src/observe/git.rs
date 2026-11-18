//! Parsers for the git commands of `docs/architecture.md` section 8.2. Every parser refuses
//! output it does not understand, so the facts built from it become `unknown`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

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
            b'1' => {
                // `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`: the path is the rest.
                let mut parts = rest.splitn(8, |&b| b == b' ');
                let xy = parts.next().unwrap_or_default();
                let path = parts.nth(6).ok_or("changed entry without a path")?;
                if xy == b".M" {
                    status.modified.push(path_from_bytes(path)?);
                } else {
                    status.other_changes = true;
                }
            }
            b'u' | b'?' => {
                status.other_changes = true;
            }
            b'2' => {
                status.other_changes = true;
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

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
