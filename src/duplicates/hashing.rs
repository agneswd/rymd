//! BLAKE3 hashing helpers for duplicate detection.
//!
//! Two stages keep full hashing rare: a partial hash over the first and
//! last 64 KiB separates most candidates, and only groups that still
//! collide get fully read.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

const CHUNK: usize = 64 * 1024;

/// Hash of head+tail; cheap because it reads at most 128 KiB.
pub fn partial_hash(path: &Path, len: u64) -> io::Result<u64> {
    let mut f = File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];

    let head = len.min(CHUNK as u64) as usize;
    f.read_exact(&mut buf[..head])?;
    h.update(&buf[..head]);

    if len > CHUNK as u64 {
        let tail_start = len - CHUNK as u64;
        use std::io::{Seek, SeekFrom};
        f.seek(SeekFrom::Start(tail_start))?;
        f.read_exact(&mut buf)?;
        h.update(&buf);
    }

    // Fold the 256-bit digest into a stable 64-bit key.
    let hash = h.finalize();
    Ok(u64::from_le_bytes(hash.as_bytes()[0..8].try_into().unwrap()))
}

/// Full content hash, streamed in chunks so huge files never balloon memory.
pub fn full_hash(path: &Path) -> io::Result<[u8; 16]> {
    let mut f = File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let hash = h.finalize();
    Ok(hash.as_bytes()[0..16].try_into().unwrap())
}
