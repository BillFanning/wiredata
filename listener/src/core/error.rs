//! Shared, typed error definitions used across the core data model and the
//! pipeline subsystems (spec §95, and the recorder contracts
//! §142).
//!
//! Core is a pure library layer: it uses `thiserror`-style typed errors, not
//! application-level `anyhow` (which belongs to the runtime/CLI/GUI layers).
//! The runtime wraps these with `.context()` at the crate boundary.

/// A recording subsystem failure (§56, §142). A recording fault is terminal for
/// the current artifact and never stalls reception (§56.1).
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// Underlying file I/O failed (create, write, flush, finalize).
    #[error("recording I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// The bounded recorder queue could not accept new data; per §56.1 the
    /// recorder faults rather than blocking the producer.
    #[error("recorder queue overflow; recording faulted")]
    QueueOverflow,
    /// The destination file is already locked by another recording — possibly another
    /// channel here or a second `listener` process (§121, ADR-014). Recording does not
    /// start; no existing data is touched.
    #[error("recording destination is already in use by another recording")]
    DestinationInUse,
    /// With file rotation, the destination is a **folder** of per-period files (§59),
    /// but the configured path is an existing **file**. Surfaced clearly instead of the
    /// opaque OS "cannot create a file when that file already exists" from `create_dir`.
    #[error(
        "rotation is on, so the destination must be a folder, but '{0}' is an existing \
         file — choose a folder, or turn rotation off to record to that file"
    )]
    RotationDestinationIsFile(String),
    /// The recording folder is gone, or no longer holds its
    /// `.wiredata-destination` marker (§59). Recovery waits for it to return
    /// rather than recreating it — an unplugged drive can leave an empty mount
    /// point on the system disk.
    #[error(
        "the recording folder '{0}' is missing or is not the one recording began in \
         (no .wiredata-destination marker) — waiting for it to return"
    )]
    DestinationMissing(String),
}
