/// Progress snapshots polled by the UI roughly every 100 ms.
///
/// The scanner only touches atomics and a small path buffer; it never
/// sends one message per discovered file.
use std::path::PathBuf;

#[derive(Default, Clone, Debug)]
pub struct ScanProgress {
    pub files_seen: u64,
    pub dirs_seen: u64,
    pub bytes_logical: u64,
    pub bytes_allocated: u64,
    pub errors: u64,
    pub current_path: PathBuf,
}
