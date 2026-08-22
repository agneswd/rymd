//! Linux implementation of the scanner backend.
//!
//! Enumeration uses `read_dir` (buffered `getdents64`); metadata for
//! spilled chunks is collected with `fstatat(dirfd, name,
//! AT_SYMLINK_NOFOLLOW)`, which resolves the name against the already-open
//! directory instead of walking the full path per entry. That is the same
//! class of optimization `dua`/`gdu` rely on and it removes both the VFS
//! path walk and a path allocation per entry.
//!
//! Identity (`st_dev`/`st_ino`), allocated blocks, and `statvfs` free
//! space round out the platform layer.

use std::io;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::MetadataExt;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::path::Path;

use super::super::metadata::{EntryKind, FileMetadata, ScannerBackend};
use super::super::scheduler::{EntryBatch, NameBlob};

pub struct LinuxFilesystem;

impl LinuxFilesystem {
    fn stat_names_inner(&self, dir: &Path, names: &NameBlob) -> Vec<Option<FileMetadata>> {
        let mut out = Vec::with_capacity(names.len());
        // One open per chunk. Plain O_RDONLY is enough to fstatat children.
        let Ok(file) = std::fs::File::open(dir) else {
            return vec![None; names.len()];
        };
        let fd = file.as_raw_fd();
        for ix in 0..names.len() {
            let bytes = names.bytes(ix);
            // POSIX names are at most NAME_MAX (255) bytes and cannot
            // contain NUL, so this always fits.
            let mut buf = [0u8; 260];
            if bytes.len() > 255 {
                out.push(None);
                continue;
            }
            buf[..bytes.len()].copy_from_slice(bytes);
            let cname = unsafe { std::ffi::CStr::from_bytes_with_nul_unchecked(
                &buf[..=bytes.len()],
            ) };
            let mut st: libc::stat64 = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::fstatat64(fd, cname.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW)
            };
            out.push(if rc == 0 {
                Some(md_from_stat(&st))
            } else {
                None
            });
        }
        out
    }
}

fn md_from_stat(st: &libc::stat64) -> FileMetadata {
    let kind = match st.st_mode & libc::S_IFMT {
        libc::S_IFDIR => EntryKind::Directory,
        libc::S_IFLNK => EntryKind::Symlink,
        libc::S_IFREG => EntryKind::File,
        _ => EntryKind::Other,
    };
    FileMetadata {
        kind,
        logical: st.st_size as u64,
        allocated: (st.st_blocks as u64) * 512,
        modified_ms: Some(unix_ms(st.st_mtime as i64, st.st_mtime_nsec)),
        device: st.st_dev as u64,
        inode: st.st_ino as u64,
        nlink: st.st_nlink as u64,
    }
}

/// Milliseconds since the epoch; saturates rather than panicking on
/// pre-epoch timestamps far from anything a disk actually holds.
fn unix_ms(secs: i64, nsecs: i64) -> i64 {
    (secs as i128 * 1000 + nsecs as i128 / 1_000_000).clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

impl ScannerBackend for LinuxFilesystem {
    fn name(&self) -> &'static str {
        "linux-fstatat"
    }

    /// Metadata arrives from a separate syscall here, so only a bounded
    /// prefix is gathered inline; the rest becomes stealable chunks.
    fn inline_limit(&self) -> usize {
        super::super::scheduler::CHUNK
    }

    fn enumerate(&self, dir: &Path, inline_limit: usize) -> io::Result<EntryBatch> {
        let rd = std::fs::read_dir(dir)?;
        let mut blob_parts: Vec<Vec<u8>> = Vec::with_capacity(64);
        let mut meta: Vec<Option<FileMetadata>> = Vec::new();
        for entry in rd {
            let entry = entry?;
            // DirEntry::metadata stats relative to the directory fd:
            // no full-path resolution, no extra allocation.
            let md = if meta.len() < inline_limit {
                match entry.metadata() {
                    Ok(md) => Some(FileMetadata {
                        kind: if md.is_dir() {
                            EntryKind::Directory
                        } else if md.is_symlink() {
                            EntryKind::Symlink
                        } else if md.is_file() {
                            EntryKind::File
                        } else {
                            EntryKind::Other
                        },
                        logical: md.len(),
                        allocated: md.blocks() * 512,
                        modified_ms: mtime_ms(&md),
                        device: md.dev(),
                        inode: md.ino(),
                        nlink: md.nlink(),
                    }),
                    Err(_) => None,
                }
            } else {
                None
            };
            meta.push(md);
            let name: OsString = entry.file_name();
            blob_parts.push(name.into_vec());
        }
        let names = NameBlob::from_flattened(blob_parts);
        Ok(EntryBatch { names, meta })
    }

    fn stat_names(&self, dir: &Path, names: &NameBlob) -> Vec<Option<FileMetadata>> {
        self.stat_names_inner(dir, names)
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        // symlink_metadata: never follow symlinks.
        let md = std::fs::symlink_metadata(path)?;
        Ok(FileMetadata {
            kind: if md.is_dir() {
                EntryKind::Directory
            } else if md.is_symlink() {
                EntryKind::Symlink
            } else if md.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            },
            logical: md.len(),
            allocated: md.blocks() * 512,
            modified_ms: mtime_ms(&md),
            device: md.dev(),
            inode: md.ino(),
            nlink: md.nlink(),
        })
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
