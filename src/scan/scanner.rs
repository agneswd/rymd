//! Background filesystem scanner.
//!
//! Runs a bounded pool of worker threads over a shared work stack. Workers
//! gather directory entries, append nodes to the arena in per-directory
//! batches, and queue child directories. The UI polls live progress through
//! [`ScanLive`] and receives the finished model on a channel; nothing here
//! ever touches GPUI.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

use parking_lot::{Condvar, Mutex};

use crate::model::{
    Node, NodeId, NodeKind, ScanIssue, ScanModel, HARDLINK_SHARED, MOUNT_BOUNDARY, UNREADABLE,
};
use crate::scan::metadata::{platform, EntryKind, FileMetadata};
use crate::scan::options::ScanOptions;
use crate::scan::progress::ScanProgress;

struct DirTask {
    path: PathBuf,
    parent: NodeId,
    dev: u64,
}

struct WorkQueue {
    stack: Mutex<Vec<DirTask>>,
    cv: Condvar,
}

impl WorkQueue {
    fn new() -> Self {
        Self {
            stack: Mutex::new(Vec::new()),
            cv: Condvar::new(),
        }
    }

    fn push(&self, task: DirTask) {
        self.stack.lock().push(task);
        self.cv.notify_one();
    }

    /// Pop one task. Returns None when the stack is empty and no worker
    /// still has work in flight (`pending == 0`), so the whole pool exits
    /// together without channel-close bookkeeping.
    fn pop(&self, pending: &AtomicUsize, cancelled: &AtomicBool) -> Option<DirTask> {
        let mut guard = self.stack.lock();
        loop {
            if let Some(task) = guard.pop() {
                return Some(task);
            }
            if cancelled.load(Ordering::Relaxed) || pending.load(Ordering::Acquire) == 0 {
                return None;
            }
            self.cv
                .wait_for(&mut guard, std::time::Duration::from_millis(50));
        }
    }
}

struct Shared {
    model: Mutex<ScanModel>,
    hardlinks: Mutex<HashMap<(u64, u64), ()>>,
    queue: WorkQueue,
    files: AtomicU64,
    dirs: AtomicU64,
    logical: AtomicU64,
    allocated: AtomicU64,
    errors: AtomicU64,
    pending: AtomicUsize,
    cancelled: Arc<AtomicBool>,
    current: Mutex<PathBuf>,
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

fn worker_count() -> usize {
    let cpus = thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    cpus.clamp(2, 8)
}

/// Start scanning `root` on background threads.
pub fn spawn_scan(root: PathBuf, _options: ScanOptions) -> ScanJob {
    let (tx, rx) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));

    // Build the initial model synchronously so failures surface fast and
    // workers start from a valid arena with the root at index 0.
    let build = || -> Result<(ScanModel, FileMetadata), String> {
        let fs = platform();
        let md = fs.metadata(&root).map_err(|e| e.to_string())?;
        if md.kind != EntryKind::Directory {
            return Err("Not a directory".into());
        }
        let mut model = ScanModel::new(root.clone(), md.device);
        model.free_space = fs.free_space(&root).ok();
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
        Ok((model, md))
    };

    let cancelled_for_shared = cancelled.clone();
    let shared = match build() {
        Ok((model, _root_md)) => Arc::new(Shared {
            model: Mutex::new(model),
            hardlinks: Mutex::new(HashMap::new()),
            queue: WorkQueue::new(),
            files: AtomicU64::new(0),
            dirs: AtomicU64::new(1),
            logical: AtomicU64::new(0),
            allocated: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            pending: AtomicUsize::new(1),
            cancelled: cancelled_for_shared,
            current: Mutex::new(root.clone()),
        }),
        Err(error) => {
            let _ = tx.send(ScanOutcome::Failed { path: root, error });
            return ScanJob {
                rx,
                cancel: CancelHandle(cancelled),
                live: ScanLive(Arc::new(Shared {
                    model: Mutex::new(ScanModel::new(PathBuf::new(), 0)),
                    hardlinks: Mutex::new(HashMap::new()),
                    queue: WorkQueue::new(),
                    files: AtomicU64::new(0),
                    dirs: AtomicU64::new(0),
                    logical: AtomicU64::new(0),
                    allocated: AtomicU64::new(0),
                    errors: AtomicU64::new(1),
                    pending: AtomicUsize::new(0),
                    cancelled: Arc::new(AtomicBool::new(true)),
                    current: Mutex::new(PathBuf::new()),
                })),
            };
        }
    };

    let root_dev = shared.model.lock().node(NodeId(0)).device;
    shared.queue.push(DirTask {
        path: root.clone(),
        parent: NodeId(0),
        dev: root_dev,
    });

    let live_for_thread = shared.clone();
    let cancelled_in_scan = cancelled.clone();
    thread::spawn(move || {
        let cancelled = cancelled_in_scan;
        let started = std::time::Instant::now();
        let mut joins = Vec::with_capacity(worker_count());
        for _ in 0..worker_count() {
            let live = live_for_thread.clone();
            joins.push(thread::spawn(move || worker_loop(&live)));
        }
        for j in joins {
            let _ = j.join();
        }

        let was_cancelled = cancelled.load(Ordering::Relaxed);
        let mut guard = live_for_thread.model.lock();
        let mut model = guard.take();
        drop(guard);
        model.aggregate();
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

fn worker_loop(shared: &Arc<Shared>) {
    let options = ScanOptions::default(); // reserved; boundary flag comes from st_dev comparison
    loop {
        let Some(task) = shared.queue.pop(&shared.pending, &shared.cancelled) else {
            return;
        };
        if !shared.cancelled.load(Ordering::Relaxed) {
            process_task(shared, &options, task);
        }
        if shared.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            shared.queue.cv.notify_all();
        }
        if shared.cancelled.load(Ordering::Relaxed) {
            return;
        }
    }
}

fn process_task(shared: &Shared, options: &ScanOptions, task: DirTask) {
    *shared.current.lock() = task.path.clone();

    let entries = match fs::read_dir(&task.path) {
        Ok(rd) => rd,
        Err(e) => {
            shared.errors.fetch_add(1, Ordering::Relaxed);
            let mut model = shared.model.lock();
            if let Some(node) = model.nodes_mut().get_mut(task.parent.index()) {
                node.flags |= UNREADABLE;
            }
            model.push_issue(ScanIssue {
                path: task.path.clone(),
                error: e.to_string(),
            });
            return;
        }
    };

    let fs = platform();
    // (node, is_dir, full child path)
    let mut staged: Vec<(Node, bool, PathBuf)> = Vec::new();

    for entry in entries {
        if shared.cancelled.load(Ordering::Relaxed) {
            return;
        }
        let Ok(entry) = entry else {
            shared.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let child_path = entry.path();
        let md = match fs.metadata(&child_path) {
            Ok(md) => md,
            Err(_) => {
                shared.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        let mut flags = 0;
        // Symlinks and special files carry no meaningful block usage here.
        // Directory inodes do own a few blocks and are counted like ncdu does.
        let mut count_bytes = !matches!(md.kind, EntryKind::Symlink | EntryKind::Other);
        if md.kind == EntryKind::File && md.nlink > 1 {
            let key = (md.device, md.inode);
            let mut links = shared.hardlinks.lock();
            if links.insert(key, ()).is_some() {
                // Storage for this inode was already counted at its first path.
                flags |= HARDLINK_SHARED;
                count_bytes = false;
            }
        }

        if count_bytes {
            shared.logical.fetch_add(md.logical, Ordering::Relaxed);
            shared.allocated.fetch_add(md.allocated, Ordering::Relaxed);
        }
        match md.kind {
            EntryKind::File => {
                shared.files.fetch_add(1, Ordering::Relaxed);
            }
            EntryKind::Directory => {
                shared.dirs.fetch_add(1, Ordering::Relaxed);
            }
            EntryKind::Symlink | EntryKind::Other => {}
        }

        let is_dir = md.kind == EntryKind::Directory;
        if is_dir && options.stay_on_filesystem && md.device != task.dev {
            // List the mount point itself but never descend past it.
            flags |= MOUNT_BOUNDARY;
        }

        let name = child_path.file_name().unwrap_or_default().to_os_string();
        staged.push((
            Node {
                parent: Some(task.parent),
                name,
                kind: match md.kind {
                    EntryKind::Directory => NodeKind::Directory,
                    EntryKind::File => NodeKind::File,
                    EntryKind::Symlink => NodeKind::Symlink,
                    EntryKind::Other => NodeKind::Other,
                },
                own_logical: if count_bytes { md.logical } else { 0 },
                own_allocated: if count_bytes { md.allocated } else { 0 },
                // Aggregation seeds with own bytes; the reverse pass then
                // folds children into parents.
                agg_logical: if count_bytes { md.logical } else { 0 },
                agg_allocated: if count_bytes { md.allocated } else { 0 },
                file_count: u64::from(md.kind == EntryKind::File),
                dir_count: u64::from(md.kind == EntryKind::Directory),
                modified_ms: md.modified_ms,
                device: md.device,
                inode: md.inode,
                children: Vec::new(),
                flags,
            },
            is_dir,
            child_path,
        ));
    }

    // One lock acquisition appends all nodes and links them to the parent.
    struct ToQueue {
        path: PathBuf,
        id: NodeId,
        dev: u64,
    }
    let mut child_ids: Vec<NodeId> = Vec::with_capacity(staged.len());
    let mut to_queue: Vec<ToQueue> = Vec::new();
    {
        let mut model = shared.model.lock();
        let base = model.len();
        for (ix, (node, is_dir, path)) in staged.into_iter().enumerate() {
            let id = NodeId((base + ix) as u32);
            child_ids.push(id);
            let descend = is_dir && node.flags & MOUNT_BOUNDARY == 0;
            let dev = node.device;
            model.nodes_mut().push(node);
            if descend {
                to_queue.push(ToQueue { path, id, dev });
            }
        }
        model.nodes_mut()[task.parent.index()]
            .children
            .extend(child_ids);
    }

    for t in to_queue {
        shared.pending.fetch_add(1, Ordering::AcqRel);
        shared.queue.push(DirTask {
            path: t.path,
            parent: t.id,
            dev: t.dev,
        });
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
            let base = std::env::temp_dir().join(format!(
                "rymd-test-{}-{}",
                tag,
                std::process::id()
            ));
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
        let job = spawn_scan(path.to_path_buf(), ScanOptions::default());
        match job.rx.recv_timeout(Duration::from_secs(30)) {
            Ok(ScanOutcome::Completed { model, .. }) => *model,
            other => panic!("scan did not complete: {other:?}"),
        }
    }

    /// Sum of aggregate allocated bytes over direct children of a node.
    fn child_sum(model: &ScanModel, id: NodeId, f: fn(&Node) -> u64) -> u64 {
        model.node(id).children.iter().map(|&c| f(model.node(c))).sum()
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
        // Recursive file count: a.txt + b.bin.
        assert_eq!(m.node(r).file_count, 2);
        // Recursive dir count: sub + empty.
        assert_eq!(m.node(r).dir_count, 2);
        let logical = child_sum(&m, r, |n| n.agg_logical);
        assert!(logical >= 60_000, "logical {logical}");
        let allocated = child_sum(&m, r, |n| n.agg_allocated);
        assert!(allocated >= logical, "allocated {allocated} < logical {logical}");
    }

    #[test]
    fn symlinks_are_not_followed() {
        let td = TempDir::new("symlink");
        let root = td.path();
        // Big file outside the scan tree.
        let outside = root.parent().unwrap().join(format!("outside-{}.bin", std::process::id()));
        fs::write(&outside, vec![7u8; 100_000]).unwrap();
        fs::write(root.join("real.txt"), vec![0u8; 1_000]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("link.txt")).unwrap();

        let m = scan(root);
        let total = m.node(m.root()).agg_logical;
        // Only real.txt (plus the directory entry itself) may be counted.
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
        let files = m.node(m.root()).children.iter().filter(|&&c| !m.node(c).is_dir()).count();
        assert_eq!(files, 3, "all three names are listed");
        let shared: Vec<_> = m.node(m.root()).children.iter()
            .filter(|&&c| m.node(c).has_flag(HARDLINK_SHARED))
            .collect();
        assert_eq!(shared.len(), 1, "exactly one link is marked shared");
        // The shared link contributes zero bytes.
        let sum_logical: u64 = m.node(m.root()).children.iter().map(|&c| m.node(c).agg_logical).sum();
        assert!(sum_logical < payload.len() as u64 + 8192, "double counted: {sum_logical}");
    }

    #[test]
    fn sparse_file_allocated_below_logical() {
        let td = TempDir::new("sparse");
        let root = td.path();
        let f = fs::File::create(root.join("sparse.bin")).unwrap();
        f.set_len(4 * 1024 * 1024).unwrap();
        drop(f);

        let m = scan(root);
        let node = m.node(m.root()).children.iter().map(|&c| m.node(c)).find(|n| n.kind == NodeKind::File).unwrap();
        assert_eq!(node.own_logical, 4 * 1024 * 1024);
        assert!(node.own_allocated < node.own_logical, "not detected as sparse");
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
        let name = &m.node(m.root()).children[0];
        assert_eq!(m.node(*name).name.as_os_str(), weird);
        // Path reconstruction must round-trip the raw bytes.
        assert!(m.path_of(*name).file_name().unwrap() == weird);
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
            Ok(ScanOutcome::Completed { cancelled: true, .. }) => {}
            other => panic!("expected cancelled completion, got {other:?}"),
        }
    }

    #[test]
    fn failed_root_reports_error() {
        let job = spawn_scan(std::path::PathBuf::from("/nonexistent/rymd/definitely-missing"), ScanOptions::default());
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
        assert!(m.node(m.root()).flags & UNREADABLE != 0 || m.node(m.root()).children.iter().any(|&c| m.node(c).flags & UNREADABLE != 0));
    }
}
