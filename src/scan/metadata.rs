/// Platform-neutral metadata representation.
///
/// The scanner only ever sees these types. Linux-specific values such as
/// `st_dev`/`st_ino` live behind the platform layer.
use std::io;
use std::path::Path;

use super::scheduler::{EntryBatch, NameBlob};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

/// Everything the scanner needs to know about one filesystem entry,
/// gathered without following symlinks (lstat semantics).
#[derive(Clone, Copy, Debug)]
pub struct FileMetadata {
    pub kind: EntryKind,
    /// Apparent size in bytes (`st_size`).
    pub logical: u64,
    /// Blocks actually allocated on disk (`st_blocks * 512` on Linux).
    pub allocated: u64,
    /// Modification time in unix milliseconds.
    pub modified_ms: Option<i64>,
    /// Filesystem identity (`st_dev` on Linux).
    pub device: u64,
    /// Inode identity (`st_ino` on Linux).
    pub inode: u64,
    pub nlink: u64,
}

/// Platform boundary for the scanner. All methods use lstat semantics:
/// symlinks are described by their own metadata, never by their target.
#[allow(dead_code)]
pub trait ScannerBackend: Send + Sync {
    /// Human-readable name for diagnostics.
    fn name(&self) -> &'static str;

    /// How many entries of a freshly enumerated directory this backend
    /// fills metadata for during enumeration itself. Entries beyond the
    /// limit spill as parallel [`ScannerBackend::stat_names`] chunks.
    /// `usize::MAX` means enumeration already carries full metadata
    /// (bulk directory reads) and no chunking is needed.
    fn inline_limit(&self) -> usize {
        usize::MAX
    }

    /// List the entry names of `dir`. Names are raw OS strings; metadata is
    /// attached up to `inline_limit()` entries.
    fn enumerate(&self, dir: &Path, inline_limit: usize) -> io::Result<EntryBatch>;

    /// Collect lstat metadata for every name in `names`, which came from a
    /// single earlier enumeration of `dir`. Per-name failures become
    /// `None` and count as scan errors.
    fn stat_names(&self, dir: &Path, names: &NameBlob) -> Vec<Option<FileMetadata>>;

    /// Full-path metadata with lstat semantics (used for the scan root).
    fn metadata(&self, path: &Path) -> io::Result<FileMetadata>;

    /// Free bytes on the filesystem that contains `path`.
    fn free_space(&self, path: &Path) -> io::Result<u64>;
}

/// The backend for the running platform.
pub fn backend() -> &'static dyn ScannerBackend {
    #[cfg(target_os = "linux")]
    {
        &super::platform::linux::LinuxFilesystem
    }
    #[cfg(target_os = "windows")]
    {
        &super::platform::windows::WindowsFilesystem
    }
}
