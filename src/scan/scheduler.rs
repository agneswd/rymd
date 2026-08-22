//! Work-stealing job pool for the scanner.
//!
//! One shared injector plus a per-worker LIFO queue. Workers consume their
//! own queue first (cache-hot, no contention), then the injector, then
//! steal from other workers. Jobs are counted as pending from push until a
//! worker completes them, so "pending == 0" plus empty queues is an exact
//! termination signal.
//!
//! Two job kinds keep wide directories from serializing one worker: `Dir`
//! enumerates a directory and inserts its nodes; directories wider than
//! [`CHUNK`] spill their remaining metadata work as stealable `Meta`
//! chunks.

use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use crossbeam_deque::{Injector, Stealer, Worker};

use crate::model::NodeId;

/// Entries per metadata chunk and the inline-statted prefix of a freshly
/// enumerated directory. Large enough that chunk overhead is noise, small
/// enough that a huge listing keeps every worker busy.
pub const CHUNK: usize = 128;

/// Raw entry names from one directory, stored contiguously.
///
/// Unix stores each name as raw bytes (`OsStr`'s native form). Windows
/// stores UTF-16 code units, which is what directory records carry, so
/// bulk enumeration never converts through an `OsString`. `ends` holds
/// cumulative end positions in elements.
#[derive(Default)]
pub struct NameBlob {
    #[cfg(unix)]
    data: Box<[u8]>,
    #[cfg(windows)]
    data: Box<[u16]>,
    ends: Vec<u32>,
}

impl NameBlob {
    /// Build from an iterator of OS string slices.
    pub fn from_names<'a, I>(names: I) -> Self
    where
        I: IntoIterator<Item = &'a std::ffi::OsStr>,
    {
        let mut data = Vec::new();
        let mut ends = Vec::new();
        for name in names {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt as _;
                data.extend_from_slice(name.as_bytes());
            }
            #[cfg(windows)]
            {
                use std::os::windows::ffi::OsStrExt as _;
                data.extend(name.encode_wide());
            }
            ends.push(element_len(&data) as u32);
        }
        Self {
            data: data.into_boxed_slice(),
            ends,
        }
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Raw bytes of the name at `ix` (Unix). POSIX names cannot contain
    /// interior NUL bytes, so these round-trip through syscalls losslessly.
    #[cfg(unix)]
    pub fn bytes(&self, ix: usize) -> &[u8] {
        let start = if ix == 0 {
            0
        } else {
            self.ends[ix - 1] as usize
        };
        &self.data[start..self.ends[ix] as usize]
    }

    /// Raw UTF-16 code units of the name at `ix` (Windows).
    #[cfg(windows)]
    pub fn units(&self, ix: usize) -> &[u16] {
        let start = if ix == 0 {
            0
        } else {
            self.ends[ix - 1] as usize
        };
        &self.data[start..self.ends[ix] as usize]
    }

    #[cfg(unix)]
    pub fn name_os(&self, ix: usize) -> std::borrow::Cow<'_, std::ffi::OsStr> {
        use std::os::unix::ffi::OsStrExt as _;
        std::borrow::Cow::Borrowed(std::ffi::OsStr::from_bytes(self.bytes(ix)))
    }

    #[cfg(windows)]
    pub fn name_os(&self, ix: usize) -> std::borrow::Cow<'_, std::ffi::OsStr> {
        use std::os::windows::ffi::OsStringExt as _;
        std::borrow::Cow::Owned(std::ffi::OsString::from_wide(self.units(ix)))
    }

    fn starts(&self, ix: usize) -> usize {
        if ix == 0 {
            0
        } else {
            self.ends[ix - 1] as usize
        }
    }

    fn end(&self, ix: usize) -> usize {
        self.ends[ix] as usize
    }

    /// Extract the sub-blob for a half-open name range, copying that
    /// slice's elements.
    pub fn slice_range(&self, start_ix: usize, end_ix: usize) -> NameBlob {
        let (s, e) = (self.starts(start_ix), self.end(end_ix - 1));
        let data = self.data[s..e].to_vec();
        let ends = self.ends[start_ix..end_ix]
            .iter()
            .map(|&v| v - s as u32)
            .collect();
        NameBlob {
            data: data.into_boxed_slice(),
            ends,
        }
    }

    #[cfg(unix)]
    pub(crate) fn from_flattened(parts: Vec<Vec<u8>>) -> Self {
        let total: usize = parts.iter().map(|p| p.len()).sum();
        let mut data = Vec::with_capacity(total);
        let mut ends = Vec::with_capacity(parts.len());
        for part in parts {
            data.extend_from_slice(&part);
            ends.push(data.len() as u32);
        }
        Self {
            data: data.into_boxed_slice(),
            ends,
        }
    }

    #[cfg(windows)]
    pub(crate) fn from_wide_parts(parts: Vec<Vec<u16>>) -> Self {
        let total: usize = parts.iter().map(|p| p.len()).sum();
        let mut data = Vec::with_capacity(total);
        let mut ends = Vec::with_capacity(parts.len());
        for part in parts {
            data.extend_from_slice(&part);
            ends.push(data.len() as u32);
        }
        Self {
            data: data.into_boxed_slice(),
            ends,
        }
    }
}

#[cfg(windows)]
fn element_len(data: &[u16]) -> usize {
    data.len()
}

#[cfg(not(windows))]
fn element_len(data: &[u8]) -> usize {
    data.len()
}

/// One directory's entries: raw names plus whatever metadata the OS handed
/// out during enumeration. Entries whose metadata is `None` need a stat
/// pass (the Linux case; see `ScannerBackend::stat_names`).
pub struct EntryBatch {
    pub names: NameBlob,
    pub meta: Vec<Option<crate::scan::metadata::FileMetadata>>,
}

impl EntryBatch {
    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// Enumerate + insert one directory's nodes.
pub(crate) struct DirJob {
    pub path: Arc<PathBuf>,
    pub parent: NodeId,
    pub dev: u64,
}

/// Collect metadata for a slice of one directory's names and insert the
/// nodes. The directory is reopened once per chunk: one syscall per 64
/// entries instead of one full-path resolution per entry.
pub(crate) struct MetaJob {
    pub dir: Arc<PathBuf>,
    pub parent: NodeId,
    pub dev: u64,
    pub names: NameBlob,
}

pub(crate) enum Job {
    Dir(DirJob),
    Meta(MetaJob),
}

impl Job {
    pub fn dir(path: Arc<PathBuf>, parent: NodeId, dev: u64) -> Job {
        Job::Dir(DirJob { path, parent, dev })
    }
}

pub(crate) struct Scheduler {
    injector: Injector<Job>,
    pending: AtomicUsize,
    rotor: AtomicUsize,
    /// Workers currently blocked waiting for work. Pushes pop one and wake
    /// it; registration happens under the same lock, so a wakeup can never
    /// be lost between a failed search and a park.
    idle: Mutex<Vec<thread::Thread>>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            injector: Injector::new(),
            pending: AtomicUsize::new(0),
            rotor: AtomicUsize::new(0),
            idle: Mutex::new(Vec::new()),
        }
    }

    /// Publish a job. Pending is bumped before the job becomes visible so
    /// termination can never race ahead of reality.
    pub fn push(&self, job: Job) {
        self.pending.fetch_add(1, Ordering::AcqRel);
        self.injector.push(job);
        if let Some(waiter) = self.idle.lock().pop() {
            waiter.unpark();
        }
    }

    pub fn is_quiet(&self) -> bool {
        self.pending.load(Ordering::Acquire) == 0
    }

    /// Mark one job finished; true when this was the last outstanding one.
    pub fn complete(&self) -> bool {
        let last = self.pending.fetch_sub(1, Ordering::AcqRel) == 1;
        if last {
            // Wake everyone so blocked workers can exit promptly.
            let mut idle = self.idle.lock();
            for waiter in idle.drain(..) {
                waiter.unpark();
            }
        }
        last
    }

    /// Register this thread as idle. The caller must re-check for work
    /// afterwards and then either unregister or park.
    pub fn register_idle(&self) {
        self.idle.lock().push(thread::current());
    }

    pub fn unregister_idle(&self) {
        let me = thread::current();
        let mut idle = self.idle.lock();
        if let Some(pos) = idle.iter().position(|t| t.id() == me.id()) {
            idle.swap_remove(pos);
        }
    }

    /// Next unit of work: own queue, then the injector, then steal.
    /// `None` means genuinely idle right now.
    pub fn find(&self, local: &Worker<Job>, stealers: &[Stealer<Job>]) -> Option<Job> {
        local
            .pop()
            .or_else(|| match self.injector.steal() {
                crossbeam_deque::Steal::Success(job) => Some(job),
                _ => None,
            })
            .or_else(|| {
                let n = stealers.len();
                if n == 0 {
                    return None;
                }
                let start = self.rotor.fetch_add(1, Ordering::Relaxed) % n;
                for k in 0..n {
                    match stealers[(start + k) % n].steal() {
                        crossbeam_deque::Steal::Success(job) => return Some(job),
                        crossbeam_deque::Steal::Empty => continue,
                        crossbeam_deque::Steal::Retry => return None,
                    }
                }
                None
            })
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn name_blob_round_trips_raw_bytes() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let weird = OsStr::from_bytes(b"weird-\xff\xfe-name");
        let blob = NameBlob::from_names([OsStr::new("a"), weird, OsStr::new("longer-name.txt")]);
        assert_eq!(blob.len(), 3);
        assert_eq!(&*blob.name_os(0), OsStr::new("a"));
        assert_eq!(blob.name_os(1).as_bytes(), weird.as_bytes());
        assert_eq!(&*blob.name_os(2), OsStr::new("longer-name.txt"));
    }

    #[test]
    fn empty_blob_is_empty() {
        let blob = NameBlob::from_names([] as [&std::ffi::OsStr; 0]);
        assert!(blob.is_empty());
        assert_eq!(blob.len(), 0);
    }
}
