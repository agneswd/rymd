//! Linux implementation of the platform filesystem layer.
//!
//! Everything Linux-specific about scanning lives here: `MetadataExt`,
//! `st_dev`/`st_ino` identity, allocated blocks, and `statvfs` free space.
//! Windows support later means adding a sibling module, not touching the
//! scanner.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::super::metadata::{EntryKind, FileMetadata, PlatformFilesystem};

pub struct LinuxFilesystem;

impl PlatformFilesystem for LinuxFilesystem {
    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        // symlink_metadata: never follow symlinks.
        let md = std::fs::symlink_metadata(path)?;
        let kind = if md.is_dir() {
            EntryKind::Directory
        } else if md.is_symlink() {
            EntryKind::Symlink
        } else if md.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        Ok(FileMetadata {
            kind,
            logical: md.len(),
            allocated: md.blocks() * 512,
            modified_ms: mtime_ms(&md),
            device: md.dev(),
            inode: md.ino(),
            nlink: md.nlink(),
        })
    }

    fn filesystem_id(&self, path: &Path) -> io::Result<u64> {
        Ok(std::fs::symlink_metadata(path)?.dev())
    }

    fn free_space(&self, path: &Path) -> io::Result<u64> {
        statvfs_free(path)
    }
}

fn mtime_ms(md: &std::fs::Metadata) -> Option<i64> {
    md.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

/// Free bytes via `statvfs(3)` on the mount point that holds `path`.
fn statvfs_free(path: &Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt as _;
    // read_dir cannot produce names containing NUL, so this only fails for
    // paths we constructed ourselves.
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // f_bavail: free blocks available to unprivileged users.
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}
