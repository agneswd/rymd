//! Windows implementation of the scanner backend.
//!
//! Identity comes from the volume serial number plus the NTFS file index,
//! which is the Windows analogue of Linux `st_dev` + `st_ino`.
//!
//! The current implementation stats one entry at a time through
//! `CreateFileW` + `GetFileInformationByHandle`, exactly like the original
//! platform layer did. It is correct but syscall-heavy; the planned
//! replacement enumerates whole directories with
//! `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` so names,
//! attributes, sizes, allocation sizes and link counts arrive from
//! directory records without opening every file.

use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceExW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};

use super::super::metadata::{EntryBatch, EntryKind, FileMetadata, ScannerBackend};
use super::super::scheduler::NameBlob;

const CLUSTER_FALLBACK: u64 = 4096;

pub struct WindowsFilesystem;

impl ScannerBackend for WindowsFilesystem {
    fn name(&self) -> &'static str {
        "windows-per-file"
    }

    /// All metadata is gathered inline during enumeration, so no chunking.
    fn inline_limit(&self) -> usize {
        usize::MAX
    }

    fn enumerate(&self, dir: &Path, _inline_limit: usize) -> io::Result<EntryBatch> {
        let rd = std::fs::read_dir(dir)?;
        let mut blob_parts: Vec<Vec<u8>> = Vec::with_capacity(64);
        let mut meta: Vec<Option<FileMetadata>> = Vec::new();
        for entry in rd {
            let entry = entry?;
            let child = dir.join(entry.file_name());
            meta.push(self.metadata(&child).ok());
            blob_parts.push(entry.file_name().into_vec());
        }
        let names = NameBlob::from_flattened(blob_parts);
        Ok(EntryBatch { names, meta })
    }

    fn stat_names(&self, dir: &Path, names: &NameBlob) -> Vec<Option<FileMetadata>> {
        // Only reached if a caller spills despite inline_limit == MAX.
        (0..names.len())
            .map(|ix| {
                let mut p = PathBuf::from(dir);
                p.push(names.name_os(ix).as_ref());
                self.metadata(&p).ok()
            })
            .collect()
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        // symlink_metadata never follows reparse points.
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

        let logical = md.len();
        let allocated = match kind {
            EntryKind::File => logical.div_ceil(CLUSTER_FALLBACK) * CLUSTER_FALLBACK,
            _ => 0,
        };
        let (device, inode, nlink) = file_identity(path).unwrap_or((0, 0, 1));

        Ok(FileMetadata {
            kind,
            logical,
            allocated,
            modified_ms: mtime_ms(&md),
            device,
            inode,
            nlink,
        })
    }

    fn free_space(&self, path: &Path) -> io::Result<u64> {
        let wide = to_wide(&path.to_string_lossy());
        let mut free: u64 = 0;
        // SAFETY: pointers are valid for the call; zero-initialized out param.
        let rc = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut free,
            )
        };
        if rc == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(free)
    }
}

/// Open a handle with attribute access only, then read identity fields.
fn file_identity(path: &Path) -> io::Result<(u64, u64, u64)> {
    let wide = to_wide(&path.to_string_lossy());
    // SAFETY: path pointer stays alive across the call; flags request
    // metadata-only access and never follow reparse points.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(io::Error::last_os_error());
    }

    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: info is a valid out parameter for this call.
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    // SAFETY: handle was created above and is closed exactly once.
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    let volume = info.dwVolumeSerialNumber as u64;
    let index = ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64);
    Ok((volume, index, info.nNumberOfLinks as u64))
}

fn mtime_ms(md: &std::fs::Metadata) -> Option<i64> {
    md.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
