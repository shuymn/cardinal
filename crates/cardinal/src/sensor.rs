//! Sensors: the phase 2 seam. Phase 0 admission decides from reservations alone, so the only
//! implementation reports every sensor as unavailable (`docs/architecture.md` section 1).

use std::path::Path;

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
