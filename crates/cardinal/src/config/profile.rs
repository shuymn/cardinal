//! `repos/*.toml` profiles and their precedence (`docs/architecture.md` section 3.3).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::ConfigError;
use super::host::{absolute_path, check_resources};
use crate::contract::Resources;

/// One profile file. Profiles are kept in file-name order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// The file stem; rule IDs in records are `<name>/<id>`.
    pub name: String,
    pub file: PathBuf,
    /// `None` for a generic profile, which applies to every repository.
    pub matcher: Option<Matcher>,
    /// `[git] base`
    pub base: Option<String>,
    /// `[classify] wrappers`, `None` when the profile does not set them.
    pub wrappers: Option<Vec<Vec<String>>>,
    /// `[classify] skip`
    pub skip: Vec<String>,
    pub rules: Vec<Rule>,
    /// `[data] regenerable`
    pub regenerable: Vec<String>,
    /// `[data] protected`
    pub protected: Vec<String>,
}

/// `[match]`: main worktree paths (matched through the git common dir) and normalized remotes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Matcher {
    pub paths: Vec<PathBuf>,
    /// `host/owner/name`, lowercase.
    pub remotes: Vec<String>,
}

/// A `[[rule]]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub id: String,
    /// `<profile>/<id>`
    pub qualified_id: String,
    pub patterns: Vec<Pattern>,
    /// Any of these words in the argv cancels a match.
    pub except: Vec<String>,
    pub class: String,
    /// `None` means `[admission] default_resources`.
    pub resources: Option<Resources>,
}

/// A command pattern: a prefix of the normalized argv; the word `*` matches any one word.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    pub text: String,
    pub words: Vec<String>,
}

impl Profile {
    pub(super) fn parse(path: &Path, text: &str, home: Option<&Path>) -> Result<Self, ConfigError> {
        let raw: RawProfile = toml::from_str(text).map_err(|err| ConfigError::Parse {
            path: path.to_path_buf(),
            message: err.to_string(),
        })?;
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| ConfigError::invalid(path, "the file name is not UTF-8"))?
            .to_owned();
        raw.resolve(path, name, home)
    }

    /// Whether this profile applies to every repository.
    #[must_use]
    pub fn is_generic(&self) -> bool {
        self.matcher.is_none()
    }

    /// Whether one of `match.paths` is a worktree whose git common dir is `common_dir`.
    /// `common_dir` must be canonical. Needs no child process.
    #[must_use]
    pub fn matches_common_dir(&self, common_dir: &Path) -> bool {
        self.matcher.as_ref().is_some_and(|matcher| {
            matcher
                .paths
                .iter()
                .filter_map(|path| git_common_dir(path))
                .any(|dir| dir == common_dir)
        })
    }

    /// Whether one of `match.remotes` equals `remote`, a URL or an already normalized remote.
    #[must_use]
    pub fn matches_remote(&self, remote: &str) -> bool {
        let Some(remote) = normalize_remote(remote) else {
            return false;
        };
        self.matcher
            .as_ref()
            .is_some_and(|matcher| matcher.remotes.contains(&remote))
    }
}

/// The profiles that apply to one repository, in precedence order: matching profiles in
/// file-name order, then generic profiles in file-name order.
#[derive(Clone, Debug, Default)]
pub struct Selection<'a> {
    pub profiles: Vec<&'a Profile>,
}

impl<'a> Selection<'a> {
    /// Selects from `profiles` (in file-name order); `matches` decides each non-generic one.
    pub fn new(profiles: &'a [Profile], mut matches: impl FnMut(&Profile) -> bool) -> Self {
        let mut selected: Vec<&Profile> = profiles
            .iter()
            .filter(|profile| !profile.is_generic() && matches(profile))
            .collect();
        selected.extend(profiles.iter().filter(|profile| profile.is_generic()));
        Self { profiles: selected }
    }

    /// Profile names in precedence order.
    #[must_use]
    pub fn names(&self) -> Vec<&'a str> {
        self.profiles.iter().map(|p| p.name.as_str()).collect()
    }

    /// Every rule in precedence order; the first that matches a stage wins.
    pub fn rules(&self) -> impl Iterator<Item = (&'a Profile, &'a Rule)> + '_ {
        self.profiles
            .iter()
            .flat_map(|profile| profile.rules.iter().map(move |rule| (*profile, rule)))
    }

    /// `[git] base` of the first profile that sets it.
    #[must_use]
    pub fn base(&self) -> Option<&'a str> {
        self.profiles.iter().find_map(|p| p.base.as_deref())
    }

    /// `[classify] wrappers` of the first profile that sets them, else none.
    #[must_use]
    pub fn wrappers(&self) -> &'a [Vec<String>] {
        self.profiles
            .iter()
            .find_map(|p| p.wrappers.as_deref())
            .unwrap_or(&[])
    }

    /// The union of `[classify] skip`.
    #[must_use]
    pub fn skip(&self) -> BTreeSet<&'a str> {
        self.union(|p| &p.skip)
    }

    /// The union of `[data] regenerable`.
    #[must_use]
    pub fn regenerable(&self) -> BTreeSet<&'a str> {
        self.union(|p| &p.regenerable)
    }

    /// The union of `[data] protected`.
    #[must_use]
    pub fn protected(&self) -> BTreeSet<&'a str> {
        self.union(|p| &p.protected)
    }

    fn union(&self, field: impl Fn(&'a Profile) -> &'a Vec<String>) -> BTreeSet<&'a str> {
        self.profiles
            .iter()
            .flat_map(|p| field(p).iter().map(String::as_str))
            .collect()
    }
}

/// The git common dir of the worktree rooted at `worktree`, read from its `.git` entry
/// without running git: the `.git` directory itself, or for a `.git` file the `gitdir:` it
/// names, followed by that directory's `commondir` file when present. Canonical, or `None`.
#[must_use]
pub fn git_common_dir(worktree: &Path) -> Option<PathBuf> {
    let dot_git = worktree.join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return dunce::canonicalize(&dot_git).ok();
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = text.strip_prefix("gitdir:")?.trim();
    let gitdir = worktree.join(gitdir);
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(rel) => gitdir.join(rel.trim()),
        Err(_) => gitdir,
    };
    dunce::canonicalize(common).ok()
}

/// Normalizes a remote URL to `host/owner/name` in lowercase: `https://`, `ssh://`, `git://`
/// URLs (with optional user and port) and scp-like `user@host:owner/name`, with or without
/// `.git`. Returns `None` for anything else, such as local paths.
#[must_use]
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(
            scheme,
            "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git"
        ) {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?;
        let host = host.split(':').next()?;
        (host, path)
    } else {
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') || path.starts_with('/') {
            return None;
        }
        (authority.rsplit('@').next()?, path)
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path.split('/').collect();
    let valid = |s: &str| !s.is_empty() && s != "." && s != "..";
    if host.is_empty() || segments.len() != 2 || !segments.iter().all(|s| valid(s)) {
        return None;
    }
    Some(format!("{host}/{}/{}", segments[0], segments[1]).to_ascii_lowercase())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawProfile {
    #[serde(rename = "match")]
    matcher: Option<RawMatch>,
    git: RawGit,
    classify: RawClassify,
    rule: Vec<RawRule>,
    data: RawData,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawMatch {
    paths: Vec<String>,
    remotes: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawGit {
    base: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawClassify {
    wrappers: Option<Vec<Vec<String>>>,
    skip: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    id: String,
    commands: Vec<String>,
    #[serde(default)]
    except: Vec<String>,
    class: String,
    resources: Option<Resources>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawData {
    regenerable: Vec<String>,
    protected: Vec<String>,
}

impl RawProfile {
    fn resolve(
        self,
        path: &Path,
        name: String,
        home: Option<&Path>,
    ) -> Result<Profile, ConfigError> {
        let invalid = |message: String| ConfigError::invalid(path, message);

        let matcher = match self.matcher {
            None => None,
            Some(raw) => {
                let paths = raw
                    .paths
                    .iter()
                    .map(|p| {
                        absolute_path(p, home).map_err(|m| invalid(format!("match.paths: {m}")))
                    })
                    .collect::<Result<_, _>>()?;
                let remotes = raw
                    .remotes
                    .iter()
                    .map(|r| {
                        normalize_remote(r)
                            .or_else(|| normalize_remote(&format!("https://{r}")))
                            .ok_or_else(|| {
                                invalid(format!("match.remotes: `{r}` is not host/owner/name"))
                            })
                    })
                    .collect::<Result<_, _>>()?;
                Some(Matcher { paths, remotes })
            }
        };

        if let Some(wrappers) = &self.classify.wrappers
            && wrappers
                .iter()
                .any(|w| w.is_empty() || w.iter().any(String::is_empty))
        {
            return Err(invalid(
                "classify.wrappers: a wrapper or word is empty".into(),
            ));
        }
        if self.classify.skip.iter().any(String::is_empty) {
            return Err(invalid("classify.skip: a word is empty".into()));
        }

        let mut ids = BTreeSet::new();
        let mut rules = Vec::with_capacity(self.rule.len());
        for rule in self.rule {
            if rule.id.is_empty() || rule.id.contains('/') || !ids.insert(rule.id.clone()) {
                return Err(invalid(format!(
                    "rule id `{}` is empty, contains `/`, or is repeated",
                    rule.id
                )));
            }
            if rule.commands.is_empty() {
                return Err(invalid(format!("rule `{}` has no commands", rule.id)));
            }
            let patterns = rule
                .commands
                .into_iter()
                .map(|text| {
                    let words: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
                    if words.is_empty() {
                        Err(invalid(format!("rule `{}` has an empty command", rule.id)))
                    } else {
                        Ok(Pattern { text, words })
                    }
                })
                .collect::<Result<_, _>>()?;
            if let Some(resources) = &rule.resources {
                check_resources(resources)
                    .map_err(|m| invalid(format!("rule `{}` resources: {m}", rule.id)))?;
            }
            rules.push(Rule {
                qualified_id: format!("{name}/{}", rule.id),
                id: rule.id,
                patterns,
                except: rule.except,
                class: rule.class,
                resources: rule.resources,
            });
        }

        for pattern in self.data.regenerable.iter().chain(&self.data.protected) {
            check_data_pattern(pattern).map_err(|m| invalid(format!("data: {m}")))?;
        }

        Ok(Profile {
            name,
            file: path.to_path_buf(),
            matcher,
            base: self.git.base,
            wrappers: self.classify.wrappers,
            skip: self.classify.skip,
            rules,
            regenerable: self.data.regenerable,
            protected: self.data.protected,
        })
    }
}

/// A data pattern is a name, or a worktree-relative path with `/`. No globs in phase 0.
fn check_data_pattern(pattern: &str) -> Result<(), String> {
    let bad = pattern.is_empty()
        || pattern.starts_with('/')
        || pattern.contains('\\')
        || pattern.contains(['*', '?', '['])
        || pattern
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..");
    if bad {
        Err(format!(
            "`{pattern}` must be a name or a relative path without globs, `.` or `..`"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{Profile, Selection, git_common_dir, normalize_remote};

    fn parse(name: &str, text: &str) -> Profile {
        Profile::parse(
            Path::new(&format!("{name}.toml")),
            text,
            Some(Path::new("/h")),
        )
        .unwrap()
    }

    #[test]
    fn rules_and_lists_parse() {
        let profile = parse(
            "app",
            r#"
            [match]
            paths = ["~/src/app"]
            remotes = ["github.com/Owner/App"]
            [git]
            base = "origin/main"
            [classify]
            wrappers = [["npx"], ["pnpm", "exec"]]
            skip = ["vitest"]
            [[rule]]
            id = "test"
            commands = ["pnpm test", "cargo  nextest run", "pnpm * test"]
            except = ["--watch"]
            class = "test"
            resources = { cpu = 4, memory_mb = 9000 }
            [data]
            regenerable = ["node_modules", "fuzz/target"]
            protected = ["fuzz/corpus"]
            "#,
        );
        assert!(!profile.is_generic());
        let matcher = profile.matcher.as_ref().unwrap();
        assert_eq!(matcher.paths, vec![PathBuf::from("/h/src/app")]);
        assert_eq!(matcher.remotes, vec!["github.com/owner/app".to_owned()]);
        let rule = &profile.rules[0];
        assert_eq!(rule.qualified_id, "app/test");
        assert_eq!(rule.patterns[1].words, ["cargo", "nextest", "run"]);
        assert_eq!(
            rule.resources.as_ref().unwrap().get("memory_mb"),
            Some(&9000)
        );
        assert!(profile.matches_remote("git@github.com:owner/app.git"));
        assert!(!profile.matches_remote("git@github.com:owner/other.git"));
    }

    #[test]
    fn invalid_profiles_are_rejected() {
        for text in [
            "[[rule]]\nid = \"a\"\ncommands = [\"x\"]\nclass = \"c\"\nenv = { A = \"1\" }",
            "[[rule]]\nid = \"a\"\ncommands = []\nclass = \"c\"",
            "[[rule]]\nid = \"a/b\"\ncommands = [\"x\"]\nclass = \"c\"",
            "[[rule]]\nid = \"a\"\ncommands = [\"x\"]\nclass = \"c\"\n[[rule]]\nid = \"a\"\ncommands = [\"y\"]\nclass = \"c\"",
            "[[rule]]\nid = \"a\"\ncommands = [\"x\"]\nclass = \"c\"\nresources = { cpu = 0 }",
            "[[rule]]\nid = \"a\"\ncommands = [\"  \"]\nclass = \"c\"",
            "[data]\nregenerable = [\"target/*\"]",
            "[data]\nprotected = [\"../x\"]",
            "[data]\nprotected = [\"/abs\"]",
            "[match]\npaths = [\"relative\"]",
            "[match]\nremotes = [\"not a remote\"]",
            "[classify]\nwrappers = [[]]",
            "[unknown]\nx = 1",
        ] {
            assert!(
                Profile::parse(Path::new("p.toml"), text, Some(Path::new("/h"))).is_err(),
                "{text}"
            );
        }
    }

    #[test]
    fn precedence_matching_then_generic_in_file_order() {
        let profiles = vec![
            parse(
                "a-generic",
                "[classify]\nskip = [\"x\"]\n[data]\nprotected = [\"p1\"]",
            ),
            parse(
                "b-app",
                "[match]\nremotes = [\"github.com/o/app\"]\n[git]\nbase = \"origin/dev\"\n[classify]\nwrappers = [[\"npx\"]]\n[data]\nprotected = [\"p2\"]\n[[rule]]\nid = \"t\"\ncommands = [\"vitest\"]\nclass = \"test\"",
            ),
            parse(
                "c-other",
                "[match]\nremotes = [\"github.com/o/other\"]\n[git]\nbase = \"origin/x\"",
            ),
            parse(
                "d-generic",
                "[git]\nbase = \"origin/main\"\n[classify]\nskip = [\"y\"]\nwrappers = [[\"bunx\"]]\n[[rule]]\nid = \"t\"\ncommands = [\"vitest\"]\nclass = \"generic\"",
            ),
        ];
        let selection = Selection::new(&profiles, |p| p.matches_remote("https://github.com/o/app"));
        assert_eq!(selection.names(), ["b-app", "a-generic", "d-generic"]);
        assert_eq!(selection.base(), Some("origin/dev"));
        assert_eq!(selection.wrappers(), [vec!["npx".to_owned()]]);
        assert_eq!(selection.skip().into_iter().collect::<Vec<_>>(), ["x", "y"]);
        assert_eq!(
            selection.protected().into_iter().collect::<Vec<_>>(),
            ["p1", "p2"]
        );
        let first = selection.rules().next().unwrap();
        assert_eq!(first.1.qualified_id, "b-app/t");

        let none = Selection::new(&profiles, |_| false);
        assert_eq!(none.names(), ["a-generic", "d-generic"]);
        assert_eq!(none.base(), Some("origin/main"));
    }

    #[test]
    fn remote_normalization() {
        for url in [
            "https://github.com/Owner/Name.git",
            "https://github.com/owner/name",
            "https://user@github.com/owner/name/",
            "ssh://git@github.com/owner/name.git",
            "ssh://git@github.com:22/owner/name.git",
            "git@github.com:owner/name.git",
            "github.com:owner/name",
        ] {
            assert_eq!(
                normalize_remote(url).as_deref(),
                Some("github.com/owner/name"),
                "{url}"
            );
        }
        for url in [
            "/srv/repo.git",
            "../repo",
            "file:///srv/repo.git",
            "https://github.com/owner",
            "https://github.com/a/b/c",
            "C:/repo",
        ] {
            assert_eq!(normalize_remote(url), None, "{url}");
        }
    }

    #[test]
    fn common_dir_from_dot_git_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        std::fs::write(main.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        let linked = tmp.path().join("wt");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        let expected = dunce::canonicalize(main.join(".git")).unwrap();
        assert_eq!(git_common_dir(&main), Some(expected.clone()));
        assert_eq!(git_common_dir(&linked), Some(expected.clone()));
        assert_eq!(git_common_dir(tmp.path()), None);

        let profile = Profile::parse(
            Path::new("p.toml"),
            &format!("[match]\npaths = [{:?}]", main.display().to_string()),
            None,
        )
        .unwrap();
        assert!(profile.matches_common_dir(&expected));
        assert!(!profile.matches_common_dir(tmp.path()));
    }
}
