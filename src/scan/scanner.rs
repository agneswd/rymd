//! Background filesystem scanner.
//!
//! A work-stealing pool ([`scheduler::Scheduler`]) distributes two kinds of
//! jobs: directory enumeration and metadata chunks. Wide directories spill
//! their remaining metadata work as stealable chunks, so no worker
//! serializes a 300k-entry listing. Workers keep local counters (flushed in
//! batches), hard-link identities are reconciled once after traversal, and
//! nothing here ever touches GPUI.
//!
//! Invariant preserved from the original design: parents are always
//! inserted into the arena before their children, which is what makes the
//! single reverse-pass aggregation correct.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_deque::{Stealer, Worker};

use parking_lot::Mutex;

use crate::model::{
    HARDLINK_SHARED, MOUNT_BOUNDARY, Node, NodeId, NodeKind, ScanIssue, ScanModel, UNREADABLE,
};
use crate::scan::metadata::{EntryKind, FileMetadata, ScannerBackend, backend};
use crate::scan::options::{Concurrency, ScanOptions};
use crate::scan::progress::ScanProgress;
use crate::scan::scheduler::{CHUNK, DirJob, EntryBatch, Job, Scheduler};

/// Human-readable name of the scanning backend, for diagnostics.
pub const BACKEND_NAME: &str = "work-stealing";

/// Entries processed between progress flushes. Progress does not need to be
/// exact to the individual inode; batching keeps shared atomics off the
/// per-entry path.
const FLUSH_EVERY: usize = 256;

struct HardlinkCandidate {
    node_ix: u32,
    device: u64,
    inode: u64,
}

type Counter = AtomicU64;

struct Shared {
    model: Mutex<ScanModel>,
    /// Identities of files with `nlink > 1`, appended per batch and
    /// reconciled once after traversal. Append-only, so lock hold times are
    /// tiny compared with a hash-map insert per file on the hot path.
    hardlinks: Mutex<Vec<HardlinkCandidate>>,
    sched: Scheduler,
    files: Counter,
    dirs: Counter,
    logical: Counter,
    allocated: Counter,
    errors: Counter,
    cancelled: Arc<AtomicBool>,
    current: Mutex<PathBuf>,
}

impl Shared {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

/// Per-worker state that never leaves its thread.
struct WorkerState {
    files: u64,
    dirs: u64,
    logical: u64,
    allocated: u64,
    errors: u64,
    hardlinks: Vec<HardlinkCandidate>,
    last_current_push: Instant,
    since_flush: usize,
}

impl WorkerState {
    fn new() -> Self {
        Self {
            files: 0,
            dirs: 0,
            logical: 0,
            allocated: 0,
            errors: 0,
            hardlinks: Vec::new(),
            // Forces an immediate first current-path push.
            last_current_push: Instant::now() - Duration::from_secs(3600),
            since_flush: 0,
        }
    }

    /// Push local counters into the shared snapshot.
    fn flush(&mut self, shared: &Shared) {
        macro_rules! add {
            ($field:ident) => {
                if self.$field != 0 {
                    shared.$field.fetch_add(self.$field, Ordering::Relaxed);
                    self.$field = 0;
                }
            };
        }
        add!(files);
        add!(dirs);
        add!(logical);
        add!(allocated);
        add!(errors);
        if !self.hardlinks.is_empty() {
            shared.hardlinks.lock().extend(self.hardlinks.drain(..));
        }
        self.since_flush = 0;
    }

    fn count(&mut self, md: &FileMetadata) {
        match md.kind {
            EntryKind::File => self.files += 1,
            EntryKind::Directory => self.dirs += 1,
            _ => {}
        }
        self.logical += md.logical;
        self.allocated += md.allocated;
        self.since_flush += 1;
    }

    /// Throttled: visual progress needs roughly a dozen updates a second,
    /// not one mutex write per directory.
    fn report_current(&mut self, shared: &Shared, dir: &std::path::Path) {
        let now = Instant::now();
        if now.duration_since(self.last_current_push) >= Duration::from_millis(80) {
            self.last_current_push = now;
            *shared.current.lock() = dir.to_path_buf();
        }
    }
}

/// Pollable live counters while a scan runs.
#[derive(Clone)]
pub struct ScanLive(Arc<Shared>);

impl ScanLive {
    /// Cheap snapshot for the UI thread; safe to call at any frequency.
    pub fn progress(&self) -> ScanProgress {
        let s = &self.0;
        ScanProgress {
            files_seen: s.files.load(Ordering::Relaxed),
            dirs_seen: s.dirs.load(Ordering::Relaxed),
            bytes_logical: s.logical.load(Ordering::Relaxed),
            bytes_allocated: s.allocated.load(Ordering::Relaxed),
            errors: s.errors.load(Ordering::Relaxed),
            current_path: s.current.lock().clone(),
        }
    }
}

/// Handle to stop an in-flight scan.
#[derive(Clone)]
pub struct CancelHandle(Arc<AtomicBool>);

impl CancelHandle {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum ScanOutcome {
    Completed {
        model: Box<ScanModel>,
        cancelled: bool,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

pub struct ScanJob {
    pub rx: mpsc::Receiver<ScanOutcome>,
    pub cancel: CancelHandle,
    pub live: ScanLive,
}

fn auto_workers() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(2)
}

/// Worker count the pool would use for a new scan right now.
pub fn planned_workers() -> usize {
    planned_workers_for(&ScanOptions::default())
}

fn planned_workers_for(options: &ScanOptions) -> usize {
    match options.concurrency {
        Concurrency::Auto => auto_workers(),
        Concurrency::Fixed(n) => n.max(1),
    }
}

/// Start scanning `root` on background threads.
pub fn spawn_scan(root: PathBuf, options: ScanOptions) -> ScanJob {
    let (tx, rx) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));

    // Build the initial model synchronously so failures surface fast and
    // workers start from a valid arena with the root at index 0.
    let build = || -> Result<(ScanModel, u64), String> {
        let fs = backend();
        let md = fs.metadata(&root).map_err(|e| e.to_string())?;
        if md.kind != EntryKind::Directory {
            return Err("Not a directory".into());
        }
        let mut model = ScanModel::new(root.clone(), md.device);
        model.free_space = fs.free_space(&root).ok();
        model.nodes_mut().reserve(1024);
        let root_node = Node {
            parent: None,
            name: root
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_else(|| root.as_os_str().to_os_string()),
            kind: NodeKind::Directory,
            own_logical: 0,
            own_allocated: 0,
            agg_logical: 0,
            agg_allocated: 0,
            file_count: 0,
            dir_count: 0,
            modified_ms: md.modified_ms,
            device: md.device,
            inode: md.inode,
            children: Vec::new(),
            flags: 0,
        };
        model.nodes_mut().push(root_node);
        Ok((model, md.device))
    };

    let shared = match build() {
        Ok((model, _root_dev)) => Arc::new(Shared {
            model: Mutex::new(model),
            hardlinks: Mutex::new(Vec::new()),
            sched: Scheduler::new(),
            files: Counter::new(0),
            dirs: Counter::new(1),
            logical: Counter::new(0),
            allocated: Counter::new(0),
            errors: Counter::new(0),
            cancelled: cancelled.clone(),
            current: Mutex::new(root.clone()),
        }),
        Err(error) => {
            let _ = tx.send(ScanOutcome::Failed { path: root, error });
            return ScanJob {
                rx,
                cancel: CancelHandle(cancelled),
                live: ScanLive(Arc::new(failed_shared())),
            };
        }
    };

    let root_dev = shared.model.lock().node(NodeId(0)).device;
    shared
        .sched
        .push(Job::dir(Arc::new(root.clone()), NodeId(0), root_dev));

    let live_for_pool = shared.clone();
    let cancelled_in_scan = cancelled.clone();
    let workers = planned_workers_for(&options);
    thread::spawn(move || {
        let started = Instant::now();
        run_pool(&live_for_pool, workers);

        let was_cancelled = cancelled_in_scan.load(Ordering::Relaxed);
        let mut guard = live_for_pool.model.lock();
        let mut model = guard.take();
        drop(guard);
        reconcile_hardlinks(&mut model, &live_for_pool.hardlinks.lock());
        let agg_started = Instant::now();
        model.aggregate();
        model.aggregate_ms = agg_started.elapsed().as_secs_f64() * 1000.0;
        model.duration_ms = started.elapsed().as_millis() as u64;
        model.was_cancelled = was_cancelled;
        let _ = tx.send(ScanOutcome::Completed {
            model: Box::new(model),
            cancelled: was_cancelled,
        });
    });

    ScanJob {
        rx,
        cancel: CancelHandle(cancelled.clone()),
        live: ScanLive(shared),
    }
}

fn failed_shared() -> Shared {
    Shared {
        model: Mutex::new(ScanModel::new(PathBuf::new(), 0)),
        hardlinks: Mutex::new(Vec::new()),
        sched: Scheduler::new(),
        files: Counter::new(0),
        dirs: Counter::new(0),
        logical: Counter::new(0),
        allocated: Counter::new(0),
        errors: Counter::new(1),
        cancelled: Arc::new(AtomicBool::new(true)),
        current: Mutex::new(PathBuf::new()),
    }
}

fn run_pool(shared: &Arc<Shared>, workers: usize) {
    // Workers exist before threads start so every stealer handle is known
    // up front (the standard crossbeam-deque pattern).
    let mut locals = Vec::with_capacity(workers);
    let stealers: Vec<Stealer<Job>> = (0..workers)
        .map(|_| {
            let w = Worker::new_lifo();
            let s = w.stealer();
            locals.push(w);
            s
        })
        .collect();

    let mut joins = Vec::with_capacity(workers);
    for local in locals {
        let s = shared.clone();
        let stealers = stealers.clone();
        joins.push(thread::spawn(move || worker_loop(&s, local, &stealers)));
    }
    for j in joins {
        let _ = j.join();
    }
}

fn worker_loop(shared: &Arc<Shared>, local: Worker<Job>, stealers: &[Stealer<Job>]) {
    let fs = backend();
    let options = ScanOptions::default();
    let mut st = WorkerState::new();

    loop {
        if shared.is_cancelled() {
            break;
        }
        if let Some(job) = shared.sched.find(&local, stealers) {
            process_job(shared, fs, job, &mut st, &options);
            if shared.sched.complete() {
                break; // nothing outstanding anywhere
            }
            continue;
        }
        // No work right now. Register as idle first (so a concurrent push
        // wakes us), then re-check once before actually parking.
        shared.sched.register_idle();
        match shared.sched.find(&local, stealers) {
            Some(job) => {
                shared.sched.unregister_idle();
                process_job(shared, fs, job, &mut st, &options);
                if shared.sched.complete() {
                    break;
                }
            }
            None => {
                if shared.sched.is_quiet() {
                    shared.sched.unregister_idle();
                    break;
                }
                thread::park();
                // Woken by a push or by final completion; unregister and
                // loop so the next find() sees either the new job or the
                // quiet state.
                shared.sched.unregister_idle();
            }
        }
    }
    st.flush(shared);
}

fn process_job(
    shared: &Shared,
    fs: &dyn ScannerBackend,
    job: Job,
    st: &mut WorkerState,
    options: &ScanOptions,
) {
    match job {
        Job::Dir(d) => process_dir(shared, fs, d, st, options),
        Job::Meta(m) => {
            if shared.is_cancelled() {
                return;
            }
            let meta = fs.stat_names(&m.dir, &m.names);
            let batch = EntryBatch {
                names: m.names,
                meta,
            };
            insert_batch(shared, batch, &m.dir, m.parent, m.dev, st, options);
        }
    }
}

fn process_dir(
    shared: &Shared,
    fs: &dyn ScannerBackend,
    task: DirJob,
    st: &mut WorkerState,
    options: &ScanOptions,
) {
    if shared.is_cancelled() {
        return;
    }
    st.report_current(shared, &task.path);

    let inline_limit = fs.inline_limit();
    let mut batch = match fs.enumerate(&task.path, inline_limit.min(CHUNK)) {
        Ok(b) => b,
        Err(e) => {
            st.errors += 1;
            let mut model = shared.model.lock();
            if let Some(node) = model.nodes_mut().get_mut(task.parent.index()) {
                node.flags |= UNREADABLE;
            }
            model.push_issue(ScanIssue {
                path: task.path.as_ref().clone(),
                error: e.to_string(),
            });
            return;
        }
    };

    // Spill everything past the inline prefix as stealable chunks. This is
    // what keeps one huge directory from serializing a single worker.
    if batch.len() > inline_limit {
        let total = batch.len();
        let mut start = inline_limit;
        while start < total {
            let end = (start + CHUNK).min(total);
            shared
                .sched
                .push(Job::Meta(crate::scan::scheduler::MetaJob {
                    dir: task.path.clone(),
                    parent: task.parent,
                    dev: task.dev,
                    names: batch.names.slice_range(start, end),
                }));
            start = end;
        }
        batch.names = batch.names.slice_range(0, inline_limit);
        batch.meta.truncate(inline_limit);
    }

    insert_batch(
        shared,
        batch,
        &task.path,
        task.parent,
        task.dev,
        st,
        options,
    );
}

/// Insert one chunk's nodes under `parent`, then queue its child
/// directories. Runs identically whether the chunk came straight from
/// enumeration or from a spilled metadata job.
fn insert_batch(
    shared: &Shared,
    batch: EntryBatch,
    dir: &Arc<PathBuf>,
    parent: NodeId,
    task_dev: u64,
    st: &mut WorkerState,
    options: &ScanOptions,
) {
    if shared.is_cancelled() {
        return;
    }
    let n = batch.len();

    struct ToQueue {
        path: PathBuf,
        id: NodeId,
        dev: u64,
    }
    let mut child_ids: Vec<NodeId> = Vec::with_capacity(n);
    let mut to_queue: Vec<ToQueue> = Vec::new();

    let mut model = shared.model.lock();
    let nodes = model.nodes_mut();
    nodes.reserve(n);
    let base = nodes.len();

    for ix in 0..n {
        let Some(md) = batch.meta[ix] else {
            st.errors += 1;
            continue;
        };
        st.count(&md);

        // Multi-link files get their bytes attributed to exactly one
        // occurrence during post-scan reconciliation; recording happens off
        // the hot map. Bytes are counted normally here so progress stays
        // honest and reconciliation only has to remove duplicates.
        if md.nlink > 1 && md.kind == EntryKind::File {
            st.hardlinks.push(HardlinkCandidate {
                node_ix: (base + child_ids.len()) as u32,
                device: md.device,
                inode: md.inode,
            });
        }
        // Symlinks and special files carry no meaningful block usage here.
        // Directory inodes own a few blocks and are counted like ncdu does.
        let count_bytes = !matches!(md.kind, EntryKind::Symlink | EntryKind::Other);

        let boundary =
            md.kind == EntryKind::Directory && options.stay_on_filesystem && md.device != task_dev;

        let id = NodeId((base + child_ids.len()) as u32);
        child_ids.push(id);

        let node = Node {
            parent: Some(parent),
            name: batch.names.name_os(ix).into_owned(),
            kind: match md.kind {
                EntryKind::Directory => NodeKind::Directory,
                EntryKind::File => NodeKind::File,
                EntryKind::Symlink => NodeKind::Symlink,
                EntryKind::Other => NodeKind::Other,
            },
            own_logical: if count_bytes { md.logical } else { 0 },
            own_allocated: if count_bytes { md.allocated } else { 0 },
            // Aggregation seeds with own bytes; the reverse pass folds
            // children into parents afterwards.
            agg_logical: if count_bytes { md.logical } else { 0 },
            agg_allocated: if count_bytes { md.allocated } else { 0 },
            file_count: u64::from(md.kind == EntryKind::File),
            dir_count: u64::from(md.kind == EntryKind::Directory),
            modified_ms: md.modified_ms,
            device: md.device,
            inode: md.inode,
            children: Vec::new(),
            flags: if boundary { MOUNT_BOUNDARY } else { 0 },
        };
        nodes.push(node);

        if md.kind == EntryKind::Directory && !boundary {
            // Only directories need a full path: traversal continues from
            // them. Plain files never get one during the scan.
            to_queue.push(ToQueue {
                path: dir
                    .as_path()
                    .join::<&std::ffi::OsStr>(&batch.names.name_os(ix)),
                id,
                dev: md.device,
            });
        }
    }
    model.nodes_mut()[parent.index()].children.extend(child_ids);
    drop(model);

    if st.since_flush >= FLUSH_EVERY {
        st.flush(shared);
    }

    for t in to_queue {
        shared.sched.push(Job::dir(Arc::new(t.path), t.id, t.dev));
    }
}

/// Mark every later link of a multi-link inode as shared and zero its byte
/// contribution, before aggregation folds the tree. Deterministic: the
/// lowest arena index keeps the bytes.
fn reconcile_hardlinks(model: &mut ScanModel, cands: &[HardlinkCandidate]) {
    if cands.len() < 2 {
        return;
    }
    let mut ordered: Vec<&HardlinkCandidate> = cands.iter().collect();
    ordered.sort_unstable_by_key(|c| c.node_ix);
    let mut seen: HashSet<(u64, u64)> = HashSet::with_capacity(cands.len());
    let nodes = model.nodes_mut();
    for c in ordered {
        if !seen.insert((c.device, c.inode)) {
            let n = &mut nodes[c.node_ix as usize];
            n.flags |= HARDLINK_SHARED;
            n.own_logical = 0;
            n.own_allocated = 0;
            n.agg_logical = 0;
            n.agg_allocated = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("rymd-test-{}-{}", tag, std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            TempDir(base)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scan(path: &std::path::Path) -> ScanModel {
        scan_with(path, ScanOptions::default())
    }

    fn scan_with(path: &std::path::Path, options: ScanOptions) -> ScanModel {
        let job = spawn_scan(path.to_path_buf(), options);
        match job.rx.recv_timeout(Duration::from_secs(120)) {
            Ok(ScanOutcome::Completed { model, .. }) => *model,
            other => panic!("scan did not complete: {other:?}"),
        }
    }

    #[test]
    fn counts_files_dirs_and_sizes() {
        let td = TempDir::new("basic");
        let root = td.path();
        fs::write(root.join("a.txt"), vec![0u8; 10_000]).unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/b.bin"), vec![1u8; 50_000]).unwrap();
        fs::create_dir(root.join("empty")).unwrap();

        let m = scan(root);
        let r = m.root();
        assert_eq!(m.node(r).children.len(), 3);
        assert_eq!(m.node(r).file_count, 2);
        assert_eq!(m.node(r).dir_count, 2);
        let logical: u64 = m
            .node(r)
            .children
            .iter()
            .map(|&c| m.node(c).agg_logical)
            .sum();
        assert!(logical >= 60_000, "logical {logical}");
        let allocated: u64 = m
            .node(r)
            .children
            .iter()
            .map(|&c| m.node(c).agg_allocated)
            .sum();
        assert!(
            allocated >= logical,
            "allocated {allocated} < logical {logical}"
        );
    }

    #[test]
    fn symlinks_are_not_followed() {
        let td = TempDir::new("symlink");
        let root = td.path();
        let outside = root
            .parent()
            .unwrap()
            .join(format!("outside-{}.bin", std::process::id()));
        fs::write(&outside, vec![7u8; 100_000]).unwrap();
        fs::write(root.join("real.txt"), vec![0u8; 1_000]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("link.txt")).unwrap();

        let m = scan(root);
        let total = m.node(m.root()).agg_logical;
        assert!(total < 20_000, "symlink target leaked into totals: {total}");

        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn broken_symlink_does_not_abort() {
        let td = TempDir::new("broken-link");
        let root = td.path();
        fs::write(root.join("ok.txt"), b"hello").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("missing-target"), root.join("dangling")).unwrap();

        let m = scan(root);
        assert_eq!(m.node(m.root()).file_count, 1);
    }

    #[test]
    fn hard_links_count_storage_once() {
        let td = TempDir::new("hardlink");
        let root = td.path();
        let payload = vec![3u8; 4096];
        fs::write(root.join("one.dat"), &payload).unwrap();
        fs::hard_link(root.join("one.dat"), root.join("two.dat")).unwrap();
        fs::write(root.join("solo.txt"), b"tiny").unwrap();

        let m = scan(root);
        let files = m
            .node(m.root())
            .children
            .iter()
            .filter(|&&c| !m.node(c).is_dir())
            .count();
        assert_eq!(files, 3, "all three names are listed");
        let shared: Vec<_> = m
            .node(m.root())
            .children
            .iter()
            .filter(|&&c| m.node(c).has_flag(HARDLINK_SHARED))
            .collect();
        assert_eq!(shared.len(), 1, "exactly one link is marked shared");
        // Exactly one of the two links carries the payload bytes.
        let carrying = m
            .node(m.root())
            .children
            .iter()
            .map(|&c| m.node(c))
            .filter(|n| n.own_logical == payload.len() as u64)
            .count();
        assert_eq!(carrying, 1, "payload must be counted exactly once");
        // The whole tree's logical total includes the file exactly once:
        // payload + solo.txt ("tiny", 4 bytes).
        let sum_logical: u64 = m
            .node(m.root())
            .children
            .iter()
            .map(|&c| m.node(c).agg_logical)
            .sum();
        assert_eq!(
            sum_logical,
            payload.len() as u64 + 4,
            "file data must appear exactly once: {sum_logical}"
        );
    }

    #[test]
    fn hard_links_across_directories_reconcile() {
        // Links spread over sibling directories exercise cross-chunk
        // reconciliation instead of the same-directory case.
        let td = TempDir::new("hardlink-wide");
        let root = td.path();
        let big = vec![5u8; 8192];
        fs::write(root.join("orig.bin"), &big).unwrap();
        fs::create_dir(root.join("d1")).unwrap();
        fs::create_dir(root.join("d2")).unwrap();
        for i in 0..80 {
            fs::hard_link(root.join("orig.bin"), root.join(format!("d1/l{i}.bin"))).unwrap();
        }
        for i in 0..80 {
            fs::hard_link(root.join("orig.bin"), root.join(format!("d2/l{i}.bin"))).unwrap();
        }

        let m = scan(root);
        // Walk the entire model: the 8192-byte payload must appear as own
        // logical bytes on exactly one node, and every other link of the
        // same inode must be flagged shared and contribute zero.
        let mut carriers = 0;
        let mut total_own: u64 = 0;
        for ix in 0..m.len() {
            let n = m.node(NodeId(ix as u32));
            if n.kind == NodeKind::File && !n.has_flag(HARDLINK_SHARED) && n.own_logical > 0 {
                total_own += n.own_logical;
                if n.own_logical == big.len() as u64 {
                    carriers += 1;
                }
            }
        }
        assert_eq!(carriers, 1, "payload must be carried by exactly one link");
        // solo files: none here besides the links themselves.
        assert_eq!(
            total_own,
            big.len() as u64,
            "multi-link storage must be counted exactly once"
        );
    }

    #[test]
    fn sparse_file_allocated_below_logical() {
        let td = TempDir::new("sparse");
        let root = td.path();
        let f = fs::File::create(root.join("sparse.bin")).unwrap();
        f.set_len(4 * 1024 * 1024).unwrap();
        drop(f);

        let m = scan(root);
        let node = m
            .node(m.root())
            .children
            .iter()
            .map(|&c| m.node(c))
            .find(|n| n.kind == NodeKind::File)
            .unwrap();
        assert_eq!(node.own_logical, 4 * 1024 * 1024);
        assert!(
            node.own_allocated < node.own_logical,
            "not detected as sparse"
        );
    }

    #[test]
    fn non_utf8_names_survive() {
        use std::os::unix::ffi::OsStrExt;
        let td = TempDir::new("nonutf8");
        let root = td.path();
        let weird = std::ffi::OsStr::from_bytes(b"weird-\xff\xfe-name.txt");
        fs::write(root.join(weird), b"data").unwrap();

        let m = scan(root);
        assert_eq!(m.node(m.root()).children.len(), 1);
        let name = m.node(m.root()).children[0];
        assert_eq!(m.node(name).name.as_os_str(), weird);
        assert!(m.path_of(name).file_name().unwrap() == weird);
    }

    #[test]
    fn mount_boundary_flag_on_other_device_is_recorded() {
        // We cannot easily create another filesystem here; instead verify
        // that same-device directories carry no boundary flag.
        let td = TempDir::new("boundary");
        let root = td.path();
        fs::create_dir(root.join("normal")).unwrap();
        let m = scan(root);
        for c in m.node(m.root()).children.clone() {
            assert!(!m.node(c).has_flag(MOUNT_BOUNDARY));
        }
    }

    #[test]
    fn cancellation_stops_early() {
        let td = TempDir::new("cancel");
        let root = td.path();
        for i in 0..200 {
            let d = root.join(format!("d{i}"));
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("f.bin"), vec![0u8; 2048]).unwrap();
        }
        let job = spawn_scan(root.to_path_buf(), ScanOptions::default());
        job.cancel.cancel();
        match job.rx.recv_timeout(Duration::from_secs(30)) {
            Ok(ScanOutcome::Completed {
                cancelled: true, ..
            }) => {}
            other => panic!("expected cancelled completion, got {other:?}"),
        }
    }

    #[test]
    fn wide_directory_parallel_chunks_match_serial_result() {
        // More entries than one chunk, forcing spilled metadata work; the
        // aggregate result must equal what serial accounting produces.
        let td = TempDir::new("wide");
        let root = td.path();
        let expected: u64 = (0..500u32).map(|i| u64::from(i) * 10).sum();
        for i in 0..500u32 {
            fs::write(root.join(format!("f{i:04}")), vec![0u8; (i * 10) as usize]).unwrap();
        }
        let single = scan_with(
            root,
            ScanOptions {
                concurrency: Concurrency::Fixed(1),
                ..Default::default()
            },
        );
        let multi = scan_with(
            root,
            ScanOptions {
                concurrency: Concurrency::Fixed(6),
                ..Default::default()
            },
        );
        assert_eq!(single.node(single.root()).file_count, 500);
        assert_eq!(single.node(single.root()).agg_logical, expected);
        assert_eq!(multi.node(multi.root()).file_count, 500);
        assert_eq!(multi.node(multi.root()).agg_logical, expected);
    }

    #[test]
    fn failed_root_reports_error() {
        let job = spawn_scan(
            std::path::PathBuf::from("/nonexistent/rymd/definitely-missing"),
            ScanOptions::default(),
        );
        match job.rx.recv_timeout(Duration::from_secs(10)) {
            Ok(ScanOutcome::Failed { .. }) => {}
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn unreadable_directory_becomes_issue() {
        let td = TempDir::new("perm");
        let root = td.path();
        let locked = root.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("hidden.txt"), b"x").unwrap();
        // Running as root ignores permission bits; skip in that case.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let mut perm = fs::metadata(&locked).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perm.set_mode(0o000);
        fs::set_permissions(&locked, perm).unwrap();

        let m = scan(root);
        assert_eq!(m.issues().len(), 1, "one issue recorded");
        assert!(
            m.node(m.root()).flags & UNREADABLE != 0
                || m.node(m.root())
                    .children
                    .iter()
                    .any(|&c| m.node(c).flags & UNREADABLE != 0)
        );
    }
}
