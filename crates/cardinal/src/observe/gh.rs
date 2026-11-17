//! GitHub facts through gh (`docs/architecture.md` section 8.2): the repository of a
//! remote, open pull requests, and merged pull requests by head object ID.

use serde::Deserialize;

use crate::config::normalize_remote;

/// `gh pr list` limit for open pull requests; a full page makes the list `unknown`.
pub const OPEN_LIMIT: usize = 500;

/// `gh pr list` limit for merged pull requests found by a head object ID.
pub const MERGED_LIMIT: usize = 20;

/// `owner/name` of a GitHub remote: `github.com`, or `gh_host` when set. `None` for any
/// other remote.
#[must_use]
pub fn github_repo(remote: &str, gh_host: Option<&str>) -> Option<String> {
    let normalized = normalize_remote(remote)?;
    let (host, repo) = normalized.split_once('/')?;
    let gh_host = gh_host.map(|h| h.trim().to_ascii_lowercase());
    (host == "github.com" || gh_host.as_deref() == Some(host)).then(|| repo.to_owned())
}

/// One open pull request.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenPr {
    pub number: u64,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub is_cross_repository: bool,
    pub url: String,
}

/// One merged pull request.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergedPr {
    pub number: u64,
    pub head_ref_oid: String,
    pub merged_at: Option<String>,
    pub url: String,
}

/// The arguments of the open pull request query.
#[must_use]
pub fn open_args(repo: &str) -> Vec<String> {
    [
        "pr",
        "list",
        "--repo",
        repo,
        "--state",
        "open",
        "--limit",
        &OPEN_LIMIT.to_string(),
        "--json",
        "number,headRefName,headRefOid,isCrossRepository,url",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The arguments of the merged pull request query for a head object ID.
#[must_use]
pub fn merged_args(repo: &str, head: &str) -> Vec<String> {
    [
        "pr",
        "list",
        "--repo",
        repo,
        "--state",
        "merged",
        "--search",
        head,
        "--json",
        "number,headRefOid,mergedAt,url",
        "--limit",
        &MERGED_LIMIT.to_string(),
    ]
    .map(str::to_owned)
    .to_vec()
}

/// Parses the open pull request list; a full page cannot be told from a truncated one and
/// is refused.
pub fn parse_open(bytes: &[u8]) -> Result<Vec<OpenPr>, String> {
    let prs: Vec<OpenPr> =
        serde_json::from_slice(bytes).map_err(|err| format!("unexpected gh output: {err}"))?;
    if prs.len() >= OPEN_LIMIT {
        return Err(format!(
            "gh returned {OPEN_LIMIT} open pull requests, the query limit; the list may be incomplete"
        ));
    }
    Ok(prs)
}

/// Parses the merged pull request list.
pub fn parse_merged(bytes: &[u8]) -> Result<Vec<MergedPr>, String> {
    serde_json::from_slice(bytes).map_err(|err| format!("unexpected gh output: {err}"))
}

/// The open pull request of a worktree: a same-repository pull request whose head branch
/// is `branch` or whose head commit is `head`.
#[must_use]
pub fn open_pr_for<'a>(
    prs: &'a [OpenPr],
    branch: Option<&str>,
    head: Option<&str>,
) -> Option<&'a OpenPr> {
    prs.iter().find(|pr| {
        !pr.is_cross_repository
            && (branch == Some(pr.head_ref_name.as_str())
                || head.is_some_and(|h| h.eq_ignore_ascii_case(&pr.head_ref_oid)))
    })
}

/// The merged pull request whose head commit is exactly `head`.
#[must_use]
pub fn merged_pr_for<'a>(prs: &'a [MergedPr], head: &str) -> Option<&'a MergedPr> {
    prs.iter()
        .find(|pr| pr.head_ref_oid.eq_ignore_ascii_case(head))
}

#[cfg(test)]
mod tests {
    use super::{github_repo, merged_pr_for, open_pr_for, parse_merged, parse_open};

    #[test]
    fn github_remotes() {
        assert_eq!(
            github_repo("git@github.com:Owner/App.git", None).as_deref(),
            Some("owner/app")
        );
        assert_eq!(
            github_repo("https://github.com/o/n", None).as_deref(),
            Some("o/n")
        );
        assert_eq!(github_repo("https://gitlab.com/o/n", None), None);
        assert_eq!(
            github_repo("https://ghe.example.com/o/n", Some("GHE.example.com")).as_deref(),
            Some("o/n")
        );
        assert_eq!(github_repo("/local/path", None), None);
    }

    #[test]
    fn open_pull_requests() {
        let prs = parse_open(
            br#"[{"number":1,"headRefName":"feat","headRefOid":"aaa","isCrossRepository":false,"url":"u1"},
                 {"number":2,"headRefName":"fork","headRefOid":"bbb","isCrossRepository":true,"url":"u2"}]"#,
        )
        .unwrap();
        assert_eq!(open_pr_for(&prs, Some("feat"), None).unwrap().number, 1);
        assert_eq!(
            open_pr_for(&prs, Some("other"), Some("AAA"))
                .unwrap()
                .number,
            1
        );
        assert!(
            open_pr_for(&prs, Some("fork"), Some("bbb")).is_none(),
            "a pull request from a fork is not this repository's branch"
        );
        assert!(parse_open(b"{}").is_err());
        assert!(parse_open(b"not json").is_err());
        let full = format!(
            "[{}]",
            vec![r#"{"number":1,"headRefName":"x","headRefOid":"a","isCrossRepository":false,"url":"u"}"#; 500].join(",")
        );
        assert!(
            parse_open(full.as_bytes()).is_err(),
            "a full page is unknown"
        );
    }

    #[test]
    fn merged_pull_requests_match_on_the_head_object_id() {
        let prs = parse_merged(
            br#"[{"number":7,"headRefOid":"abc","mergedAt":"2026-01-01T00:00:00Z","url":"u"}]"#,
        )
        .unwrap();
        assert_eq!(merged_pr_for(&prs, "abc").unwrap().number, 7);
        assert!(merged_pr_for(&prs, "abd").is_none());
    }
}
