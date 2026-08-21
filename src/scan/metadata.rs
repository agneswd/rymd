/// Platform-neutral metadata representation.
///
/// The scanner only ever sees these types. Linux-specific values such as
/// `st_dev`/`st_ino` live behind the platform layer.
use std::io;
use std::path::Path;

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

/// Identifies the filesystem containing a path (`st_dev` on Linux,
/// volume serial number on Windows).
#[allow(dead_code)]
pub type FilesystemId = u64;

/// Platform boundary for the scanner. All methods use lstat semantics:
/// symlinks are described by their own metadata, never by their target.
#[allow(dead_code)]
pub trait PlatformFilesystem: Send + Sync {
    fn metadata(&self, path: &Path) -> io::Result<FileMetadata>;
    fn filesystem_id(&self, path: &Path) -> io::Result<FilesystemId>;
    /// Free space in bytes on the filesystem that contains `path`.
    fn free_space(&self, path: &Path) -> io::Result<u64>;
}

pub fn platform() -> &'static dyn PlatformFilesystem {
    #[cfg(target_os = "linux")]
    {
        &super::platform::linux::LinuxFilesystem
    }
    #[cfg(target_os = "windows")]
    {
        &super::platform::windows::WindowsFilesystem
    }
}
