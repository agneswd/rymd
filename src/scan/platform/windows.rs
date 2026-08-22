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

/// `.` and `..` in UTF-16 code units.
fn is_dot_entry(units: &[u16]) -> bool {
    units == [0x2E] || units == [0x2E, 0x2E]
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
        let debug = std::env::var_os("RYMD_SCAN_DEBUG").is_some();
        let mut rounds = 0u32;

        loop {
            rounds += 1;
            if debug && rounds > 8 {
                eprintln!("rymd: enumerate_bulk {dir:?} round {rounds}");
            }
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
                        if debug {
                            eprintln!(
                                "rymd: enumerate_bulk {dir:?} stopping on error {}",
                                err.raw_os_error().unwrap_or(-1)
                            );
                        }
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
                        // Some volumes hand back `.` and `..` even though
                        // the API contract says they are excluded; either
                        // would loop forever or escape the scan root.
                        if is_dot_entry(&name_units) {
                            continue;
                        }
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

/// Identity of a path for delete-safety checks: (volume serial, file
/// index, link count). Used by `actions::fs_ops` on Windows where
/// directory-entry metadata cannot provide stable ids.
pub fn identity_of(path: &Path) -> io::Result<(u64, u64, u64)> {
    file_identity(path)
}

/// Whether `path` is currently a directory (lstat semantics: reparse
/// points do not count).
pub fn is_dir_no_follow(path: &Path) -> io::Result<bool> {
    let md = std::fs::symlink_metadata(path)?;
    Ok(md.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0
        && md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_bulk_enumeration_scans_temp_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test_file.bin");
        std::fs::write(&file_path, vec![0xAB; 8192]).unwrap();
        let sub_dir = dir.path().join("sub_dir");
        std::fs::create_dir(&sub_dir).unwrap();
        let sub_file = sub_dir.join("sub.txt");
        std::fs::write(&sub_file, b"hello").unwrap();

        let fs = WindowsFilesystem;
        let batch = fs.enumerate(dir.path(), usize::MAX).unwrap();
        assert!(batch.names.len() >= 2);
        let names: Vec<String> = (0..batch.names.len())
            .map(|i| batch.names.name_os(i).to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "test_file.bin"));
        assert!(names.iter().any(|n| n == "sub_dir"));
    }
}
