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
    /// An extent map is truncated, out of order, or incomplete.
    IncompleteExtentMap,
}

const RECORD_HEADER_MIN_LEN: usize = 0x18;
const ATTR_HEADER_RESIDENT_LEN: usize = 0x18;

pub const ATTR_TYPE_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_TYPE_FILE_NAME: u32 = 0x30;
pub const ATTR_TYPE_DATA: u32 = 0x80;
pub const ATTR_TYPE_INDEX_ROOT: u32 = 0x90;

/// Record flags (`u16` at offset 0x16 in a FILE record header).
pub const RECORD_FLAG_IN_USE: u16 = 0x0001;
pub const RECORD_FLAG_DIRECTORY: u16 = 0x0002;

/// Apply the multi-sector header (update sequence array / fixups) in place.
///
/// In a standard NTFS `MULTI_SECTOR_HEADER`:
/// - 0x00..0x04: signature ("FILE")
/// - 0x04..0x06: UpdateSequenceArrayOffset (USA offset)
/// - 0x06..0x08: UpdateSequenceArraySize (USA count in 2-byte units)
///
/// Every 512-byte sector's trailing two bytes were replaced with a check
/// value (USN) when written to disk; the original bytes live in the USA
/// array immediately following the USN. Returns `FixupMismatch` if any
/// sector tail disagrees.
pub fn apply_fixups(record: &mut [u8]) -> Result<(), MftError> {
    if record.len() < RECORD_HEADER_MIN_LEN {
        return Err(MftError::OutOfBounds);
    }
    let usa_offset = u16_at(record, 0x04) as usize;
    let usa_count = u16_at(record, 0x06) as usize;
    if usa_count == 0 {
        return Err(MftError::OutOfBounds);
    }
    let usa_bytes = usa_count.checked_mul(2).ok_or(MftError::OutOfBounds)?;
    if usa_offset
        .checked_add(usa_bytes)
        .is_none_or(|end| end > record.len())
    {
        return Err(MftError::OutOfBounds);
    }
    let sector = 512usize;
    if !record.len().is_multiple_of(sector) {
        return Err(MftError::OutOfBounds);
    }
    let sectors = record.len() / sector;
    if usa_count < 1 + sectors {
        return Err(MftError::OutOfBounds);
    }
    let usn = [record[usa_offset], record[usa_offset + 1]];
    for i in 1..=sectors {
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
    record.len() >= RECORD_HEADER_MIN_LEN && flags_of(record) & RECORD_FLAG_IN_USE != 0
}

/// Whether the record describes a directory (from the header flags).
pub fn record_is_directory(record: &[u8]) -> bool {
    record.len() >= RECORD_HEADER_MIN_LEN && flags_of(record) & RECORD_FLAG_DIRECTORY != 0
}

pub fn flags_of(record: &[u8]) -> u16 {
    if record.len() >= RECORD_HEADER_MIN_LEN {
        u16_at(record, 0x16)
    } else {
        0
    }
}

/// Iterate raw attribute records inside an MFT record.
///
/// Yields subslices; malformed lengths terminate the iteration instead of
/// panicking or running past the buffer.
pub fn attributes(record: &[u8]) -> impl Iterator<Item = &[u8]> {
    let start = if record.len() >= RECORD_HEADER_MIN_LEN {
        u16_at(record, 0x14) as usize
    } else {
        0
    };
    let mut off = start;
    std::iter::from_fn(move || {
        if off < RECORD_HEADER_MIN_LEN || off + 8 > record.len() {
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

/// Extract the value slice from a resident attribute record.
///
/// Verifies that the attribute is resident (byte 0x08 == 0) and uses the
/// recorded `ValueOffset` (0x14) and `ValueLength` (0x10) fields to return
/// the exact value slice without assuming a hardcoded start offset.
pub fn resident_value(attr: &[u8]) -> Result<&[u8], MftError> {
    if attr.len() < ATTR_HEADER_RESIDENT_LEN {
        return Err(MftError::OutOfBounds);
    }
    if attr[8] != 0 {
        return Err(MftError::MalformedAttribute);
    }
    let val_len = u32_at(attr, 0x10) as usize;
    let val_off = u16_at(attr, 0x14) as usize;
    let end = val_off.checked_add(val_len).ok_or(MftError::OutOfBounds)?;
    if end > attr.len() {
        return Err(MftError::OutOfBounds);
    }
    Ok(&attr[val_off..end])
}

/// Whether an attribute is the unnamed default stream (name length at 0x09 is 0).
pub fn attribute_is_unnamed(attr: &[u8]) -> bool {
    attr.len() >= 0x0A && attr[9] == 0
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

/// Parse a `$FILE_NAME` attribute body (the bytes of the resident value).
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
        let val = resident_value(attr)?;
        Ok(DataInfo {
            resident: true,
            real_size: val.len() as u64,
            allocated_size: 0,
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
                if let Ok(body) = resident_value(attr)
                    && body.len() >= 0x10
                {
                    let ft = i64_at(body, 0x08);
                    modified_ms = filetime_to_unix_ms(ft);
                }
            }
            ATTR_TYPE_FILE_NAME => {
                if let Ok(body) = resident_value(attr) {
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
            }
            ATTR_TYPE_DATA => {
                // Only the unnamed stream is the file's content.
                if attribute_is_unnamed(attr) && data.is_none() {
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
            if u32_at(attr, 0) != ATTR_TYPE_FILE_NAME {
                continue;
            }
            if let Ok(body) = resident_value(attr)
                && let Ok(info) = parse_file_name(body)
                && info.name_units == name_info.name_units
                && info.namespace == name_info.namespace
                && info.parent_record == name_info.parent_record
            {
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
            Some(d) => (d.real_size, d.allocated_size),
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

/// Streaming parser for MFT records across arbitrary chunk buffers.
///
/// Processes complete records in-place without copying or shifting buffer
/// memory. Maintains carry only for partial records spanning chunk boundaries.
pub struct MftStreamParser {
    record_size: usize,
    mft_valid_length: u64,
    next_record_no: u64,
    carry: Vec<u8>,
    entries: Vec<Entry>,
}

impl MftStreamParser {
    pub fn new(record_size: usize, mft_valid_length: u64) -> Self {
        let rs = record_size.max(128);
        Self {
            record_size: rs,
            mft_valid_length,
            next_record_no: 0,
            carry: Vec::with_capacity(rs),
            entries: Vec::with_capacity(64 * 1024),
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn into_entries(self) -> Vec<Entry> {
        self.entries
    }

    pub fn next_record_no(&self) -> u64 {
        self.next_record_no
    }

    /// Process a mutable chunk of MFT bytes.
    ///
    /// Fixups and attribute parsing happen in place within `chunk`.
    /// Returns `Err("cancelled")` if `cancel` is set.
    pub fn process_chunk(
        &mut self,
        mut chunk: &mut [u8],
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<bool, String> {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        let rs = self.record_size;

        // If we have leftover bytes from a previous chunk, complete the record first.
        if !self.carry.is_empty() {
            let need = rs - self.carry.len();
            if chunk.len() < need {
                self.carry.extend_from_slice(chunk);
                return Ok(true);
            }
            let (head, rest) = chunk.split_at_mut(need);
            self.carry.extend_from_slice(head);
            let record_no = self.next_record_no;
            self.next_record_no += 1;
            consume_record(record_no, &mut self.carry, &mut self.entries);
            self.carry.clear();
            chunk = rest;
        }

        // Process full records directly from `chunk` using in-place slices.
        let mut chunks = chunk.chunks_exact_mut(rs);
        for record_slice in &mut chunks {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("cancelled".into());
            }
            if self.next_record_no * (rs as u64) >= self.mft_valid_length {
                return Ok(false);
            }
            let record_no = self.next_record_no;
            self.next_record_no += 1;
            consume_record(record_no, record_slice, &mut self.entries);
        }

        // Preserve trailing incomplete record as carry for next chunk.
        let remainder = chunks.into_remainder();
        if !remainder.is_empty() && self.next_record_no * (rs as u64) < self.mft_valid_length {
            self.carry.extend_from_slice(remainder);
        }

        let keep_going = self.next_record_no * (rs as u64) < self.mft_valid_length;
        Ok(keep_going)
    }
}

pub fn consume_record(record_no: u64, record: &mut [u8], entries: &mut Vec<Entry>) {
    if record.len() < 4 || &record[0..4] != b"FILE" {
        return;
    }
    if !record_in_use(record) {
        return;
    }
    if (record_no >= META_RECORDS || record_no == ROOT_RECORD)
        && let Ok(entry) = extract_entry(record_no, record)
    {
        entries.push(entry);
    }
}

/// Create a synthetic 1024-byte MFT record for testing and benchmarks.
///
/// Follows standard Microsoft NTFS on-disk layouts:
/// - 0x00..0x04: "FILE"
/// - 0x04..0x06: UpdateSequenceArrayOffset = 0x30
/// - 0x06..0x08: UpdateSequenceArraySize = 3 (1 USN + 2 sector fixup entries)
/// - 0x14..0x16: FirstAttributeOffset = 0x38
/// - 0x16..0x18: Flags (IN_USE | DIRECTORY)
/// - 0x30..0x36: USA array
/// - 0x38..: Attributes ($STANDARD_INFORMATION, $FILE_NAME, $DATA)
pub fn create_synthetic_record(
    _record_no: u64,
    parent_record: u64,
    name: &str,
    is_dir: bool,
    real_size: u64,
    allocated_size: u64,
) -> Vec<u8> {
    let mut rec = vec![0u8; 1024];
    rec[0..4].copy_from_slice(b"FILE");
    // USA offset at 0x04: 0x30, USA count at 0x06: 3
    rec[0x04..0x06].copy_from_slice(&0x30u16.to_le_bytes());
    rec[0x06..0x08].copy_from_slice(&3u16.to_le_bytes());
    // First attribute offset at 0x14: 0x38
    rec[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
    // Flags at 0x16
    let flags: u16 = RECORD_FLAG_IN_USE | if is_dir { RECORD_FLAG_DIRECTORY } else { 0 };
    rec[0x16..0x18].copy_from_slice(&flags.to_le_bytes());

    let mut off = 0x38usize;

    // Attr 1: $STANDARD_INFORMATION (0x10)
    let std_val_len = 0x30u32;
    let std_val_off = 0x18u16;
    let std_len = 0x48u32;
    rec[off..off + 4].copy_from_slice(&ATTR_TYPE_STANDARD_INFORMATION.to_le_bytes());
    rec[off + 4..off + 8].copy_from_slice(&std_len.to_le_bytes());
    rec[off + 8] = 0; // resident
    rec[off + 9] = 0; // unnamed
    rec[off + 0x10..off + 0x14].copy_from_slice(&std_val_len.to_le_bytes());
    rec[off + 0x14..off + 0x16].copy_from_slice(&std_val_off.to_le_bytes());
    let std_val_start = off + std_val_off as usize;
    let mtime: i64 = 133500000000000000;
    rec[std_val_start + 0x08..std_val_start + 0x10].copy_from_slice(&mtime.to_le_bytes());
    off += std_len as usize;

    // Attr 2: $FILE_NAME (0x30)
    let name_utf16: Vec<u16> = name.encode_utf16().collect();
    let name_units = name_utf16.len() as u8;
    let name_bytes = name_units as usize * 2;
    let body_len = 66 + name_bytes;
    let fn_val_off = 0x18u16;
    let fn_attr_len = (0x18 + body_len).next_multiple_of(8);
    rec[off..off + 4].copy_from_slice(&ATTR_TYPE_FILE_NAME.to_le_bytes());
    rec[off + 4..off + 8].copy_from_slice(&(fn_attr_len as u32).to_le_bytes());
    rec[off + 8] = 0; // resident
    rec[off + 9] = 0; // unnamed
    rec[off + 0x10..off + 0x14].copy_from_slice(&(body_len as u32).to_le_bytes());
    rec[off + 0x14..off + 0x16].copy_from_slice(&fn_val_off.to_le_bytes());
    let fn_body = off + fn_val_off as usize;
    let parent_ref: u64 = parent_record | (1u64 << 48);
    rec[fn_body..fn_body + 8].copy_from_slice(&parent_ref.to_le_bytes());
    rec[fn_body + 64] = name_units;
    rec[fn_body + 65] = 1; // Win32 namespace
    for (i, u) in name_utf16.iter().enumerate() {
        rec[fn_body + 66 + i * 2..fn_body + 68 + i * 2].copy_from_slice(&u.to_le_bytes());
    }
    off += fn_attr_len;

    // Attr 3: $DATA (0x80)
    if !is_dir {
        if allocated_size == 0 && real_size <= 64 {
            let data_attr_len = (0x18 + real_size as usize).next_multiple_of(8).max(0x20);
            rec[off..off + 4].copy_from_slice(&ATTR_TYPE_DATA.to_le_bytes());
            rec[off + 4..off + 8].copy_from_slice(&(data_attr_len as u32).to_le_bytes());
            rec[off + 8] = 0; // resident
            rec[off + 9] = 0; // unnamed
            rec[off + 0x10..off + 0x14].copy_from_slice(&(real_size as u32).to_le_bytes());
            rec[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
            off += data_attr_len;
        } else {
            let runs_bytes = [0x11, 0x01, 0x10, 0x00];
            let runs_off = 0x40u16;
            let data_attr_len = (runs_off as usize + runs_bytes.len())
                .next_multiple_of(8)
                .max(0x48);
            rec[off..off + 4].copy_from_slice(&ATTR_TYPE_DATA.to_le_bytes());
            rec[off + 4..off + 8].copy_from_slice(&(data_attr_len as u32).to_le_bytes());
            rec[off + 8] = 1; // non-resident
            rec[off + 9] = 0; // unnamed
            rec[off + 0x20..off + 0x22].copy_from_slice(&runs_off.to_le_bytes());
            rec[off + 0x28..off + 0x30].copy_from_slice(&allocated_size.to_le_bytes());
            rec[off + 0x30..off + 0x38].copy_from_slice(&real_size.to_le_bytes());
            rec[off + 0x38..off + 0x40].copy_from_slice(&real_size.to_le_bytes());
            rec[off + runs_off as usize..off + runs_off as usize + runs_bytes.len()]
                .copy_from_slice(&runs_bytes);
            off += data_attr_len;
        }
    }

    if off + 4 <= 1024 {
        rec[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    }

    // USA array at 0x30..0x36: USN [0x42, 0x42]
    rec[0x30] = 0x42;
    rec[0x31] = 0x42;
    rec[0x32] = rec[510];
    rec[0x33] = rec[511];
    rec[0x34] = rec[1022];
    rec[0x35] = rec[1023];

    rec[510] = 0x42;
    rec[511] = 0x42;
    rec[1022] = 0x42;
    rec[1023] = 0x42;

    rec
}

/// An extent in an MFT stream mapping (VCN range to optional LCN).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MftExtent {
    pub vcn_start: u64,
    pub next_vcn: u64,
    pub lcn: Option<u64>,
}

/// Parse a raw `RETRIEVAL_POINTERS_BUFFER` received from `FSCTL_GET_RETRIEVAL_POINTERS`.
///
/// Windows layout:
/// - 0x00..0x04: `ExtentCount` (`u32`)
/// - 0x04..0x08: padding / unused (`u32`)
/// - 0x08..0x10: `StartingVcn` (`LARGE_INTEGER` as `i64` / `u64`)
/// - Array of extents starting at 0x10, each 16 bytes:
///   - 0x00..0x08: `NextVcn` (`i64` / `u64`)
///   - 0x08..0x10: `Lcn` (`i64` / `u64`, -1 / `u64::MAX` indicates unmapped / sparse)
///
/// Returns `(next_starting_vcn, extents)` on success.
pub fn parse_retrieval_pointers_buffer(
    buf: &[u8],
    expected_starting_vcn: u64,
) -> Result<(u64, Vec<MftExtent>), MftError> {
    if buf.len() < 16 {
        return Err(MftError::OutOfBounds);
    }
    let extent_count = u32_at(buf, 0) as usize;
    if extent_count == 0 {
        return Err(MftError::IncompleteExtentMap);
    }
    let starting_vcn = u64_at(buf, 8);
    if starting_vcn != expected_starting_vcn {
        return Err(MftError::IncompleteExtentMap);
    }
    let required_len = 16usize
        .checked_add(extent_count.checked_mul(16).ok_or(MftError::OutOfBounds)?)
        .ok_or(MftError::OutOfBounds)?;
    if buf.len() < required_len {
        return Err(MftError::OutOfBounds);
    }

    let mut extents = Vec::with_capacity(extent_count);
    let mut current_vcn = starting_vcn;

    for i in 0..extent_count {
        let off = 16 + i * 16;
        let next_vcn = u64_at(buf, off);
        let lcn_raw = u64_at(buf, off + 8);

        if next_vcn <= current_vcn {
            return Err(MftError::IncompleteExtentMap);
        }

        let lcn = if lcn_raw == u64::MAX || lcn_raw as i64 == -1 {
            None
        } else {
            Some(lcn_raw)
        };

        extents.push(MftExtent {
            vcn_start: current_vcn,
            next_vcn,
            lcn,
        });
        current_vcn = next_vcn;
    }

    Ok((current_vcn, extents))
}

/// Convert `(lcn, cluster_count)` pairs from `decode_runs` into contiguous `MftExtent`s.
pub fn runs_to_extents(runs: &[(u64, u64)]) -> Result<Vec<MftExtent>, MftError> {
    let mut extents = Vec::with_capacity(runs.len());
    let mut current_vcn = 0u64;
    for &(lcn, clusters) in runs {
        if clusters == 0 {
            return Err(MftError::MalformedAttribute);
        }
        let next_vcn = current_vcn
            .checked_add(clusters)
            .ok_or(MftError::OutOfBounds)?;
        extents.push(MftExtent {
            vcn_start: current_vcn,
            next_vcn,
            lcn: Some(lcn),
        });
        current_vcn = next_vcn;
    }
    Ok(extents)
}

/// Validate an extent map for VCN continuity, volume boundary safety, and coverage.
///
/// Ensures:
/// - Map starts at VCN 0.
/// - Every extent has strictly positive length (`next_vcn > vcn_start`).
/// - Extents are contiguous without gaps or overlapping VCN ranges.
/// - Allocated LCN ranges do not overflow and fit within `total_volume_clusters`.
/// - Total VCN coverage in bytes meets or exceeds `mft_valid_length`.
///
/// Returns the physical `(lcn, cluster_count)` extents for volume reads.
pub fn validate_and_convert_extents(
    extents: &[MftExtent],
    mft_valid_length: u64,
    bytes_per_cluster: u64,
    total_volume_clusters: u64,
) -> Result<Vec<(u64, u64)>, MftError> {
    if extents.is_empty() || bytes_per_cluster == 0 {
        return Err(MftError::IncompleteExtentMap);
    }
    if extents[0].vcn_start != 0 {
        return Err(MftError::IncompleteExtentMap);
    }

    let mut prev_next_vcn = 0u64;
    let mut physical_extents = Vec::with_capacity(extents.len());

    for (i, extent) in extents.iter().enumerate() {
        if extent.next_vcn <= extent.vcn_start {
            return Err(MftError::IncompleteExtentMap);
        }
        if i > 0 && extent.vcn_start != prev_next_vcn {
            return Err(MftError::IncompleteExtentMap);
        }
        prev_next_vcn = extent.next_vcn;

        let clusters = extent.next_vcn - extent.vcn_start;

        if let Some(lcn) = extent.lcn {
            let end_lcn = lcn.checked_add(clusters).ok_or(MftError::OutOfBounds)?;
            if total_volume_clusters > 0 && end_lcn > total_volume_clusters {
                return Err(MftError::OutOfBounds);
            }
            physical_extents.push((lcn, clusters));
        }
    }

    let total_clusters = prev_next_vcn;
    let total_bytes = total_clusters
        .checked_mul(bytes_per_cluster)
        .ok_or(MftError::OutOfBounds)?;
    if total_bytes < mft_valid_length {
        return Err(MftError::IncompleteExtentMap);
    }

    Ok(physical_extents)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    #[test]
    fn fixups_apply_and_detect_mismatch() {
        // Two-sector record with one fixup pair.
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        // USA pointer at 0x04 (offset 0x28), USA count at 0x06 (count 3 => USN + 2 sectors).
        rec[0x04..0x06].copy_from_slice(&0x28u16.to_le_bytes());
        rec[0x06..0x08].copy_from_slice(&3u16.to_le_bytes());
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
    fn usa_offsets_and_flags_at_spec_locations_not_legacy_offsets() {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        // Real USA header fields at 0x04 and 0x06
        rec[0x04..0x06].copy_from_slice(&0x28u16.to_le_bytes());
        rec[0x06..0x08].copy_from_slice(&3u16.to_le_bytes());
        // Real flags at 0x16
        rec[0x16..0x18]
            .copy_from_slice(&(RECORD_FLAG_IN_USE | RECORD_FLAG_DIRECTORY).to_le_bytes());

        // Bogus values at wrong legacy locations (0x30/0x32/0x38)
        rec[0x30..0x32].copy_from_slice(&0xDEADu16.to_le_bytes());
        rec[0x32..0x34].copy_from_slice(&0xBEEFu16.to_le_bytes());
        rec[0x38..0x3A].copy_from_slice(&0xCAFEu16.to_le_bytes());

        // Set up USN at real USA location (0x28)
        rec[0x28] = 0x42;
        rec[0x29] = 0x42;
        rec[0x2A] = 0x11;
        rec[0x2B] = 0x22;
        rec[0x2C] = 0x33;
        rec[0x2D] = 0x44;
        rec[510] = 0x42;
        rec[511] = 0x42;
        rec[1022] = 0x42;
        rec[1023] = 0x42;

        assert!(apply_fixups(&mut rec).is_ok());
        assert!(record_in_use(&rec));
        assert!(record_is_directory(&rec));
    }

    #[test]
    fn flags_only_read_from_0x16() {
        let mut rec = vec![0u8; 512];
        // Flags at 0x16 is 0, bogus flag at 0x38 is IN_USE
        rec[0x16..0x18].copy_from_slice(&0u16.to_le_bytes());
        rec[0x38..0x3A].copy_from_slice(&RECORD_FLAG_IN_USE.to_le_bytes());
        assert!(!record_in_use(&rec));

        // Now set real flags at 0x16
        rec[0x16..0x18].copy_from_slice(&RECORD_FLAG_IN_USE.to_le_bytes());
        rec[0x38..0x3A].copy_from_slice(&0u16.to_le_bytes());
        assert!(record_in_use(&rec));
        assert!(!record_is_directory(&rec));

        rec[0x16..0x18]
            .copy_from_slice(&(RECORD_FLAG_IN_USE | RECORD_FLAG_DIRECTORY).to_le_bytes());
        assert!(record_is_directory(&rec));
    }

    #[test]
    fn resident_value_offset_resolution() {
        // Standard attribute with ValueOffset = 0x18
        let mut attr_std = vec![0u8; 0x28];
        attr_std[0..4].copy_from_slice(&ATTR_TYPE_DATA.to_le_bytes());
        attr_std[4..8].copy_from_slice(&0x28u32.to_le_bytes());
        attr_std[8] = 0; // resident
        attr_std[0x10..0x14].copy_from_slice(&10u32.to_le_bytes()); // ValueLength = 10
        attr_std[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes()); // ValueOffset = 0x18
        attr_std[0x18..0x22].copy_from_slice(b"0123456789");
        let val = resident_value(&attr_std).unwrap();
        assert_eq!(val, b"0123456789");

        // Custom attribute with non-default ValueOffset = 0x20
        let mut attr_custom = vec![0u8; 0x30];
        attr_custom[0..4].copy_from_slice(&ATTR_TYPE_DATA.to_le_bytes());
        attr_custom[4..8].copy_from_slice(&0x30u32.to_le_bytes());
        attr_custom[8] = 0; // resident
        attr_custom[0x10..0x14].copy_from_slice(&8u32.to_le_bytes()); // ValueLength = 8
        attr_custom[0x14..0x16].copy_from_slice(&0x20u16.to_le_bytes()); // ValueOffset = 0x20
        attr_custom[0x20..0x28].copy_from_slice(b"abcdefgh");
        let val = resident_value(&attr_custom).unwrap();
        assert_eq!(val, b"abcdefgh");

        // Out-of-bounds ValueOffset
        let mut bad_off = attr_std.clone();
        bad_off[0x14..0x16].copy_from_slice(&0x50u16.to_le_bytes());
        assert_eq!(resident_value(&bad_off).unwrap_err(), MftError::OutOfBounds);

        // Out-of-bounds ValueLength
        let mut bad_len = attr_std.clone();
        bad_len[0x10..0x14].copy_from_slice(&500u32.to_le_bytes());
        assert_eq!(resident_value(&bad_len).unwrap_err(), MftError::OutOfBounds);

        // Non-resident attribute passed to resident_value
        let mut non_res = attr_std.clone();
        non_res[8] = 1;
        assert_eq!(
            resident_value(&non_res).unwrap_err(),
            MftError::MalformedAttribute
        );
    }

    #[test]
    fn standard_information_timestamp_from_value_offset() {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        rec[0x04..0x06].copy_from_slice(&0x30u16.to_le_bytes());
        rec[0x06..0x08].copy_from_slice(&3u16.to_le_bytes());
        rec[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        rec[0x16..0x18].copy_from_slice(&RECORD_FLAG_IN_USE.to_le_bytes());

        // Attr 1: $STANDARD_INFORMATION with non-default ValueOffset = 0x20
        let mut off = 0x38usize;
        let std_val_len = 0x30u32;
        let std_val_off = 0x20u16;
        let std_len = 0x50u32;
        rec[off..off + 4].copy_from_slice(&ATTR_TYPE_STANDARD_INFORMATION.to_le_bytes());
        rec[off + 4..off + 8].copy_from_slice(&std_len.to_le_bytes());
        rec[off + 8] = 0; // resident
        rec[off + 0x10..off + 0x14].copy_from_slice(&std_val_len.to_le_bytes());
        rec[off + 0x14..off + 0x16].copy_from_slice(&std_val_off.to_le_bytes());
        let mtime: i64 = 133500000000000000;
        let std_val_start = off + std_val_off as usize;
        rec[std_val_start + 0x08..std_val_start + 0x10].copy_from_slice(&mtime.to_le_bytes());
        off += std_len as usize;

        // Attr 2: $FILE_NAME
        let fn_attr = {
            let fn_rec = create_synthetic_record(16, 5, "test.txt", false, 100, 4096);
            attributes(&fn_rec).nth(1).unwrap().to_vec()
        };
        rec[off..off + fn_attr.len()].copy_from_slice(&fn_attr);
        off += fn_attr.len();

        rec[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

        // Set up USN fixups
        rec[0x30] = 0x42;
        rec[0x31] = 0x42;
        rec[0x32] = rec[510];
        rec[0x33] = rec[511];
        rec[0x34] = rec[1022];
        rec[0x35] = rec[1023];
        rec[510] = 0x42;
        rec[511] = 0x42;
        rec[1022] = 0x42;
        rec[1023] = 0x42;

        let entry = extract_entry(16, &mut rec).unwrap();
        assert_eq!(entry.modified_ms, filetime_to_unix_ms(mtime));
    }

    #[test]
    fn named_alternate_data_stream_ignored() {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        rec[0x04..0x06].copy_from_slice(&0x30u16.to_le_bytes());
        rec[0x06..0x08].copy_from_slice(&3u16.to_le_bytes());
        rec[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        rec[0x16..0x18].copy_from_slice(&RECORD_FLAG_IN_USE.to_le_bytes());

        let mut off = 0x38usize;

        // $STANDARD_INFORMATION
        let std_attr = attributes(&create_synthetic_record(16, 5, "a", false, 0, 0))
            .next()
            .unwrap()
            .to_vec();
        rec[off..off + std_attr.len()].copy_from_slice(&std_attr);
        off += std_attr.len();

        // $FILE_NAME
        let fn_attr = attributes(&create_synthetic_record(16, 5, "file.txt", false, 0, 0))
            .nth(1)
            .unwrap()
            .to_vec();
        rec[off..off + fn_attr.len()].copy_from_slice(&fn_attr);
        off += fn_attr.len();

        // Named $DATA stream (ADS $DATA:Zone.Identifier)
        let ads_val_len = 20u32;
        let ads_len = (0x18 + ads_val_len).next_multiple_of(8);
        rec[off..off + 4].copy_from_slice(&ATTR_TYPE_DATA.to_le_bytes());
        rec[off + 4..off + 8].copy_from_slice(&ads_len.to_le_bytes());
        rec[off + 8] = 0; // resident
        rec[off + 9] = 4; // name_length = 4 (named stream!)
        rec[off + 0x10..off + 0x14].copy_from_slice(&ads_val_len.to_le_bytes());
        rec[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        off += ads_len as usize;

        // Unnamed primary $DATA stream
        let primary_val_len = 50u32;
        let primary_len = (0x18 + primary_val_len).next_multiple_of(8);
        rec[off..off + 4].copy_from_slice(&ATTR_TYPE_DATA.to_le_bytes());
        rec[off + 4..off + 8].copy_from_slice(&primary_len.to_le_bytes());
        rec[off + 8] = 0; // resident
        rec[off + 9] = 0; // name_length = 0 (unnamed stream!)
        rec[off + 0x10..off + 0x14].copy_from_slice(&primary_val_len.to_le_bytes());
        rec[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        off += primary_len as usize;

        rec[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

        // Fixups
        rec[0x30] = 0x42;
        rec[0x31] = 0x42;
        rec[0x32] = rec[510];
        rec[0x33] = rec[511];
        rec[0x34] = rec[1022];
        rec[0x35] = rec[1023];
        rec[510] = 0x42;
        rec[511] = 0x42;
        rec[1022] = 0x42;
        rec[1023] = 0x42;

        let entry = extract_entry(16, &mut rec).unwrap();
        assert_eq!(entry.logical, 50);
    }

    #[test]
    fn realistic_file_record_fixture() {
        // Byte fixture representing a spec-correct NTFS FILE record for "notes.txt":
        // - Magic "FILE"
        // - USA offset at 0x04 = 0x30, USA count at 0x06 = 3
        // - First attribute offset at 0x14 = 0x38
        // - Flags at 0x16 = 0x0001 (IN_USE)
        // - Attributes: $STANDARD_INFORMATION at 0x38, $FILE_NAME at 0x80, $DATA at 0xFA
        let mut fixture = vec![0u8; 1024];
        fixture[0x00..0x04].copy_from_slice(b"FILE");
        fixture[0x04..0x06].copy_from_slice(&0x30u16.to_le_bytes());
        fixture[0x06..0x08].copy_from_slice(&3u16.to_le_bytes());
        fixture[0x08..0x10].copy_from_slice(&0x12345678u64.to_le_bytes()); // LSN
        fixture[0x10..0x12].copy_from_slice(&1u16.to_le_bytes()); // Seq
        fixture[0x12..0x14].copy_from_slice(&1u16.to_le_bytes()); // HardLinkCount
        fixture[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes()); // FirstAttr
        fixture[0x16..0x18].copy_from_slice(&RECORD_FLAG_IN_USE.to_le_bytes()); // Flags

        // USA at 0x30..0x36
        fixture[0x30..0x32].copy_from_slice(&[0x99, 0x88]); // USN
        fixture[0x32..0x34].copy_from_slice(&[0x11, 0x22]); // Sector 1 tail
        fixture[0x34..0x36].copy_from_slice(&[0x33, 0x44]); // Sector 2 tail

        // Attr 1: $STANDARD_INFORMATION at 0x38 (len 0x48)
        let mut off = 0x38usize;
        fixture[off..off + 4].copy_from_slice(&ATTR_TYPE_STANDARD_INFORMATION.to_le_bytes());
        fixture[off + 4..off + 8].copy_from_slice(&0x48u32.to_le_bytes());
        fixture[off + 8] = 0; // resident
        fixture[off + 9] = 0; // unnamed
        fixture[off + 0x10..off + 0x14].copy_from_slice(&0x30u32.to_le_bytes()); // ValueLength
        fixture[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes()); // ValueOffset
        let mtime: i64 = 133500000000000000;
        fixture[off + 0x18 + 0x08..off + 0x18 + 0x10].copy_from_slice(&mtime.to_le_bytes());
        off += 0x48;

        // Attr 2: $FILE_NAME at 0x80 (len 0x78)
        let name = "notes.txt";
        let name_u16: Vec<u16> = name.encode_utf16().collect();
        let body_len = (66 + name_u16.len() * 2) as u32;
        let fn_attr_len = (0x18 + body_len).next_multiple_of(8);
        fixture[off..off + 4].copy_from_slice(&ATTR_TYPE_FILE_NAME.to_le_bytes());
        fixture[off + 4..off + 8].copy_from_slice(&fn_attr_len.to_le_bytes());
        fixture[off + 8] = 0; // resident
        fixture[off + 9] = 0; // unnamed
        fixture[off + 0x10..off + 0x14].copy_from_slice(&body_len.to_le_bytes());
        fixture[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        let val_start = off + 0x18;
        let parent_ref = 5u64 | (1u64 << 48); // parent 5, seq 1
        fixture[val_start..val_start + 8].copy_from_slice(&parent_ref.to_le_bytes());
        fixture[val_start + 64] = name_u16.len() as u8;
        fixture[val_start + 65] = 1; // Win32
        for (i, u) in name_u16.iter().enumerate() {
            fixture[val_start + 66 + i * 2..val_start + 68 + i * 2]
                .copy_from_slice(&u.to_le_bytes());
        }
        off += fn_attr_len as usize;

        // Attr 3: $DATA at off (len 0x28, resident content b"Hello, World!")
        let content = b"Hello, World!";
        let data_len = (0x18 + content.len()).next_multiple_of(8) as u32;
        fixture[off..off + 4].copy_from_slice(&ATTR_TYPE_DATA.to_le_bytes());
        fixture[off + 4..off + 8].copy_from_slice(&data_len.to_le_bytes());
        fixture[off + 8] = 0; // resident
        fixture[off + 9] = 0; // unnamed
        fixture[off + 0x10..off + 0x14].copy_from_slice(&(content.len() as u32).to_le_bytes());
        fixture[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        fixture[off + 0x18..off + 0x18 + content.len()].copy_from_slice(content);
        off += data_len as usize;

        // End marker
        fixture[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

        // Sector tails with USN check values
        fixture[510] = 0x99;
        fixture[511] = 0x88;
        fixture[1022] = 0x99;
        fixture[1023] = 0x88;

        let entry = extract_entry(42, &mut fixture).unwrap();
        assert_eq!(entry.record_no, 42);
        assert_eq!(entry.parent_record, Some(5));
        assert_eq!(String::from_utf16_lossy(&entry.name_units), "notes.txt");
        assert!(!entry.is_directory);
        assert_eq!(entry.logical, 13);
        assert_eq!(entry.allocated, 0);
        assert_eq!(entry.modified_ms, filetime_to_unix_ms(mtime));

        // Verify fixup restored the original sector tail bytes
        assert_eq!(fixture[510..512], [0x11, 0x22]);
        assert_eq!(fixture[1022..1024], [0x33, 0x44]);
    }

    #[test]
    fn truncated_fixup_array_is_out_of_bounds() {
        let mut rec = vec![0u8; 512];
        rec[0x04] = 0xFF;
        rec[0x05] = 0xFF; // USA offset beyond record
        rec[0x06] = 0x05;
        rec[0x07] = 0x00;
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
        // Resident data of 100 bytes (allocated is 0 as data is in MFT record).
        let mut attr = vec![0u8; 0x20 + 100];
        attr[0..4].copy_from_slice(&ATTR_TYPE_DATA.to_le_bytes());
        attr[4..8].copy_from_slice(&((0x18 + 100) as u32).to_le_bytes());
        attr[8] = 0; // resident
        attr[0x10..0x14].copy_from_slice(&100u32.to_le_bytes());
        attr[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        let d = parse_data_attr(&attr).unwrap();
        assert!(d.resident);
        assert_eq!(d.real_size, 100);
        assert_eq!(d.allocated_size, 0);

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

    #[test]
    fn sparse_file_allocated_below_logical() {
        let logical = 100 * 1024 * 1024 * 1024u64; // 100 GiB
        let allocated = 2 * 1024 * 1024 * 1024u64; // 2 GiB
        let mut rec = create_synthetic_record(16, 5, "sparse.img", false, logical, allocated);
        let entry = extract_entry(16, &mut rec).unwrap();
        assert_eq!(entry.logical, logical);
        assert_eq!(entry.allocated, allocated);
        assert!(entry.allocated < entry.logical);
    }

    #[test]
    fn compressed_file_allocation() {
        let logical = 64 * 1024u64; // 64 KiB
        let allocated = 32 * 1024u64; // 32 KiB
        let mut rec = create_synthetic_record(17, 5, "compressed.bin", false, logical, allocated);
        let entry = extract_entry(17, &mut rec).unwrap();
        assert_eq!(entry.logical, logical);
        assert_eq!(entry.allocated, allocated);
        assert!(entry.allocated < entry.logical);
    }

    #[test]
    fn zero_byte_and_resident_files() {
        // Zero-byte file
        let mut rec_zero = create_synthetic_record(18, 5, "empty.txt", false, 0, 0);
        let entry_zero = extract_entry(18, &mut rec_zero).unwrap();
        assert_eq!(entry_zero.logical, 0);
        assert_eq!(entry_zero.allocated, 0);

        // Resident data file
        let mut rec_res = create_synthetic_record(19, 5, "tiny.txt", false, 42, 0);
        let entry_res = extract_entry(19, &mut rec_res).unwrap();
        assert_eq!(entry_res.logical, 42);
        assert_eq!(entry_res.allocated, 0);
    }

    #[test]
    fn records_split_across_read_boundaries() {
        let cancel = AtomicBool::new(false);
        let rec5 = create_synthetic_record(5, 5, "C:", true, 0, 0);
        let rec16 = create_synthetic_record(16, 5, "file1.dat", false, 1000, 4096);
        let rec17 = create_synthetic_record(17, 5, "file2.dat", false, 2000, 4096);
        let rec18 = create_synthetic_record(18, 5, "file3.dat", false, 3000, 4096);

        let mut stream = vec![0u8; 5 * 1024]; // 0..4 unused
        stream.extend_from_slice(&rec5);
        stream.extend_from_slice(&vec![0u8; 10 * 1024]); // 6..15 unused
        stream.extend_from_slice(&rec16);
        stream.extend_from_slice(&rec17);
        stream.extend_from_slice(&rec18);

        let total_valid = stream.len() as u64;

        // Parse with single large chunk
        let mut p_single = MftStreamParser::new(1024, total_valid);
        let mut single_buf = stream.clone();
        p_single.process_chunk(&mut single_buf, &cancel).unwrap();
        let entries_single = p_single.into_entries();

        // Parse with prime chunk sizes (333 bytes) crossing boundaries
        let mut p_split = MftStreamParser::new(1024, total_valid);
        let mut split_buf = stream.clone();
        for chunk in split_buf.chunks_mut(333) {
            p_split.process_chunk(chunk, &cancel).unwrap();
        }
        let entries_split = p_split.into_entries();

        assert_eq!(entries_single.len(), 4);
        assert_eq!(entries_split.len(), 4);
        for (a, b) in entries_single.iter().zip(entries_split.iter()) {
            assert_eq!(a.record_no, b.record_no);
            assert_eq!(a.parent_record, b.parent_record);
            assert_eq!(a.name_units, b.name_units);
            assert_eq!(a.logical, b.logical);
            assert_eq!(a.allocated, b.allocated);
        }
    }

    #[test]
    fn multiple_records_in_one_buffer() {
        let cancel = AtomicBool::new(false);
        let mut stream = vec![0u8; 16 * 1024];
        stream[5 * 1024..6 * 1024]
            .copy_from_slice(&create_synthetic_record(5, 5, "root", true, 0, 0));
        for i in 16..26 {
            stream.extend_from_slice(&create_synthetic_record(
                i,
                5,
                &format!("file_{i}.txt"),
                false,
                i * 100,
                4096,
            ));
        }

        let mut parser = MftStreamParser::new(1024, stream.len() as u64);
        parser.process_chunk(&mut stream, &cancel).unwrap();
        assert_eq!(parser.entries().len(), 11);
    }

    #[test]
    fn correct_record_numbers_across_chunks() {
        let cancel = AtomicBool::new(false);
        let mut stream = vec![0u8; 30 * 1024];
        let rec5 = create_synthetic_record(5, 5, "root", true, 0, 0);
        let rec16 = create_synthetic_record(16, 5, "a.txt", false, 10, 4096);
        let rec25 = create_synthetic_record(25, 5, "b.txt", false, 20, 4096);
        stream[5 * 1024..6 * 1024].copy_from_slice(&rec5);
        stream[16 * 1024..17 * 1024].copy_from_slice(&rec16);
        stream[25 * 1024..26 * 1024].copy_from_slice(&rec25);

        let mut parser = MftStreamParser::new(1024, stream.len() as u64);
        for chunk in stream.chunks_mut(700) {
            parser.process_chunk(chunk, &cancel).unwrap();
        }
        let entries = parser.into_entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].record_no, 5);
        assert_eq!(entries[1].record_no, 16);
        assert_eq!(entries[2].record_no, 25);
    }

    #[test]
    fn incomplete_final_records() {
        let cancel = AtomicBool::new(false);
        let mut stream = vec![0u8; 16 * 1024];
        stream[5 * 1024..6 * 1024]
            .copy_from_slice(&create_synthetic_record(5, 5, "root", true, 0, 0));
        stream.extend_from_slice(&create_synthetic_record(
            16, 5, "test.txt", false, 100, 4096,
        ));
        // Add 500 bytes trailing partial record
        stream.extend_from_slice(&vec![0xAAu8; 500]);

        let mut parser = MftStreamParser::new(1024, 18 * 1024);
        parser.process_chunk(&mut stream, &cancel).unwrap();
        let entries = parser.into_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].record_no, 5);
        assert_eq!(entries[1].record_no, 16);
    }

    #[test]
    fn cancellation_stops_processing() {
        let cancel = AtomicBool::new(true);
        let mut stream = create_synthetic_record(16, 5, "test.txt", false, 100, 4096);
        let mut parser = MftStreamParser::new(1024, 1024);
        let res = parser.process_chunk(&mut stream, &cancel);
        assert_eq!(res.unwrap_err(), "cancelled");
    }

    #[test]
    fn malformed_records_skipped_safely() {
        let cancel = AtomicBool::new(false);
        let mut stream = vec![0u8; 16 * 1024];
        // Record 16: Bad magic "BAAD"
        let mut bad_magic = create_synthetic_record(16, 5, "bad1.txt", false, 100, 4096);
        bad_magic[0..4].copy_from_slice(b"BAAD");
        stream.extend_from_slice(&bad_magic);

        // Record 17: Valid record
        let good17 = create_synthetic_record(17, 5, "good17.txt", false, 100, 4096);
        stream.extend_from_slice(&good17);

        // Record 18: Fixup mismatch
        let mut bad_fixup = create_synthetic_record(18, 5, "bad2.txt", false, 100, 4096);
        bad_fixup[510] = 0x99;
        stream.extend_from_slice(&bad_fixup);

        // Record 19: Valid record
        let good19 = create_synthetic_record(19, 5, "good19.txt", false, 200, 4096);
        stream.extend_from_slice(&good19);

        let mut parser = MftStreamParser::new(1024, 20 * 1024);
        // Feed in 600-byte chunks to test split boundaries with malformed records
        for chunk in stream.chunks_mut(600) {
            parser.process_chunk(chunk, &cancel).unwrap();
        }
        let entries = parser.into_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].record_no, 17);
        assert_eq!(entries[1].record_no, 19);
    }

    #[test]
    fn multiple_mft_extents() {
        let cancel = AtomicBool::new(false);
        // Extent 1 has records 0..20
        let mut extent1 = vec![0u8; 20 * 1024];
        extent1[5 * 1024..6 * 1024]
            .copy_from_slice(&create_synthetic_record(5, 5, "root", true, 0, 0));
        extent1[16 * 1024..17 * 1024]
            .copy_from_slice(&create_synthetic_record(16, 5, "e1.txt", false, 100, 4096));

        // Extent 2 has records 20..30
        let mut extent2 = vec![0u8; 10 * 1024];
        extent2[2 * 1024..3 * 1024]
            .copy_from_slice(&create_synthetic_record(22, 5, "e2.txt", false, 200, 4096));

        let mut parser = MftStreamParser::new(1024, 30 * 1024);
        for chunk in extent1.chunks_mut(1500) {
            parser.process_chunk(chunk, &cancel).unwrap();
        }
        for chunk in extent2.chunks_mut(1500) {
            parser.process_chunk(chunk, &cancel).unwrap();
        }
        let entries = parser.into_entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].record_no, 5);
        assert_eq!(entries[1].record_no, 16);
        assert_eq!(entries[2].record_no, 22);
    }

    #[test]
    fn unnamed_attribute_with_nonzero_record_length_byte_6() {
        // Attribute with length 0x010020 (byte 4=0x20, byte 5=0x00, byte 6=0x01, byte 7=0x00).
        // NameLength at 0x09 is 0 (unnamed).
        let mut attr = vec![0u8; 0x30];
        attr[0..4].copy_from_slice(&ATTR_TYPE_DATA.to_le_bytes());
        attr[4..8].copy_from_slice(&0x010020u32.to_le_bytes());
        attr[8] = 1; // non-resident
        attr[9] = 0; // NameLength = 0 (unnamed)
        assert_eq!(attr[6], 1); // Byte 6 is non-zero
        assert!(attribute_is_unnamed(&attr));
    }

    #[test]
    fn named_attribute_with_nonzero_name_length_not_unnamed() {
        // Attribute with NameLength at 0x09 = 4 (named ADS).
        let mut attr = vec![0u8; 0x30];
        attr[0..4].copy_from_slice(&ATTR_TYPE_DATA.to_le_bytes());
        attr[4..8].copy_from_slice(&0x30u32.to_le_bytes());
        attr[8] = 0; // resident
        attr[9] = 4; // NameLength = 4
        assert!(!attribute_is_unnamed(&attr));
    }

    #[test]
    fn single_extent_retrieval_pointers_and_validation() {
        let mut buf = vec![0u8; 32];
        buf[0..4].copy_from_slice(&1u32.to_le_bytes()); // ExtentCount = 1
        buf[8..16].copy_from_slice(&0u64.to_le_bytes()); // StartingVcn = 0
        buf[16..24].copy_from_slice(&100u64.to_le_bytes()); // NextVcn = 100
        buf[24..32].copy_from_slice(&5000u64.to_le_bytes()); // Lcn = 5000

        let (next_vcn, extents) = parse_retrieval_pointers_buffer(&buf, 0).unwrap();
        assert_eq!(next_vcn, 100);
        assert_eq!(extents.len(), 1);
        assert_eq!(
            extents[0],
            MftExtent {
                vcn_start: 0,
                next_vcn: 100,
                lcn: Some(5000),
            }
        );

        let physical = validate_and_convert_extents(&extents, 100 * 4096, 4096, 10000).unwrap();
        assert_eq!(physical, vec![(5000, 100)]);
    }

    #[test]
    fn multiple_contiguous_and_fragmented_extents() {
        let extents = vec![
            MftExtent {
                vcn_start: 0,
                next_vcn: 50,
                lcn: Some(1000),
            },
            MftExtent {
                vcn_start: 50,
                next_vcn: 120,
                lcn: Some(2500),
            },
            MftExtent {
                vcn_start: 120,
                next_vcn: 200,
                lcn: Some(4000),
            },
        ];
        let physical = validate_and_convert_extents(&extents, 200 * 4096, 4096, 10000).unwrap();
        assert_eq!(physical, vec![(1000, 50), (2500, 70), (4000, 80)]);
    }

    #[test]
    fn multiple_retrieval_pointer_responses_simulated() {
        // Chunk 1: StartingVcn = 0, Extents: 0..50 (LCN 1000), 50..100 (LCN 2000)
        let mut chunk1 = vec![0u8; 48];
        chunk1[0..4].copy_from_slice(&2u32.to_le_bytes());
        chunk1[8..16].copy_from_slice(&0u64.to_le_bytes());
        chunk1[16..24].copy_from_slice(&50u64.to_le_bytes());
        chunk1[24..32].copy_from_slice(&1000u64.to_le_bytes());
        chunk1[32..40].copy_from_slice(&100u64.to_le_bytes());
        chunk1[40..48].copy_from_slice(&2000u64.to_le_bytes());

        let (next_vcn1, extents1) = parse_retrieval_pointers_buffer(&chunk1, 0).unwrap();
        assert_eq!(next_vcn1, 100);

        // Chunk 2: StartingVcn = 100, Extents: 100..150 (LCN 3000), 150..200 (LCN 4000)
        let mut chunk2 = vec![0u8; 48];
        chunk2[0..4].copy_from_slice(&2u32.to_le_bytes());
        chunk2[8..16].copy_from_slice(&100u64.to_le_bytes());
        chunk2[16..24].copy_from_slice(&150u64.to_le_bytes());
        chunk2[24..32].copy_from_slice(&3000u64.to_le_bytes());
        chunk2[32..40].copy_from_slice(&200u64.to_le_bytes());
        chunk2[40..48].copy_from_slice(&4000u64.to_le_bytes());

        let (next_vcn2, extents2) = parse_retrieval_pointers_buffer(&chunk2, 100).unwrap();
        assert_eq!(next_vcn2, 200);

        let mut all_extents = extents1;
        all_extents.extend(extents2);

        let physical = validate_and_convert_extents(&all_extents, 200 * 4096, 4096, 10000).unwrap();
        assert_eq!(
            physical,
            vec![(1000, 50), (2000, 50), (3000, 50), (4000, 50)]
        );
    }

    #[test]
    fn incomplete_extent_map_shorter_than_mft_valid_length_rejected() {
        let extents = vec![MftExtent {
            vcn_start: 0,
            next_vcn: 10, // 10 clusters * 4096 = 40,960 bytes
            lcn: Some(100),
        }];
        // mft_valid_length is 81,920 bytes (20 clusters)
        let res = validate_and_convert_extents(&extents, 81920, 4096, 1000);
        assert_eq!(res.unwrap_err(), MftError::IncompleteExtentMap);
    }

    #[test]
    fn overlapping_and_gapped_vcn_ranges_rejected() {
        // Overlapping VCN ranges (0..100 and 80..150)
        let overlap = vec![
            MftExtent {
                vcn_start: 0,
                next_vcn: 100,
                lcn: Some(1000),
            },
            MftExtent {
                vcn_start: 80,
                next_vcn: 150,
                lcn: Some(2000),
            },
        ];
        assert_eq!(
            validate_and_convert_extents(&overlap, 100 * 4096, 4096, 10000).unwrap_err(),
            MftError::IncompleteExtentMap
        );

        // Gap in VCN ranges (0..100 and 110..200)
        let gap = vec![
            MftExtent {
                vcn_start: 0,
                next_vcn: 100,
                lcn: Some(1000),
            },
            MftExtent {
                vcn_start: 110,
                next_vcn: 200,
                lcn: Some(2000),
            },
        ];
        assert_eq!(
            validate_and_convert_extents(&gap, 200 * 4096, 4096, 10000).unwrap_err(),
            MftError::IncompleteExtentMap
        );

        // Map not starting at VCN 0
        let non_zero_start = vec![MftExtent {
            vcn_start: 5,
            next_vcn: 100,
            lcn: Some(1000),
        }];
        assert_eq!(
            validate_and_convert_extents(&non_zero_start, 100 * 4096, 4096, 10000).unwrap_err(),
            MftError::IncompleteExtentMap
        );
    }

    #[test]
    fn backwards_and_zero_length_extents_rejected() {
        let mut buf = vec![0u8; 32];
        buf[0..4].copy_from_slice(&1u32.to_le_bytes());
        buf[8..16].copy_from_slice(&100u64.to_le_bytes());
        // NextVcn <= StartingVcn (100 -> 100 is zero-length, 100 -> 50 is backwards)
        buf[16..24].copy_from_slice(&100u64.to_le_bytes());
        buf[24..32].copy_from_slice(&500u64.to_le_bytes());

        assert_eq!(
            parse_retrieval_pointers_buffer(&buf, 100).unwrap_err(),
            MftError::IncompleteExtentMap
        );

        buf[16..24].copy_from_slice(&50u64.to_le_bytes());
        assert_eq!(
            parse_retrieval_pointers_buffer(&buf, 100).unwrap_err(),
            MftError::IncompleteExtentMap
        );
    }

    #[test]
    fn arithmetic_overflow_and_volume_boundary_rejected() {
        // LCN + clusters overflows u64
        let overflow = vec![MftExtent {
            vcn_start: 0,
            next_vcn: 100,
            lcn: Some(u64::MAX - 10),
        }];
        assert_eq!(
            validate_and_convert_extents(&overflow, 100 * 4096, 4096, 0).unwrap_err(),
            MftError::OutOfBounds
        );

        // LCN + clusters exceeds total volume clusters
        let out_of_bounds = vec![MftExtent {
            vcn_start: 0,
            next_vcn: 100,
            lcn: Some(950),
        }];
        assert_eq!(
            validate_and_convert_extents(&out_of_bounds, 100 * 4096, 4096, 1000).unwrap_err(),
            MftError::OutOfBounds
        );
    }

    #[test]
    fn sparse_and_unmapped_extents_handled() {
        let mut buf = vec![0u8; 32];
        buf[0..4].copy_from_slice(&1u32.to_le_bytes());
        buf[8..16].copy_from_slice(&0u64.to_le_bytes());
        buf[16..24].copy_from_slice(&50u64.to_le_bytes());
        buf[24..32].copy_from_slice(&u64::MAX.to_le_bytes()); // Sparse LCN

        let (next_vcn, extents) = parse_retrieval_pointers_buffer(&buf, 0).unwrap();
        assert_eq!(next_vcn, 50);
        assert_eq!(extents[0].lcn, None);

        let physical = validate_and_convert_extents(&extents, 50 * 4096, 4096, 1000).unwrap();
        assert_eq!(physical, vec![]); // Sparse extents produce no physical read requests
    }

    #[test]
    fn runs_to_extents_conversion_and_validation() {
        let runs = vec![(1000, 50), (2000, 30)];
        let extents = runs_to_extents(&runs).unwrap();
        assert_eq!(extents.len(), 2);
        assert_eq!(
            extents[0],
            MftExtent {
                vcn_start: 0,
                next_vcn: 50,
                lcn: Some(1000),
            }
        );
        assert_eq!(
            extents[1],
            MftExtent {
                vcn_start: 50,
                next_vcn: 80,
                lcn: Some(2000),
            }
        );

        let physical = validate_and_convert_extents(&extents, 80 * 4096, 4096, 10000).unwrap();
        assert_eq!(physical, vec![(1000, 50), (2000, 30)]);
    }

    #[test]
    fn exact_coverage_and_final_partial_record_coverage() {
        let cancel = AtomicBool::new(false);
        let rs = 1024;

        // Exact coverage: 5 records = 5120 bytes
        let mut stream = vec![0u8; 5 * rs];
        for i in 0..5 {
            let rec = create_synthetic_record(i, 5, &format!("f{i}"), false, 100, 4096);
            stream[i as usize * rs..(i as usize + 1) * rs].copy_from_slice(&rec);
        }
        let mut parser_exact = MftStreamParser::new(rs, 5120);
        parser_exact.process_chunk(&mut stream, &cancel).unwrap();
        let processed_exact = parser_exact.next_record_no() * (rs as u64);
        assert_eq!(processed_exact, 5120);
        assert!(processed_exact >= 5120);

        // Final partial record: 5 records + 300 bytes = 5420 bytes
        let mut stream_partial = vec![0u8; 6 * rs];
        for i in 0..6 {
            let rec = create_synthetic_record(i, 5, &format!("f{i}"), false, 100, 4096);
            stream_partial[i as usize * rs..(i as usize + 1) * rs].copy_from_slice(&rec);
        }
        let mut parser_partial = MftStreamParser::new(rs, 5420);
        parser_partial
            .process_chunk(&mut stream_partial, &cancel)
            .unwrap();
        let processed_partial = parser_partial.next_record_no() * (rs as u64);
        assert_eq!(processed_partial, 6144);
        assert!(processed_partial >= 5420);

        // Incomplete stream: expected 10240 bytes (10 records), but only 5 records provided
        let mut stream_short = vec![0u8; 5 * rs];
        for i in 0..5 {
            let rec = create_synthetic_record(i, 5, &format!("f{i}"), false, 100, 4096);
            stream_short[i as usize * rs..(i as usize + 1) * rs].copy_from_slice(&rec);
        }
        let mut parser_short = MftStreamParser::new(rs, 10240);
        parser_short
            .process_chunk(&mut stream_short, &cancel)
            .unwrap();
        let processed_short = parser_short.next_record_no() * (rs as u64);
        assert_eq!(processed_short, 5120);
        assert!(processed_short < 10240); // Incomplete coverage detected!
    }
}

/// Volume access and MFT streaming (Windows only).
///
/// Everything above this point is pure parsing; this shell is the only
/// part that touches raw volume handles. It requires administrator
/// rights; any failure returns `Err` and the caller falls back to normal
/// directory traversal.
#[cfg(target_os = "windows")]
pub mod reader {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_BEGIN, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING, ReadFile, SetFilePointerEx,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    use super::{
        ATTR_TYPE_DATA, MftExtent, MftStreamParser, apply_fixups, attribute_is_unnamed, attributes,
        build_model, decode_runs, parse_data_attr, parse_retrieval_pointers_buffer,
        runs_to_extents, u32_at, validate_and_convert_extents,
    };
    use crate::model::ScanModel;

    const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x0009_0026;
    const FSCTL_GET_RETRIEVAL_POINTERS: u32 = 0x0009_0073;
    const ERROR_MORE_DATA: u32 = 234;
    const ERROR_HANDLE_EOF: u32 = 38;

    /// Bytes read from the volume per I/O.
    const READ_CHUNK: usize = 8 * 1024 * 1024;

    /// Subset of `NTFS_VOLUME_DATA_BUFFER` the scanner needs.
    #[derive(Clone, Copy)]
    struct VolumeData {
        serial: u64,
        total_clusters: u64,
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
            Some(format!("{}:", (b[0] as char).to_ascii_uppercase()))
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
        let result = scan_with_handle(handle, &drive, root, cancel);
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
            total_clusters: le_u64(16),
            free_clusters: le_u64(24),
            mft_valid_length: le_u64(56),
            bytes_per_cluster: (le_u32(44) as u64).max(512),
            bytes_per_record: (le_u32(48) as u64).max(128),
            mft_start_lcn: le_u64(64),
        })
    }

    fn query_retrieval_pointers(drive: &str) -> Result<Vec<MftExtent>, String> {
        let mft_path = wide(&format!(r"\\.\{drive}\$MFT"));
        // SAFETY: path is a null-terminated wide string; open with read attributes and maximum sharing.
        let handle = unsafe {
            CreateFileW(
                mft_path.as_ptr(),
                windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(format!("cannot open $MFT on {drive}: {}", io_err()));
        }

        let mut all_extents = Vec::new();
        let mut starting_vcn = 0u64;
        let mut out_buf = vec![0u8; 64 * 1024];

        loop {
            let in_buf = starting_vcn.to_le_bytes();
            let mut returned: u32 = 0;
            // SAFETY: valid handle, input buffer is 8 bytes, output buffer is allocated.
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    FSCTL_GET_RETRIEVAL_POINTERS,
                    in_buf.as_ptr().cast(),
                    in_buf.len() as u32,
                    out_buf.as_mut_ptr().cast(),
                    out_buf.len() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };

            if ok != 0 {
                let (_, extents) =
                    parse_retrieval_pointers_buffer(&out_buf[..returned as usize], starting_vcn)
                        .map_err(|e| format!("parse retrieval pointers failed: {e:?}"))?;
                all_extents.extend(extents);
                break;
            }

            let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            if err == ERROR_MORE_DATA {
                let (next_vcn, extents) =
                    parse_retrieval_pointers_buffer(&out_buf[..returned as usize], starting_vcn)
                        .map_err(|e| format!("parse retrieval pointers chunk failed: {e:?}"))?;
                if extents.is_empty() || next_vcn <= starting_vcn {
                    unsafe { CloseHandle(handle) };
                    return Err("retrieval pointers progress stalled".into());
                }
                all_extents.extend(extents);
                starting_vcn = next_vcn;
            } else if err == ERROR_HANDLE_EOF {
                break;
            } else {
                unsafe { CloseHandle(handle) };
                return Err(format!("FSCTL_GET_RETRIEVAL_POINTERS failed: {err}"));
            }
        }

        unsafe { CloseHandle(handle) };
        Ok(all_extents)
    }

    /// Locate all `$MFT` extents, attempting `FSCTL_GET_RETRIEVAL_POINTERS` first,
    /// and falling back to Record 0 run-list parsing.
    fn mft_extents(
        volume_handle: HANDLE,
        drive: &str,
        vd: &VolumeData,
    ) -> Result<Vec<(u64, u64)>, String> {
        // Attempt 1: Query full extent map via FSCTL_GET_RETRIEVAL_POINTERS on $MFT.
        if let Ok(extents) = query_retrieval_pointers(drive)
            && let Ok(physical) = validate_and_convert_extents(
                &extents,
                vd.mft_valid_length,
                vd.bytes_per_cluster,
                vd.total_clusters,
            )
        {
            return Ok(physical);
        }

        // Attempt 2: Fall back to parsing Record 0's unnamed $DATA run list.
        let offset = vd
            .mft_start_lcn
            .checked_mul(vd.bytes_per_cluster)
            .ok_or("MFT location overflows")?;
        seek_to(volume_handle, offset)?;
        let mut rec = vec![0u8; vd.bytes_per_record.max(1024) as usize];
        read_exact(volume_handle, &mut rec)?;
        if rec.len() < 4 || &rec[0..4] != b"FILE" {
            return Err("MFT record 0 lacks the FILE magic".into());
        }
        apply_fixups(&mut rec).map_err(|e| format!("MFT record 0 fixup failed: {e:?}"))?;

        for attr in attributes(&rec) {
            if attr.len() >= 0x10
                && u32_at(attr, 0) == ATTR_TYPE_DATA
                && attribute_is_unnamed(attr)
                && attr.get(8).copied().unwrap_or(0) != 0
            // non-resident
            {
                let info = parse_data_attr(attr).map_err(|e| format!("{e:?}"))?;
                let runs_end = (info.runs_offset + info.runs_len).min(attr.len());
                let raw_runs = decode_runs(&attr[info.runs_offset..runs_end])
                    .map_err(|e| format!("$MFT run list malformed: {e:?}"))?;
                let extents = runs_to_extents(&raw_runs)
                    .map_err(|e| format!("$MFT run list conversion failed: {e:?}"))?;
                return validate_and_convert_extents(
                    &extents,
                    vd.mft_valid_length,
                    vd.bytes_per_cluster,
                    vd.total_clusters,
                )
                .map_err(|e| format!("$MFT extent validation failed: {e:?}"));
            }
        }
        Err("$MFT record has no non-resident data attribute".into())
    }

    fn scan_with_handle(
        handle: HANDLE,
        drive: &str,
        root: &Path,
        cancel: &AtomicBool,
    ) -> Result<ScanModel, String> {
        let vd = query_volume_data(handle)?;
        if vd.mft_valid_length == 0 {
            return Err("invalid volume metadata: mft_valid_length is zero".into());
        }
        let extents = mft_extents(handle, drive, &vd)?;
        let rs = vd.bytes_per_record.max(128) as usize;

        let mut parser = MftStreamParser::new(rs, vd.mft_valid_length);
        let mut chunk = vec![0u8; READ_CHUNK];

        for (lcn, clusters) in extents {
            let base = lcn
                .checked_mul(vd.bytes_per_cluster)
                .ok_or("extent LCN overflows")?;
            let len = clusters
                .checked_mul(vd.bytes_per_cluster)
                .ok_or("extent size overflows")?;
            seek_to(handle, base)?;
            let mut done: u64 = 0;
            while done < len {
                let take = (len - done).min(chunk.len() as u64) as usize;
                read_exact(handle, &mut chunk[..take])?;
                done += take as u64;

                let keep_going = parser.process_chunk(&mut chunk[..take], cancel)?;
                if !keep_going {
                    break;
                }
            }
            if parser.next_record_no() * (rs as u64) >= vd.mft_valid_length {
                break;
            }
        }

        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        let processed_bytes = parser.next_record_no().saturating_mul(rs as u64);
        if processed_bytes < vd.mft_valid_length {
            return Err(format!(
                "incomplete MFT scan: processed {processed_bytes} bytes, expected at least {}",
                vd.mft_valid_length
            ));
        }

        let entries = parser.into_entries();
        let mut model =
            build_model(root, vd.serial, &entries).ok_or("MFT contains no root directory")?;
        model.free_space = Some(vd.free_clusters.saturating_mul(vd.bytes_per_cluster));
        Ok(model)
    }
}
