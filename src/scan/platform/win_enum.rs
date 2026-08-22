//! Strict parser for `FILE_ID_BOTH_DIR_INFO` records returned by
//! `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`.
//!
//! Compiled on every platform so the malformed-input tests run in Linux CI
//! as well. Nothing here calls Windows APIs; the caller hands us the raw
//! bytes the kernel wrote into our buffer.
//!
//! Layout (documented in the Win32 API reference):
//!
//! ```text
//! 0    DWORD NextEntryOffset      (0 marks the last record)
//! 4    DWORD FileIndex            (reserved, ignore)
//! 8    LARGE_INTEGER CreationTime
//! 16   LARGE_INTEGER LastAccessTime
//! 24   LARGE_INTEGER LastWriteTime
//! 32   LARGE_INTEGER ChangeTime
//! 40   LARGE_INTEGER EndOfFile        logical size
//! 48   LARGE_INTEGER AllocationSize   size on disk (sparse/compressed aware)
//! 56   DWORD FileAttributes
//! 60   DWORD FileNameLength           bytes of UTF-16 data
//! 64   DWORD EaSize
//! 68   BYTE  ShortNameLength (+3 padding)
//! 72   WCHAR ShortName[12]
//! 96   LARGE_INTEGER FileId
//! 104  WCHAR FileName[FileNameLength / 2]
//! ```

/// One parsed directory record. Names stay as raw UTF-16 code units; the
/// caller converts them once into its own storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DirRecord {
    /// Offset of the name inside the source buffer.
    pub name_offset: usize,
    /// Length of the name in UTF-16 code units (`FileNameLength / 2`).
    pub name_units: usize,
    pub attributes: u32,
    pub end_of_file: u64,
    pub allocation_size: u64,
    pub last_write_filetime: i64,
    /// 64-bit file id; unique per volume on NTFS/ReFS.
    pub file_id: u64,
}

pub(crate) const HEADER_LEN: usize = 104;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParseOutcome {
    /// All complete records were parsed; `true` means the final record had
    /// `NextEntryOffset == 0`, i.e. this call returned everything.
    Records(Vec<DirRecord>, /* chain_terminated */ bool),
    /// A record header claims to extend past the buffer: the kernel wrote
    /// a truncated or corrupt chain. Never index out of bounds.
    Corrupt,
}

/// Parse a chain of records out of `buf`.
///
/// Every field access is bounds-checked against `buf.len()`; a chain that
/// lies about its offsets yields [`ParseOutcome::Corrupt`] instead of a
/// panic or an out-of-bounds read.
pub(crate) fn parse_id_both_dir_info(buf: &[u8]) -> ParseOutcome {
    let mut records = Vec::new();
    let mut off = 0usize;
    loop {
        if off + HEADER_LEN > buf.len() {
            // Either a truncated tail (kernel guarantees whole records, so
            // this means corruption) or garbage.
            return if off == buf.len() {
                ParseOutcome::Records(records, false)
            } else {
                ParseOutcome::Corrupt
            };
        }
        let next = u32_at(buf, off);
        let attributes = u32_at(buf, off + 56);
        let name_len_bytes = u32_at(buf, off + 60) as usize;
        let end = off + HEADER_LEN + name_len_bytes;
        if end > buf.len() {
            return ParseOutcome::Corrupt;
        }
        // Odd byte lengths cannot be valid UTF-16.
        if !name_len_bytes.is_multiple_of(2) {
            return ParseOutcome::Corrupt;
        }

        records.push(DirRecord {
            name_offset: off + HEADER_LEN,
            name_units: name_len_bytes / 2,
            attributes,
            end_of_file: i64_at(buf, off + 40) as u64,
            allocation_size: i64_at(buf, off + 48) as u64,
            last_write_filetime: i64_at(buf, off + 24),
            file_id: i64_at(buf, off + 96) as u64,
        });

        if next == 0 {
            return ParseOutcome::Records(records, true);
        }
        let next = next as usize;
        // Offsets must move forward and stay inside what the kernel wrote.
        if next < HEADER_LEN || off + next > buf.len() {
            return ParseOutcome::Corrupt;
        }
        off += next;
    }
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn i64_at(buf: &[u8], off: usize) -> i64 {
    i64::from_le_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
        buf[off + 4],
        buf[off + 5],
        buf[off + 6],
        buf[off + 7],
    ])
}

/// FILETIME (100 ns ticks since 1601-01-01) to unix milliseconds.
pub(crate) fn filetime_to_unix_ms(ft: i64) -> Option<i64> {
    // 11_644_473_600 seconds between the 1601 and 1970 epochs.
    const EPOCH_DIFF_TICKS: i128 = 11_644_473_600 * 10_000_000;
    let ticks = ft as i128 - EPOCH_DIFF_TICKS;
    let ms = ticks / 10_000;
    i64::try_from(ms).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn put_i64(v: &mut Vec<u8>, x: i64) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    fn build_record(name: &str, next: u32, alloc: i64, eof: i64, attrs: u32) -> Vec<u8> {
        let units = name.encode_utf16().collect::<Vec<_>>();
        let mut v = Vec::with_capacity(HEADER_LEN + units.len() * 2);
        let total = (HEADER_LEN + units.len() * 2 + 7) & !7;
        put_u32(&mut v, if next == u32::MAX { total as u32 } else { next });
        put_u32(&mut v, 0); // FileIndex
        put_i64(&mut v, 0); // Creation
        put_i64(&mut v, 0); // Access
        put_i64(&mut v, 123_456); // LastWrite
        put_i64(&mut v, 0); // Change
        put_i64(&mut v, eof);
        put_i64(&mut v, alloc);
        put_u32(&mut v, attrs);
        put_u32(&mut v, (units.len() * 2) as u32);
        put_u32(&mut v, 0); // EaSize
        v.push(0); // ShortNameLength
        v.extend_from_slice(&[0u8; 3]);
        v.extend_from_slice(
            &[0u16; 12]
                .iter()
                .flat_map(|x| x.to_le_bytes())
                .collect::<Vec<u8>>(),
        );
        put_i64(&mut v, 42); // FileId
        for u in &units {
            v.extend_from_slice(&u.to_le_bytes());
        }
        while v.len() % 8 != 0 {
            v.push(0);
        }
        assert_eq!(v.len(), total);
        v
    }

    #[test]
    fn parses_a_single_terminated_record() {
        let rec = build_record("hello.txt", 0, 8192, 5000, 0x20);
        match parse_id_both_dir_info(&rec) {
            ParseOutcome::Records(rs, true) => {
                assert_eq!(rs.len(), 1);
                let r = rs[0];
                assert_eq!(r.name_units, 9);
                let name: Vec<u16> = rec[r.name_offset..r.name_offset + r.name_units * 2]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect();
                assert_eq!(String::from_utf16_lossy(&name), "hello.txt");
                assert_eq!(r.end_of_file, 5000);
                assert_eq!(r.allocation_size, 8192);
                assert_eq!(r.last_write_filetime, 123_456);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_chains_until_zero_offset() {
        let mut buf = build_record("a", u32::MAX, 0, 1, 0x10);
        let second = build_record("bb", 0, 0, 2, 0x10);
        let next = buf.len() as u32;
        buf.extend_from_slice(&second);
        buf[0..4].copy_from_slice(&next.to_le_bytes());
        match parse_id_both_dir_info(&buf) {
            ParseOutcome::Records(rs, true) => assert_eq!(rs.len(), 2),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn truncated_name_is_corrupt_not_oob() {
        let mut rec = build_record("truncated-name-here", 0, 0, 0, 0);
        rec.truncate(rec.len() - 4);
        assert_eq!(parse_id_both_dir_info(&rec), ParseOutcome::Corrupt);
    }

    #[test]
    fn lying_next_offset_is_corrupt_not_oob() {
        let mut rec = build_record("x", 0, 0, 0, 0);
        // Claim a huge jump.
        rec[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_id_both_dir_info(&rec), ParseOutcome::Corrupt);
    }

    #[test]
    fn self_referential_offset_is_corrupt() {
        let mut rec = build_record("x", 0, 0, 0, 0);
        rec[0..4].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        assert_eq!(parse_id_both_dir_info(&rec), ParseOutcome::Corrupt);
    }

    #[test]
    fn odd_name_length_is_corrupt() {
        let mut rec = build_record("xy", 0, 0, 0, 0);
        rec[60..64].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(parse_id_both_dir_info(&rec), ParseOutcome::Corrupt);
    }

    #[test]
    fn empty_buffer_is_an_empty_chain() {
        assert_eq!(
            parse_id_both_dir_info(&[]),
            ParseOutcome::Records(Vec::new(), false)
        );
    }

    #[test]
    fn filetime_conversion_matches_known_values() {
        // 2024-01-01T00:00:00Z is 133485408000000000 ticks since 1601.
        let ms = filetime_to_unix_ms(133_485_408_000_000_000).unwrap();
        assert_eq!(ms, 1_704_067_200_000);
        // Extreme values stay finite instead of panicking or overflowing.
        assert_eq!(filetime_to_unix_ms(0), Some(-11_644_473_600_000));
        assert_eq!(
            filetime_to_unix_ms(i64::MIN),
            Some(-11644473600000 - 922337203685477)
        );
    }
}
