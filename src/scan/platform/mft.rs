//! NTFS MFT fast-path parsing.
//!
//! The master file table contains every file's name, parent reference,
//! sizes and timestamps in one contiguous stream, which is how modern
//! Windows scanners enumerate an NTFS volume far faster than walking
//! directories. Reading it requires volume access (administrator rights);
//! when that is unavailable Rymd falls back to directory traversal.
//!
//! This module is deliberately split in two:
//!
//! - A pure byte-parsing layer (record headers, update-sequence fixups,
//!   attribute walks, run lists, hierarchy reconstruction). It never calls
//!   Windows and is unit-tested on every platform, including malformed and
//!   hostile input.
//! - A thin Windows-only I/O shell (`windows.rs`) that opens the volume,
//!   reads the boot sector and `FSCTL_GET_NTFS_VOLUME_DATA`, streams the
//!   MFT and feeds the parsers above.
//!
//! References: Microsoft's documented on-disk structures (NTFS
//! `MASTER_FILE_TABLE`, `MULTI_SECTOR_HEADER`, `ATTRIBUTE_RECORD_HEADER`,
//! `NTFS_VOLUME_DATA_BUFFER`). The implementation is independent.

use std::collections::HashMap;

use super::win_enum::filetime_to_unix_ms;
use crate::model::{Node, NodeId, NodeKind, ScanModel};

/// Parse failures are data problems, not I/O problems; callers translate
/// them into a clean fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MftError {
    /// Record does not start with the FILE magic.
    BadMagic,
    /// Update sequence array disagrees with the recorded check values.
    FixupMismatch,
    /// An offset or length field points outside its container.
    OutOfBounds,
    /// A required attribute is missing or malformed.
    MalformedAttribute,
}

const RECORD_HEADER_LEN: usize = 0x30;
const ATTR_HEADER_RESIDENT_LEN: usize = 0x18;

pub const ATTR_TYPE_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_TYPE_FILE_NAME: u32 = 0x30;
pub const ATTR_TYPE_DATA: u32 = 0x80;
pub const ATTR_TYPE_INDEX_ROOT: u32 = 0x90;

/// Record flags (`u16` at offset 0x38).
pub const RECORD_FLAG_IN_USE: u16 = 0x0001;
pub const RECORD_FLAG_DIRECTORY: u16 = 0x0002;

/// Apply the multi-sector header (update sequence array / fixups) in place.
///
/// Every 512-byte sector's trailing two bytes were replaced with a check
/// value when the record hit disk; the originals live in the array right
/// after the USN. Returns `FixupMismatch` if any sector tail disagrees,
/// which is the classic signature of a torn or corrupted record.
pub fn apply_fixups(record: &mut [u8]) -> Result<(), MftError> {
    if record.len() < RECORD_HEADER_LEN {
        return Err(MftError::OutOfBounds);
    }
    let usa_offset = u16_at(record, 0x30) as usize;
    let usa_count = u16_at(record, 0x32) as usize;
    // One USN (2 bytes) plus (count - 1) fixup pairs.
    if usa_count == 0 || usa_offset + usa_count * 2 > record.len() {
        return Err(MftError::OutOfBounds);
    }
    let usn = [record[usa_offset], record[usa_offset + 1]];
    let sector = 512usize;
    if !record.len().is_multiple_of(sector) {
        return Err(MftError::OutOfBounds);
    }
    for i in 1..usa_count {
        let val_off = usa_offset + i * 2;
        // The fixup belongs at the end of the i-th 512-byte sector.
        let tail = i * sector - 2;
        if tail + 2 > record.len() {
            return Err(MftError::OutOfBounds);
        }
        if record[tail] != usn[0] || record[tail + 1] != usn[1] {
            return Err(MftError::FixupMismatch);
        }
        record[tail] = record[val_off];
        record[tail + 1] = record[val_off + 1];
    }
    Ok(())
}

/// Whether the record header claims this entry is present and valid.
pub fn record_in_use(record: &[u8]) -> bool {
    record.len() >= 0x3A && flags_of(record) & RECORD_FLAG_IN_USE != 0
}

/// Whether the record describes a directory (from the header flags).
pub fn record_is_directory(record: &[u8]) -> bool {
    record.len() >= 0x3A && flags_of(record) & RECORD_FLAG_DIRECTORY != 0
}

fn flags_of(record: &[u8]) -> u16 {
    u16_at(record, 0x38)
}

/// Iterate raw attribute records inside an MFT record.
///
/// Yields subslices; malformed lengths terminate the iteration instead of
/// panicking or running past the buffer.
pub fn attributes(record: &[u8]) -> impl Iterator<Item = &[u8]> {
    let start = if record.len() >= 0x20 {
        u16_at(record, 0x14) as usize
    } else {
        0
    };
    let mut off = start;
    std::iter::from_fn(move || {
        if off + 0x10 > record.len() {
            return None;
        }
        let attr_type = u32_at(record, off);
        if attr_type == 0xFFFF_FFFF {
            return None;
        }
        let len = u32_at(record, off + 4) as usize;
        if len < ATTR_HEADER_RESIDENT_LEN || off + len > record.len() {
            return None;
        }
        let attr = &record[off..off + len];
        off += len;
        Some(attr)
    })
}

/// A `$FILE_NAME` attribute body.
#[derive(Debug, Clone)]
pub struct FileNameInfo {
    /// Parent directory's record number (low 48 bits of the reference).
    pub parent_record: u64,
    /// Parent reference sequence number (top 16 bits).
    pub parent_sequence: u16,
    /// Name in UTF-16 code units (not NUL terminated).
    pub name_units: u16,
    /// Offset of the name inside the attribute body.
    pub name_offset: usize,
    /// Namespace (0 = POSIX, 1 = Win32, 2 = DOS, 3 = both).
    pub namespace: u8,
}

/// Parse a `$FILE_NAME` attribute body (the bytes after the resident
/// attribute header).
pub fn parse_file_name(body: &[u8]) -> Result<FileNameInfo, MftError> {
    // Fixed part: parent(8) creation(8) modification(8) mft_mod(8)
    // access(8) allocated(8) real(8) flags(4) reparse_tag(4)
    // name_len(1) namespace(1).
    const FIXED: usize = 66;
    if body.len() < FIXED {
        return Err(MftError::OutOfBounds);
    }
    let full_ref = u64_at(body, 0);
    let name_units = body[64] as usize;
    let namespace = body[65];
    let name_offset = FIXED;
    if name_offset + name_units * 2 > body.len() {
        return Err(MftError::OutOfBounds);
    }
    Ok(FileNameInfo {
        parent_record: full_ref & 0x0000_FFFF_FFFF_FFFF,
        parent_sequence: (full_ref >> 48) as u16,
        name_units: name_units as u16,
        name_offset,
        namespace,
    })
}

/// A `$DATA` attribute summary.
#[derive(Debug, Clone, Copy, Default)]
pub struct DataInfo {
    pub resident: bool,
    /// Logical size (`EndOfFile` equivalent).
    pub real_size: u64,
    /// Allocated size on disk.
    pub allocated_size: u64,
    /// Byte offset of the run list inside the attribute (non-resident).
    pub runs_offset: usize,
    /// Length of the run list region within the attribute.
    pub runs_len: usize,
}

/// Parse a `$DATA` attribute record (the full attribute, including its
/// resident/non-resident header).
pub fn parse_data_attr(attr: &[u8]) -> Result<DataInfo, MftError> {
    if attr.len() < 0x10 {
        return Err(MftError::OutOfBounds);
    }
    let non_resident = attr[8] != 0;
    if !non_resident {
        // Resident: value_len @0x10, value_off @0x14.
        if attr.len() < 0x16 {
            return Err(MftError::OutOfBounds);
        }
        let value_len = u32_at(attr, 0x10) as u64;
        Ok(DataInfo {
            resident: true,
            real_size: value_len,
            allocated_size: value_len.next_multiple_of(8),
            runs_offset: 0,
            runs_len: 0,
        })
    } else {
        // Non-resident: start_vcn@0x10 last_vcn@0x18 runs_off@0x20
        // compress@0x22, alloc@0x28 real@0x30 initialized@0x38.
        if attr.len() < 0x38 {
            return Err(MftError::OutOfBounds);
        }
        let runs_off = u16_at(attr, 0x20) as usize;
        if runs_off >= attr.len() {
            return Err(MftError::OutOfBounds);
        }
        Ok(DataInfo {
            resident: false,
            real_size: u64_at(attr, 0x30),
            allocated_size: u64_at(attr, 0x28),
            runs_offset: runs_off,
            runs_len: attr.len() - runs_off,
        })
    }
}

/// Decode an MFT data-run list into `(lcn, cluster_count)` extents.
///
/// Run lists are variable-width integers: a header byte holds the width of
/// the length field (low nibble) and offset field (high nibble). Offset
/// fields are signed relative to the previous extent; a zero offset marks
/// a sparse extent.
pub fn decode_runs(runs: &[u8]) -> Result<Vec<(u64, u64)>, MftError> {
    let mut out = Vec::new();
    let mut lcn: i64 = 0;
    let mut off = 0usize;
    loop {
        if off >= runs.len() {
            break;
        }
        let header = runs[off];
        if header == 0 {
            break; // terminator
        }
        off += 1;
        let len_width = (header & 0x0F) as usize;
        let off_width = (header >> 4) as usize;
        if len_width == 0 || len_width > 8 || off_width > 8 {
            return Err(MftError::MalformedAttribute);
        }
        if off + len_width > runs.len() {
            return Err(MftError::OutOfBounds);
        }
        let mut clusters: u64 = 0;
        for (i, b) in runs[off..off + len_width].iter().enumerate() {
            clusters |= u64::from(*b) << (i * 8);
        }
        off += len_width;
        if off + off_width > runs.len() {
            return Err(MftError::OutOfBounds);
        }
        // A missing offset field marks a sparse extent: clusters exist in
        // the stream's virtual range but map to no LCN.
        if off_width == 0 {
            continue;
        }
        let mut delta: i64 = 0;
        for (i, b) in runs[off..off + off_width].iter().enumerate() {
            delta |= i64::from(*b) << (i * 8);
        }
        // Sign-extend the last byte.
        if off_width > 0 {
            let shift = 64 - off_width * 8;
            if shift < 64 {
                delta = (delta << shift) >> shift;
            }
        }
        off += off_width;
        if clusters == 0 {
            continue;
        }
        lcn = lcn.checked_add(delta).ok_or(MftError::MalformedAttribute)?;
        if lcn < 0 {
            return Err(MftError::MalformedAttribute);
        }
        out.push((lcn as u64, clusters));
    }
    Ok(out)
}

/// Everything extracted from one MFT record that the tree builder needs.
#[derive(Debug, Clone)]
pub struct Entry {
    pub record_no: u64,
    pub parent_record: Option<u64>,
    /// UTF-16 code units of the chosen name.
    pub name_units: Vec<u16>,
    pub is_directory: bool,
    pub logical: u64,
    pub allocated: u64,
    pub modified_ms: Option<i64>,
    /// Number of `$FILE_NAME` attributes: >1 means hard links exist.
    pub nlink: u64,
}

/// Record number of the volume root directory.
pub const ROOT_RECORD: u64 = 5;
/// Records 0..META_RECORDS are filesystem metadata ($MFT, $LogFile, ...),
/// not user data; they are excluded from the model.
pub const META_RECORDS: u64 = 16;

fn name_to_os(units: &[u16]) -> std::ffi::OsString {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt as _;
        std::ffi::OsString::from_wide(units)
    }
    #[cfg(not(windows))]
    {
        // Test builds on other platforms decode names lossily.
        String::from_utf16_lossy(units).into()
    }
}

/// Rebuild the hierarchy from parent references into a [`ScanModel`].
///
/// Records are placed by breadth-first traversal from the root directory,
/// so every parent lands in the arena before its children and the single
/// reverse-pass aggregation stays correct. Corrupt parent references
/// (self-loops, missing targets, cycles) attach the record to the scan
/// root instead of dropping it. Returns `None` when the root record is
/// missing entirely.
pub fn build_model(
    root_path: &std::path::Path,
    device: u64,
    entries: &[Entry],
) -> Option<ScanModel> {
    let mut ix_of: HashMap<u64, usize> = HashMap::with_capacity(entries.len());
    for (i, e) in entries.iter().enumerate() {
        ix_of.insert(e.record_no, i);
    }
    let root_ix = *ix_of.get(&ROOT_RECORD)?;

    // Adjacency over entry indices.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    for (i, e) in entries.iter().enumerate() {
        if i == root_ix || e.record_no < META_RECORDS {
            continue;
        }
        let parent = e
            .parent_record
            .filter(|p| *p != e.record_no && *p >= META_RECORDS);
        match parent.and_then(|p| ix_of.get(&p).copied()) {
            Some(p) if p != i => children[p].push(i),
            _ => children[root_ix].push(i),
        }
    }

    let mut model = ScanModel::new(root_path.to_path_buf(), device);
    model.nodes_mut().reserve(entries.len());

    // BFS assigning arena ids level by level.
    let mut id_of: Vec<Option<NodeId>> = vec![None; entries.len()];
    let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    queue.push_back(root_ix);

    // The root becomes node 0 with the drive path's own file name.
    {
        let root_entry = &entries[root_ix];
        let root_node = Node {
            parent: None,
            name: root_path
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_else(|| root_path.as_os_str().to_os_string()),
            kind: NodeKind::Directory,
            own_logical: 0,
            own_allocated: 0,
            agg_logical: 0,
            agg_allocated: 0,
            file_count: 0,
            dir_count: 0,
            modified_ms: root_entry.modified_ms,
            device,
            inode: ROOT_RECORD,
            children: Vec::new(),
            flags: 0,
        };
        model.nodes_mut().push(root_node);
        id_of[root_ix] = Some(NodeId(0));
    }

    while let Some(ix) = queue.pop_front() {
        let my_id = id_of[ix].unwrap();
        for child_ix in &children[ix] {
            let e = &entries[*child_ix];
            let id = NodeId(model.len() as u32);
            let kind = if e.is_directory {
                NodeKind::Directory
            } else {
                NodeKind::File
            };
            let node = Node {
                parent: Some(my_id),
                name: name_to_os(&e.name_units),
                kind,
                own_logical: e.logical,
                own_allocated: e.allocated,
                agg_logical: e.logical,
                agg_allocated: e.allocated,
                file_count: u64::from(!e.is_directory),
                dir_count: u64::from(e.is_directory),
                modified_ms: e.modified_ms,
                device,
                inode: e.record_no,
                children: Vec::new(),
                flags: 0,
            };
            model.nodes_mut().push(node);
            id_of[*child_ix] = Some(id);
            model.nodes_mut()[my_id.index()].children.push(id);
            queue.push_back(*child_ix);
        }
    }

    Some(model)
}

/// Extract [`Entry`] data from a raw record (already validated to start
/// with `FILE` and be marked in use).
///
/// Names: the first `$FILE_NAME` in a Win32-capable namespace wins; DOS
/// 8.3 aliases are only used when nothing better exists. Multiple
/// `$FILE_NAME` attributes (hard links) collapse to one canonical
/// location, so storage is counted exactly once without any dedup pass.
pub fn extract_entry(record_no: u64, record: &mut [u8]) -> Result<Entry, MftError> {
    apply_fixups(record)?;
    let is_directory = record_is_directory(record);

    let mut best_name: Option<FileNameInfo> = None;
    let mut link_count: u64 = 0;
    let mut data: Option<DataInfo> = None;
    let mut index_alloc: Option<DataInfo> = None;
    let mut modified_ms: Option<i64> = None;

    for attr in attributes(record) {
        match u32_at(attr, 0) {
            ATTR_TYPE_STANDARD_INFORMATION => {
                // Body starts after the resident header (0x10); the
                // modification time sits at body offset 0x08.
                if attr.len() >= 0x10 + 0x10 {
                    let ft = i64_at(attr, 0x10 + 0x08);
                    modified_ms = filetime_to_unix_ms(ft);
                }
            }
            ATTR_TYPE_FILE_NAME => {
                // Skip resident-header bytes to reach the attribute body.
                let Some(body) = attr.get(0x10..) else {
                    return Err(MftError::OutOfBounds);
                };
                let info = parse_file_name(body)?;
                link_count += 1;
                let better = match (best_name.as_ref().map(|n| n.namespace), info.namespace) {
                    (_, 3) | (_, 1) => true,
                    // DOS aliases and POSIX names lose against any Win32 name.
                    (Some(1) | Some(3), _) => false,
                    _ => true,
                };
                if better || best_name.is_none() {
                    best_name = Some(info);
                }
            }
            ATTR_TYPE_DATA => {
                // Only the unnamed stream is the file's content.
                let has_name = attr[6] != 0;
                if !has_name && data.is_none() {
                    data = Some(parse_data_attr(attr)?);
                }
            }
            ATTR_TYPE_INDEX_ROOT | 0xA0 /* INDEX_ALLOCATION */ if index_alloc.is_none() => {
                index_alloc = parse_data_attr(attr).ok();
            }
            _ => {}
        }
    }

    let Some(name_info) = best_name else {
        return Err(MftError::MalformedAttribute);
    };
    // Copy the UTF-16 name out of the winning attribute.
    let name_units: Vec<u16>;
    {
        // Re-walk to find the chosen attribute again (cheap; records have
        // few attributes) so we borrow from `record`, not a stale slice.
        let mut found: Option<Vec<u16>> = None;
        for attr in attributes(record) {
            if u32_at(attr, 0) != ATTR_TYPE_FILE_NAME || attr.len() < 0x10 {
                continue;
            }
            if let Ok(info) = parse_file_name(&attr[0x10..])
                && info.name_units == name_info.name_units
                && info.namespace == name_info.namespace
                && info.parent_record == name_info.parent_record
            {
                let body = &attr[0x10..];
                let start = info.name_offset;
                let end = start + info.name_units as usize * 2;
                found = body.get(start..end).map(|raw| {
                    raw.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes(*c))
                        .collect()
                });
                break;
            }
        }
        name_units = found.ok_or(MftError::MalformedAttribute)?;
    }

    let (logical, allocated) = if is_directory {
        (
            0,
            index_alloc.as_ref().map(|d| d.allocated_size).unwrap_or(0),
        )
    } else {
        match data {
            Some(d) => (d.real_size, d.allocated_size.max(d.real_size)),
            None => (0, 0),
        }
    };

    Ok(Entry {
        record_no,
        parent_record: Some(name_info.parent_record),
        name_units,
        is_directory,
        logical,
        allocated,
        modified_ms,
        nlink: link_count,
    })
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn u64_at(buf: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(b)
}

fn i64_at(buf: &[u8], off: usize) -> i64 {
    u64_at(buf, off) as i64
}

fn u16_at(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixups_apply_and_detect_mismatch() {
        // Two-sector record with one fixup pair.
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        // USA at 0x30, count 3 => USN + 2 pairs.
        rec[0x30] = 0x28;
        rec[0x31] = 0x00;
        rec[0x32] = 0x03;
        rec[0x33] = 0x00;
        // USN value 0xABCD.
        rec[0x28] = 0xCD;
        rec[0x29] = 0xAB;
        // Sector 1 tail carries the USN; saved values 0x1234.
        rec[510] = 0xCD;
        rec[511] = 0xAB;
        rec[0x2A] = 0x34;
        rec[0x2B] = 0x12;
        // Sector 2 tail likewise; saved values 0x5678.
        rec[1022] = 0xCD;
        rec[1023] = 0xAB;
        rec[0x2C] = 0x78;
        rec[0x2D] = 0x56;

        assert!(apply_fixups(&mut rec).is_ok());
        assert_eq!(rec[510], 0x34);
        assert_eq!(rec[511], 0x12);
        assert_eq!(rec[1022], 0x78);
        assert_eq!(rec[1023], 0x56);

        // Corrupting the tail triggers a mismatch.
        rec[1022] = 0x99;
        assert_eq!(apply_fixups(&mut rec).unwrap_err(), MftError::FixupMismatch);
    }

    #[test]
    fn truncated_fixup_array_is_out_of_bounds() {
        let mut rec = vec![0u8; 512];
        rec[0x30] = 0xFF;
        rec[0x31] = 0xFF; // USA offset beyond record
        rec[0x32] = 0x05;
        rec[0x33] = 0x00;
        assert_eq!(apply_fixups(&mut rec).unwrap_err(), MftError::OutOfBounds);
    }

    #[test]
    fn attribute_walk_stops_on_bad_length() {
        let mut rec = vec![0xFF; 512];
        rec[0x14] = 0x40; // attributes start at 0x40
        rec[0x15] = 0x00;
        // First attribute: valid type, absurd length.
        rec[0x40] = 0x10; // type lo byte ($STANDARD_INFORMATION)
        rec[0x44] = 0xFF; // length = huge
        rec[0x45] = 0xFF;
        rec[0x46] = 0xFF;
        rec[0x47] = 0xFF;
        assert_eq!(attributes(&rec).count(), 0);
    }

    #[test]
    fn attribute_walk_yields_wellformed_chain() {
        let mut rec = vec![0u8; 512];
        rec[0x14] = 0x40;
        rec[0x15] = 0x00;
        // Attribute A at 0x40, length 0x60.
        rec[0x40] = 0x10;
        rec[0x44] = 0x60;
        // End marker at 0xA0.
        rec[0xA0] = 0xFF;
        rec[0xA1] = 0xFF;
        rec[0xA2] = 0xFF;
        rec[0xA3] = 0xFF;
        let found: Vec<usize> = attributes(&rec).map(|a| a.len()).collect();
        assert_eq!(found, vec![0x60]);
    }

    #[test]
    fn run_lists_decode_signed_offsets_and_terminator() {
        // Extent 1: length 8 (1 byte), offset +16 (1 byte).
        // Extent 2: length 4, offset -12 (signed).
        // Terminator zero byte.
        let runs = [0x11, 0x08, 0x10, 0x11, 0x04, 0xF4, 0x00];
        let ext = decode_runs(&runs).unwrap();
        assert_eq!(ext, vec![(16, 8), (4, 4)]);
    }

    #[test]
    fn sparse_extents_are_skipped() {
        // Extent 1: header 0x01 => length only, no offset field: sparse.
        // Extent 2: header 0x11, length 8 clusters at LCN +=32.
        let runs = [0x01, 0x10, 0x11, 0x08, 0x20, 0x00];
        let ext = decode_runs(&runs).unwrap();
        assert_eq!(ext, vec![(32, 8)]);
    }

    #[test]
    fn lying_run_widths_are_rejected() {
        // Low nibble zero means an impossible zero-width length field.
        assert_eq!(
            decode_runs(&[0x10, 0x08]).unwrap_err(),
            MftError::MalformedAttribute
        );
        // Truncated length field.
        assert_eq!(
            decode_runs(&[0x22, 0x01]).unwrap_err(),
            MftError::OutOfBounds
        );
        // A leading terminator is simply an empty list.
        assert_eq!(decode_runs(&[0x00]), Ok(Vec::new()));
    }

    #[test]
    fn file_name_parses_parent_and_name_bounds() {
        // Build a FILE_NAME body: 66-byte fixed part + UTF-16 name.
        let mut body = vec![0u8; 66 + 6];
        let parent_ref: u64 = (5u64) | (0x1234u64 << 48); // root, seq 0x1234
        body[0..8].copy_from_slice(&parent_ref.to_le_bytes());
        body[64] = 3; // name length in code units
        body[65] = 1; // Win32 namespace
        body[66..72].copy_from_slice(
            &"abc"
                .encode_utf16()
                .collect::<Vec<_>>()
                .iter()
                .map(|u| u.to_le_bytes())
                .collect::<Vec<_>>()
                .concat(),
        );

        let info = parse_file_name(&body).unwrap();
        assert_eq!(info.parent_record, 5);
        assert_eq!(info.parent_sequence, 0x1234);
        assert_eq!(info.name_units, 3);
        assert_eq!(info.namespace, 1);

        // Claim more name units than the body holds.
        let mut bad = body.clone();
        bad[64] = 200;
        assert_eq!(parse_file_name(&bad).unwrap_err(), MftError::OutOfBounds);
    }

    #[test]
    fn short_bodies_are_out_of_bounds() {
        assert_eq!(
            parse_file_name(&[0u8; 10]).unwrap_err(),
            MftError::OutOfBounds
        );
    }

    #[test]
    fn data_attrs_report_real_and_allocated_sizes() {
        // Resident data of 100 bytes.
        let mut attr = vec![0u8; 0x20];
        attr[8] = 0; // resident
        attr[0x10] = 100;
        let d = parse_data_attr(&attr).unwrap();
        assert!(d.resident);
        assert_eq!(d.real_size, 100);
        assert_eq!(d.allocated_size, 104);

        // Non-resident with sizes and a run offset.
        let mut nr = vec![0u8; 0x40];
        nr[8] = 1;
        nr[0x20] = 0x38; // runs begin at 0x38
        nr[0x28..0x30].copy_from_slice(&8192u64.to_le_bytes());
        nr[0x30..0x38].copy_from_slice(&5000u64.to_le_bytes());
        let d = parse_data_attr(&nr).unwrap();
        assert!(!d.resident);
        assert_eq!(d.real_size, 5000);
        assert_eq!(d.allocated_size, 8192);

        // Run offset pointing outside the attribute.
        let mut bad = nr.clone();
        bad[0x20] = 0xFF;
        bad[0x21] = 0xFF;
        assert_eq!(parse_data_attr(&bad).unwrap_err(), MftError::OutOfBounds);
    }
}

/// Volume access and MFT streaming (Windows only).
///
/// Everything above this point is pure parsing; this shell is the only
/// part that touches raw volume handles. It requires administrator
/// rights; any failure returns `Err` and the caller falls back to normal
/// directory traversal.
/// Volume access and MFT streaming (Windows only).
///
/// Everything above this point is pure parsing; this shell is the only
/// part that touches raw volume handles. It requires administrator
/// rights; any failure returns `Err` and the caller falls back to normal
/// directory traversal.
#[cfg(target_os = "windows")]
pub mod reader {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_BEGIN, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING, ReadFile, SetFilePointerEx,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    use super::{
        ATTR_TYPE_DATA, META_RECORDS, ROOT_RECORD, apply_fixups, attributes, build_model,
        decode_runs, extract_entry, parse_data_attr,
    };
    use crate::model::ScanModel;

    const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x0009_0026;
    /// Bytes read from the volume per I/O.
    const READ_CHUNK: usize = 8 * 1024 * 1024;

    /// Subset of `NTFS_VOLUME_DATA_BUFFER` the scanner needs.
    #[derive(Clone, Copy)]
    struct VolumeData {
        serial: u64,
        bytes_per_cluster: u64,
        bytes_per_record: u64,
        mft_start_lcn: u64,
        mft_valid_length: u64,
        free_clusters: u64,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn io_err() -> std::io::Error {
        std::io::Error::last_os_error()
    }

    /// `"C:\\"` or `"C:"` -> `Some("C:")`, anything else `None`.
    pub fn as_drive_root(p: &Path) -> Option<String> {
        let s = p.to_str()?;
        let b = s.as_bytes();
        if b.len() >= 2
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && b[2..].iter().all(|c| *c == b'\\' || *c == b'/')
        {
            Some((b[0] as char).to_ascii_uppercase().to_string())
        } else {
            None
        }
    }

    pub fn try_volume_scan(root: &Path, cancel: &AtomicBool) -> Result<ScanModel, String> {
        let drive = as_drive_root(root).ok_or("not a drive root")?;
        let path = wide(&format!(r"\\.\{drive}"));
        // SAFETY: path pointer valid for the call; read-only access with
        // maximum sharing so nothing else is disturbed.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            let err = io_err();
            return Err(format!(
                "cannot open volume {drive} (administrator rights required): {err}"
            ));
        }
        let result = scan_with_handle(handle, root, cancel);
        // SAFETY: created above, closed exactly once.
        unsafe { CloseHandle(handle) };
        result
    }

    fn seek_to(handle: HANDLE, byte_offset: u64) -> Result<(), String> {
        let mut new_pos: i64 = 0;
        // SAFETY: valid handle; out param is a valid pointer.
        let ok = unsafe { SetFilePointerEx(handle, byte_offset as i64, &mut new_pos, FILE_BEGIN) };
        if ok == 0 {
            let err = io_err();
            return Err(format!("seek failed: {err}"));
        }
        Ok(())
    }

    fn read_exact(handle: HANDLE, buf: &mut [u8]) -> Result<(), String> {
        let mut done = 0usize;
        while done < buf.len() {
            let want = (buf.len() - done).min(u32::MAX as usize) as u32;
            let mut got: u32 = 0;
            // SAFETY: buffer slice is valid for `want` bytes at `done`.
            let ok = unsafe {
                ReadFile(
                    handle,
                    buf[done..].as_mut_ptr(),
                    want,
                    &mut got,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let err = io_err();
                return Err(format!("volume read failed: {err}"));
            }
            if got == 0 {
                return Err("unexpected end of volume".into());
            }
            done += got as usize;
        }
        Ok(())
    }

    fn query_volume_data(handle: HANDLE) -> Result<VolumeData, String> {
        let mut out = [0u8; 112];
        let mut returned: u32 = 0;
        // SAFETY: output buffer sized for NTFS_VOLUME_DATA_BUFFER.
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_GET_NTFS_VOLUME_DATA,
                std::ptr::null(),
                0,
                out.as_mut_ptr().cast(),
                out.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = io_err();
            return Err(format!("FSCTL_GET_NTFS_VOLUME_DATA failed: {err}"));
        }
        let le_u64 = |o: usize| u64::from_le_bytes(out[o..o + 8].try_into().unwrap());
        let le_u32 = |o: usize| u32::from_le_bytes(out[o..o + 4].try_into().unwrap());
        Ok(VolumeData {
            serial: le_u64(0),
            free_clusters: le_u64(24),
            mft_valid_length: le_u64(56),
            bytes_per_cluster: le_u32(44) as u64,
            bytes_per_record: le_u32(48) as u64,
            mft_start_lcn: le_u64(64),
        })
    }

    /// Locate `$MFT`'s extents by parsing record 0's unnamed `$DATA`.
    fn mft_extents(handle: HANDLE, vd: &VolumeData) -> Result<Vec<(u64, u64)>, String> {
        let offset = vd
            .mft_start_lcn
            .checked_mul(vd.bytes_per_cluster)
            .ok_or("MFT location overflows")?;
        seek_to(handle, offset)?;
        let mut rec = vec![0u8; vd.bytes_per_record.max(1024) as usize];
        read_exact(handle, &mut rec)?;
        if rec.len() < 4 || &rec[0..4] != b"FILE" {
            return Err("MFT record 0 lacks the FILE magic".into());
        }
        apply_fixups(&mut rec).map_err(|e| format!("MFT record 0 fixup failed: {e:?}"))?;

        for attr in attributes(&rec) {
            if attr.len() >= 0x10
                && u32_at(attr, 0) == ATTR_TYPE_DATA
                && attr.get(6).copied().unwrap_or(1) == 0 // unnamed stream
                && attr.get(8).copied().unwrap_or(0) != 0
            // non-resident
            {
                let info = parse_data_attr(attr).map_err(|e| format!("{e:?}"))?;
                let runs_end = (info.runs_offset + info.runs_len).min(attr.len());
                return decode_runs(&attr[info.runs_offset..runs_end])
                    .map_err(|e| format!("$MFT run list malformed: {e:?}"));
            }
        }
        Err("$MFT record has no non-resident data attribute".into())
    }

    fn scan_with_handle(
        handle: HANDLE,
        root: &Path,
        cancel: &AtomicBool,
    ) -> Result<ScanModel, String> {
        let vd = query_volume_data(handle)?;
        let extents = mft_extents(handle, &vd)?;
        let rs = vd.bytes_per_record.max(128) as usize;

        let mut entries: Vec<super::Entry> = Vec::with_capacity(64 * 1024);
        let mut carry: Vec<u8> = Vec::new();
        let mut next_record_no: u64 = 0;

        for (lcn, clusters) in extents {
            let base = lcn
                .checked_mul(vd.bytes_per_cluster)
                .ok_or("extent LCN overflows")?;
            let len = clusters
                .checked_mul(vd.bytes_per_cluster)
                .ok_or("extent size overflows")?;
            seek_to(handle, base)?;
            let mut chunk = vec![0u8; READ_CHUNK];
            let mut done: u64 = 0;
            while done < len {
                if cancel.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }
                let take = (len - done).min(chunk.len() as u64) as usize;
                read_exact(handle, &mut chunk[..take])?;
                carry.extend_from_slice(&chunk[..take]);
                done += take as u64;

                // Consume whole records out of the carry buffer. Every MFT
                // slot counts toward its record number even when skipped,
                // because parent references are slot numbers.
                while carry.len() >= rs {
                    let mut record: Vec<u8> = carry.drain(..rs).collect();
                    let record_no = next_record_no;
                    next_record_no += 1;
                    consume_record(record_no, &mut record, &mut entries);
                }
                if next_record_no * rs as u64 >= vd.mft_valid_length {
                    break;
                }
            }
        }
        drop(carry);

        let mut model =
            build_model(root, vd.serial, &entries).ok_or("MFT contains no root directory")?;
        model.free_space = Some(vd.free_clusters.saturating_mul(vd.bytes_per_cluster));
        Ok(model)
    }

    fn consume_record(record_no: u64, record: &mut [u8], entries: &mut Vec<super::Entry>) {
        if record.len() < 4 || &record[0..4] != b"FILE" {
            return;
        }
        if !super::record_in_use(record) {
            return;
        }
        if (record_no >= META_RECORDS || record_no == ROOT_RECORD)
            && let Ok(entry) = extract_entry(record_no, record)
        {
            entries.push(entry);
        }
    }

    fn u32_at(buf: &[u8], off: usize) -> u32 {
        u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
    }
}
