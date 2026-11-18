//! Sensors: free space by path for `reconcile` and the runner's job-end check
//! (`docs/architecture.md` sections 5.2 and 10.4), and the phase 2 seam of admission, which
//! decides from reservations alone, so its implementation reports every sensor as
//! unavailable (section 1).
//!
//! **Readings fake** (section 13.5). With `CARDINAL_TEST_DIR` set, a test can write
//! `<dir>/disk-readings.json`, `{"volumes": [{"prefix": P, "free": [F, …], "total": T}]}`:
//! a path at or below `P` (the longest such prefix) is then on a volume of its own, mounted at
//! `P`, and each read of it takes the next of the `free` values, the last one repeating; a
//! `null` is a read that fails. Paths below no prefix, and every path outside tests, are read
//! from the OS.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use crate::os::{Measure, Space, Volume};

/// A sensor reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reading {
    /// The sensor could not be read; admission decides from reservations alone.
    Unavailable,
    Value(u64),
}

/// Host sensors read by admission.
pub trait Sensors {
    /// Memory pressure (phase 2).
    fn memory(&self) -> Reading;
    /// Free bytes on the volume holding `path` (phase 1).
    fn disk_free(&self, path: &Path) -> Reading;
}

/// The phase 0 sensors: nothing is available.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unavailable;

impl Sensors for Unavailable {
    fn memory(&self) -> Reading {
        Reading::Unavailable
    }

    fn disk_free(&self, _path: &Path) -> Reading {
        Reading::Unavailable
    }
}

/// The free and total bytes of the volume holding `path`, measured as `measure` says, or
/// the readings fake's next value for it.
pub fn free_space(path: &Path, measure: Measure) -> Result<Space, String> {
    if let Some(fake) = Fake::load()
        && let Some(index) = fake.find(path)
    {
        return fake.take(index);
    }
    crate::os::disk(path, measure).map_err(|err| err.to_string())
}

/// The volume holding `path`, or the readings fake's volume for it.
pub fn volume(path: &Path) -> Result<Volume, String> {
    fake_volume(path).map_or_else(|| crate::os::volume(path).map_err(|e| e.to_string()), Ok)
}

/// The volume holding the entry at `path` itself, a final symbolic link not followed
/// (`os::entry_volume`), or the readings fake's volume for it.
pub fn entry_volume(path: &Path) -> Result<Volume, String> {
    fake_volume(path).map_or_else(
        || crate::os::entry_volume(path).map_err(|e| e.to_string()),
        Ok,
    )
}

/// The readings fake's volume for `path`, if it makes one.
fn fake_volume(path: &Path) -> Option<Volume> {
    let fake = Fake::load()?;
    let prefix = &fake.file.volumes[fake.find(path)?].prefix;
    let mut id = b"fake:".to_vec();
    id.extend_from_slice(prefix.as_os_str().as_encoded_bytes());
    Some(Volume {
        id,
        mount: prefix.clone(),
    })
}

/// The file of the readings fake.
#[derive(Debug, Default, Serialize, Deserialize)]
struct FakeFile {
    volumes: Vec<FakeVolume>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FakeVolume {
    prefix: PathBuf,
    free: Vec<Option<u64>>,
    total: u64,
}

/// The readings fake as loaded from the test directory.
struct Fake {
    path: PathBuf,
    file: FakeFile,
}

impl Fake {
    const FILE: &str = "disk-readings.json";

    /// The fake, when `CARDINAL_TEST_DIR` is set and holds one.
    fn load() -> Option<Self> {
        let dir = test_dir()?;
        let path = dir.join(Self::FILE);
        let bytes = std::fs::read(&path).ok()?;
        let file = serde_json::from_slice(&bytes).ok()?;
        Some(Self { path, file })
    }

    /// The volume with the longest prefix at or above `path`.
    fn find(&self, path: &Path) -> Option<usize> {
        self.file
            .volumes
            .iter()
            .enumerate()
            .filter(|(_, v)| path.starts_with(&v.prefix))
            .max_by_key(|(_, v)| v.prefix.components().count())
            .map(|(index, _)| index)
    }

    /// The next reading of volume `index`, consumed unless it is the last.
    fn take(mut self, index: usize) -> Result<Space, String> {
        let volume = &mut self.file.volumes[index];
        let reading = if volume.free.len() > 1 {
            let reading = volume.free.remove(0);
            let bytes = serde_json::to_vec(&self.file).map_err(|err| err.to_string())?;
            let tmp = self
                .path
                .with_extension(format!("{}.tmp", std::process::id()));
            std::fs::write(&tmp, bytes)
                .and_then(|()| std::fs::rename(&tmp, &self.path))
                .map_err(|err| format!("{}: {err}", self.path.display()))?;
            reading
        } else {
            volume.free.first().copied().flatten()
        };
        let volume = &self.file.volumes[index];
        match reading {
            Some(free_bytes) => Ok(Space {
                free_bytes,
                total_bytes: volume.total,
            }),
            None => Err(format!(
                "{}: the readings fake fails this read",
                volume.prefix.display()
            )),
        }
    }
}

/// `CARDINAL_TEST_DIR`, read once.
fn test_dir() -> Option<&'static Path> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        std::env::var_os("CARDINAL_TEST_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
    })
    .as_deref()
}
