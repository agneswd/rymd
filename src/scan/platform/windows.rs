//! Windows implementation of the scanner backend.
//!
//! Directories are enumerated in bulk with
//! `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`: one handle per
//! directory yields names, attributes, logical size, allocation size
//! (sparse/compression aware), timestamps and file ids straight from the
//! directory records. No per-file `CreateFileW` round trip.
//!
//! What the bulk records do not carry is the hard-link count, so links are
//! not deduplicated on this path (`nlink` stays 0 and every link counts its
//! bytes). The NTFS fast path reads real link counts from the MFT.
//!
//! Filesystems that reject the bulk query (older FAT, some network shares)
//! fall back to per-entry `std::fs` metadata.

use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_FILES, GENERIC_READ, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileIdBothDirectoryInfo, GetDiskFreeSpaceExW, GetFileInformationByHandle,
    GetFileInformationByHandleEx, OPEN_EXISTING,
};

use super::super::metadata::{EntryKind, FileMetadata, ScannerBackend};
use super::super::scheduler::{EntryBatch, NameBlob};
use super::win_enum::{ParseOutcome, parse_id_both_dir_info};

const CLUSTER_FALLBACK: u64 = 4096;

/// Initial enumeration buffer; grows if a single record cannot fit.
const ENUM_BUF: usize = 64 * 1024;
/// Ceiling so a corrupt length report cannot balloon memory.
const MAX_ENUM_BUF: usize = 16 * 1024 * 1024;

pub struct WindowsFilesystem;

impl ScannerBackend for WindowsFilesystem {
    fn name(&self) -> &'static str {
        "windows-bulk-enum"
    }

    /// All metadata arrives with the records; nothing spills.
    fn inline_limit(&self) -> usize {
        usize::MAX
    }

    fn enumerate(&self, dir: &Path, _inline_limit: usize) -> io::Result<EntryBatch> {
        match self.enumerate_bulk(dir) {
            Ok(batch) => Ok(batch),
            Err(bulk_err) => self.enumerate_fallback(dir).map_err(|_| bulk_err),
        }
    }

    fn stat_names(&self, dir: &Path, names: &NameBlob) -> Vec<Option<FileMetadata>> {
        // Only reached if a caller spills despite inline_limit == MAX.
        (0..names.len())
            .map(|ix| {
                let mut p = PathBuf::from(dir);
                let name = names.name_os(ix);
                p.push(&*name);
                self.metadata(&p).ok()
            })
            .collect()
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        // symlink_metadata never follows reparse points.
        let md = std::fs::symlink_metadata(path)?;
        let kind = kind_of_attributes(md.file_attributes());
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
        let wide = to_wide(path);
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

fn kind_of_attributes(attrs: u32) -> EntryKind {
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        // Symlinks and junctions are reparse points; both are described by
        // their own metadata and never descended into.
        EntryKind::Symlink
    } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        EntryKind::Directory
    } else if attrs & FILE_ATTRIBUTE_DEVICE != 0 {
        EntryKind::Other
    } else {
        EntryKind::File
    }
}

impl WindowsFilesystem {
    fn enumerate_bulk(&self, dir: &Path) -> io::Result<EntryBatch> {
        let wide = to_wide(dir);
        // SAFETY: path pointer lives across the call; flags request
        // directory listing access only and never follow reparse points.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY | GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let volume_serial = volume_serial_of(handle);

        let mut buf = vec![0u8; ENUM_BUF];
        let mut meta: Vec<Option<FileMetadata>> = Vec::with_capacity(128);
        let mut blob_parts: Vec<Vec<u16>> = Vec::with_capacity(128);
        let mut name_units: Vec<u16> = Vec::new();

        loop {
            // The call does not report how much it wrote, so the buffer is
            // zeroed each round and parsing walks until a zero offset or an
            // untouched tail.
            buf.fill(0);
            // SAFETY: buf is a valid writable buffer of its full length.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    handle,
                    FileIdBothDirectoryInfo,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                )
            };
            if ok == 0 {
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(code) if code == ERROR_NO_MORE_FILES as i32 => break,
                    Some(code) if code == ERROR_INSUFFICIENT_BUFFER as i32 => {
                        if buf.len() >= MAX_ENUM_BUF {
                            // SAFETY: created above, closed exactly once.
                            unsafe { CloseHandle(handle) };
                            return Err(err);
                        }
                        buf.resize(buf.len() * 2, 0);
                        continue;
                    }
                    _ => {
                        // SAFETY: created above, closed exactly once.
                        unsafe { CloseHandle(handle) };
                        return Err(err);
                    }
                }
            }

            match parse_id_both_dir_info(&buf) {
                ParseOutcome::Records(records, _terminated) => {
                    for r in records {
                        let units = &buf[r.name_offset..r.name_offset + r.name_units * 2];
                        name_units.clear();
                        name_units.extend(
                            units
                                .as_chunks::<2>()
                                .0
                                .iter()
                                .map(|c| u16::from_le_bytes(*c)),
                        );
                        blob_parts.push(std::mem::take(&mut name_units));
                        meta.push(Some(record_to_md(&r, volume_serial)));
                    }
                }
                ParseOutcome::Corrupt => {
                    // SAFETY: created above, closed exactly once.
                    unsafe { CloseHandle(handle) };
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory enumeration returned a malformed record",
                    ));
                }
            }
        }

        // SAFETY: created above, closed exactly once.
        unsafe { CloseHandle(handle) };

        let names = NameBlob::from_wide_parts(blob_parts);
        Ok(EntryBatch { names, meta })
    }

    /// Per-entry fallback for filesystems that reject the bulk query.
    fn enumerate_fallback(&self, dir: &Path) -> io::Result<EntryBatch> {
        let rd = std::fs::read_dir(dir)?;
        let mut blob_parts: Vec<Vec<u16>> = Vec::with_capacity(64);
        let mut meta: Vec<Option<FileMetadata>> = Vec::new();
        for entry in rd {
            let entry = entry?;
            let child = dir.join(entry.file_name());
            meta.push(self.metadata(&child).ok());
            blob_parts.push(entry.file_name().encode_wide().collect());
        }
        let names = NameBlob::from_wide_parts(blob_parts);
        Ok(EntryBatch { names, meta })
    }
}

fn record_to_md(r: &super::win_enum::DirRecord, device: u64) -> FileMetadata {
    let kind = kind_of_attributes(r.attributes);
    let allocated = match kind {
        // Real allocation from the record: sparse, compressed and unusual
        // cluster sizes all land here correctly.
        EntryKind::File => r.allocation_size,
        _ => 0,
    };
    FileMetadata {
        kind,
        logical: r.end_of_file,
        allocated,
        modified_ms: super::win_enum::filetime_to_unix_ms(r.last_write_filetime),
        device,
        // FileId from the record; unique per volume on NTFS/ReFS.
        inode: r.file_id,
        nlink: 0,
    }
}

/// Volume serial number of the filesystem behind `handle`; 0 on failure.
fn volume_serial_of(handle: HANDLE) -> u64 {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: info is a valid out parameter for this call.
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    if ok == 0 {
        0
    } else {
        u64::from(info.dwVolumeSerialNumber)
    }
}

/// Open a handle with attribute access only, then read identity fields.
fn file_identity(path: &Path) -> io::Result<(u64, u64, u64)> {
    let wide = to_wide(path);
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

fn to_wide(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
