//! An execution's records in `executions/<plan>/` (`docs/architecture.md` section 9.7): the
//! claim and the manifest. Unit X writes and reads them; no released version creates either.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::de::Error as _;
use serde::ser::SerializeMap as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{ActClaimSchema, ActManifestSchema, RootId};

/// The type of an entry an execution records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    Dir,
    File,
    Symlink,
}

/// How far an execution got, which decides what recovery does (section 9.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStage {
    /// Before the commit: recovery puts the target back.
    Claimed,
    /// After the commit: recovery finishes the deletion.
    Committed,
}

/// The worktree an execution works in, with its root's identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimWorktree {
    pub path: String,
    pub root: RootId,
}

/// What selects the repository's profiles (section 3.3), as the execution's observation read
/// it: recovery reads git no more, so it applies these to the configuration it loads to find
/// the patterns its deletion checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRepo {
    /// The repository's git common dir, canonical.
    pub common_dir: String,
    /// `remote.origin.url`, when it is set.
    pub remote: Option<String>,
}

/// A directory between the worktree root and the target, by name and identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainDir {
    pub name: String,
    pub dev: u64,
    pub ino: u64,
}

/// The target as the snapshot read it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimTarget {
    pub name: String,
    pub dev: u64,
    pub ino: u64,
    #[serde(rename = "type")]
    pub kind: EntryType,
}

/// `cardinal.act-claim.v1` (`executions/<plan>/claim`): the stage of an execution and what
/// its recovery needs. It is published complete and replaced at each stage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActClaim {
    pub schema: ActClaimSchema,
    pub plan_id: String,
    pub caller: String,
    pub stage: ClaimStage,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub worktree: ClaimWorktree,
    pub repo: ClaimRepo,
    /// The worktree root's children down to the target's parent.
    pub chain: Vec<ChainDir>,
    pub target: ClaimTarget,
    pub manifest_entries: u64,
}

/// The first line of `cardinal.act-manifest.v1` (`executions/<plan>/manifest`). `entries`
/// lines of [`ManifestEntry`] follow, one per entry of the target, in walk order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActManifest {
    pub schema: ActManifestSchema,
    pub plan_id: String,
    pub entries: u64,
}

/// One entry of the target as the snapshot read it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// The entry's index, which names it in the hold.
    pub i: u64,
    #[serde(flatten)]
    pub path: EntryPath,
    pub dev: u64,
    pub ino: u64,
    #[serde(rename = "type")]
    pub kind: EntryType,
    pub size: u64,
    pub mtime_ns: i64,
    pub nlink: u64,
}

/// An entry's path relative to the target, empty for the target itself: `path` when it is
/// UTF-8, else `path_b64`, the standard base64 of its bytes. A line has exactly one of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryPath {
    Utf8(String),
    Bytes(Vec<u8>),
}

impl Serialize for EntryPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::Utf8(path) => map.serialize_entry("path", path)?,
            Self::Bytes(bytes) => map.serialize_entry("path_b64", &STANDARD.encode(bytes))?,
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for EntryPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Fields {
            path: Option<String>,
            path_b64: Option<String>,
        }
        match Fields::deserialize(deserializer)? {
            Fields {
                path: Some(path),
                path_b64: None,
            } => Ok(Self::Utf8(path)),
            Fields {
                path: None,
                path_b64: Some(encoded),
            } => STANDARD
                .decode(encoded)
                .map(Self::Bytes)
                .map_err(|err| D::Error::custom(format!("path_b64: {err}"))),
            _ => Err(D::Error::custom(
                "an entry needs exactly one of path and path_b64",
            )),
        }
    }
}
