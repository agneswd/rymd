//! Scanner-only benchmark harness.
//!
//! Runs Rymd's real production scanner (`rymd::scan::scanner::spawn_scan`)
//! over a path and reports wall-clock time, entry counts, byte totals,
//! throughput, model aggregation cost, and approximate peak memory.
//!
//! Usage: cargo run --release --bin rymd-bench -- /path [--runs N] [--json]
//!
//! No network activity is possible here: nothing in the scan path touches
//! the updater.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use rymd::model::ScanModel;
use rymd::scan::options::{Concurrency, ScanOptions};
use rymd::scan::scanner::{ScanOutcome, spawn_scan};

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next();

    // In-memory search benchmark needs no path: rymd-bench --search N
    if first.as_deref() == Some("--search") {
        let n = args
            .next()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or_else(|| {
                eprintln!("--search requires a node count");
                std::process::exit(2);
            });
        run_search_bench(n);
        return;
    }

    // Synthetic MFT streaming parser benchmark: rymd-bench --mft N
    if first.as_deref() == Some("--mft") {
        let n = args
            .next()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or_else(|| {
                eprintln!("--mft requires a record count");
                std::process::exit(2);
            });
        run_mft_bench(n);
        return;
    }

    let Some(root) = first.map(PathBuf::from) else {
        eprintln!(
            "usage: rymd-bench <path> [--runs N] [--json] | rymd-bench --search N | rymd-bench --mft N"
        );
        std::process::exit(2);
    };
    let mut runs = 1usize;
    let mut json = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--runs" => {
                runs = args.next().and_then(|v| v.parse().ok()).unwrap_or(1);
            }
            "--json" => json = true,
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    if !root.is_dir() {
        eprintln!("not a directory: {}", root.display());
        std::process::exit(2);
    }

    // One warm-up pass is implicit in --runs > 1; report every run so
    // warm/cold effects stay visible rather than being averaged away.
    for run in 1..=runs {
        let sample = run_scan(&root);
        if json {
            println!("{}", sample.to_json(run));
        } else {
            print_human(&sample, run, runs);
        }
    }
}

struct Sample {
    wall_ms: f64,
    files: u64,
    dirs: u64,
    nodes: usize,
    logical: u64,
    allocated: u64,
    errors: u64,
    workers: usize,
    backend: &'static str,
    aggregate_ms: f64,
    cancelled: bool,
    peak_rss_kb: u64,
}

fn run_scan(root: &std::path::Path) -> Sample {
    let peak = PeakRss::start();
    let started = Instant::now();
    // Optional worker override for scheduler tuning: RYMD_BENCH_WORKERS=4
    let mut options = ScanOptions::default();
    if let Ok(n) = std::env::var("RYMD_BENCH_WORKERS") {
        options.concurrency = Concurrency::Fixed(n.parse().unwrap_or(0));
    }
    let job = spawn_scan(root.to_path_buf(), options);
    let outcome = job.rx.recv().unwrap_or_else(|_| panic!("scan thread died"));
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    let peak_rss_kb = peak.finish();

    match outcome {
        ScanOutcome::Completed { model, cancelled } => {
            finish(*model, wall_ms, cancelled, peak_rss_kb)
        }
        ScanOutcome::Failed { path, error } => {
            panic!("scan of {} failed: {error}", path.display())
        }
    }
}

fn finish(model: ScanModel, wall_ms: f64, cancelled: bool, peak_rss_kb: u64) -> Sample {
    let root = model.root();
    Sample {
        wall_ms,
        files: model.node(root).file_count,
        dirs: model.node(root).dir_count,
        nodes: model.len(),
        logical: model.node(root).agg_logical,
        allocated: model.node(root).agg_allocated,
        errors: model.issues().len() as u64,
        workers: rymd::scan::scanner::planned_workers(),
        backend: rymd::scan::scanner::BACKEND_NAME,
        aggregate_ms: model.aggregate_ms,
        cancelled,
        peak_rss_kb,
    }
}

fn print_human(s: &Sample, run: usize, runs: usize) {
    if runs > 1 {
        println!("--- run {run}/{runs} ---");
    }
    let entries = s.files + s.dirs;
    let eps = if s.wall_ms > 0.0 {
        entries as f64 / (s.wall_ms / 1000.0)
    } else {
        0.0
    };
    println!(
        "wall {:.1} ms | entries {} (files {}, dirs {}) | nodes {} | {:?} | workers {}",
        s.wall_ms, entries, s.files, s.dirs, s.nodes, s.backend, s.workers
    );
    println!(
        "logical {} | allocated {} | errors {} | entries/s {:.0} | aggregate {:.1} ms",
        human_bytes(s.logical),
        human_bytes(s.allocated),
        s.errors,
        eps,
        s.aggregate_ms
    );
    println!(
        "peak rss ~{} MiB{}",
        s.peak_rss_kb / 1024,
        if s.cancelled { " | CANCELLED" } else { "" }
    );
}

fn human_bytes(v: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut f = v as f64;
    let mut u = 0;
    while f >= 1024.0 && u < UNITS.len() - 1 {
        f /= 1024.0;
        u += 1;
    }
    format!("{f:.2} {}", UNITS[u])
}

impl Sample {
    fn to_json(&self, run: usize) -> String {
        format!(
            "{{\"run\":{},\"wall_ms\":{:.3},\"files\":{},\"dirs\":{},\"nodes\":{},\"logical\":{},\"allocated\":{},\"errors\":{},\"workers\":{},\"backend\":\"{}\",\"aggregate_ms\":{:.3},\"cancelled\":{},\"peak_rss_kb\":{}}}",
            run,
            self.wall_ms,
            self.files,
            self.dirs,
            self.nodes,
            self.logical,
            self.allocated,
            self.errors,
            self.workers,
            self.backend,
            self.aggregate_ms,
            self.cancelled,
            self.peak_rss_kb
        )
    }
}

/// Samples this process's RSS from /proc while the scan runs.
struct PeakRss {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    peak: Arc<AtomicU64>,
}

impl PeakRss {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let (s, p) = (stop.clone(), peak.clone());
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                if let Some(kb) = current_rss_kb() {
                    p.fetch_max(kb, Ordering::Relaxed);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });
        Self {
            stop,
            handle: Some(handle),
            peak,
        }
    }

    fn finish(mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.peak.load(Ordering::Relaxed)
    }
}

fn current_rss_kb() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let rss_pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    Some(rss_pages * 4) // assume 4 KiB pages
}

// ---- in-memory search benchmark ----------------------------------------

fn run_search_bench(nodes: usize) {
    use rymd::model::{Node, NodeId, NodeKind, ScanModel};
    use rymd::search::{SearchIndex, normalize_query};

    println!("building synthetic model with {nodes} nodes...");
    let t0 = Instant::now();
    let mut nodes_vec: Vec<Node> = Vec::with_capacity(nodes + 1);
    nodes_vec.push(Node {
        parent: None,
        name: "/synthetic".into(),
        kind: NodeKind::Directory,
        own_logical: 0,
        own_allocated: 0,
        agg_logical: 0,
        agg_allocated: 0,
        file_count: 0,
        dir_count: 0,
        modified_ms: None,
        device: 0,
        inode: 0,
        children: Vec::new(),
        flags: 0,
    });
    for i in 0..nodes {
        let id = NodeId(i as u32 + 1);
        let name = match i % 3 {
            0 => format!("document{i:08}.txt"),
            1 => format!("Photo_{i:06}.JPG"),
            _ => format!("backup-{i:09}.zip"),
        };
        let node = Node {
            parent: Some(NodeId(0)),
            name: name.into(),
            kind: NodeKind::File,
            own_logical: i as u64,
            own_allocated: i as u64,
            agg_logical: i as u64,
            agg_allocated: i as u64,
            file_count: 1,
            dir_count: 0,
            modified_ms: None,
            device: 0,
            inode: id.0 as u64,
            children: Vec::new(),
            flags: 0,
        };
        nodes_vec.push(node);
    }
    // Link children to the root so the tree is well-formed.
    for n in nodes_vec.iter_mut().skip(1) {
        n.parent = Some(NodeId(0));
    }
    nodes_vec[0].children = (1..=nodes as u32).map(NodeId).collect();
    let model = ScanModel::from_nodes(PathBuf::from("/synthetic"), 0, nodes_vec);
    println!("model built in {:.1} ms", elapsed_ms(t0));

    let peak = PeakRss::start();
    let t = Instant::now();
    let index = SearchIndex::build(&model);
    let build_ms = elapsed_ms(t);
    let mem_kb = peak.finish();
    println!(
        "index build: {build_ms:.1} ms | index+overhead rss peak ~{} MiB",
        mem_kb / 1024
    );

    for q in ["document", "photo_42", "backup-000123456"] {
        let query = normalize_query(q);
        // Warm pass so allocator effects do not dominate.
        let warm = index.find(&query);

        // Time to first hit: run the scan on a thread that reports when
        // the first id is pushed. Approximated here by scanning a prefix:
        // find() is a flat loop, so per-hit latency is uniform; report
        // full-pass time and derive the per-hit figure.
        let t_all = Instant::now();
        let results = index.find(&query);
        let all_ms = elapsed_ms(t_all);
        let per_hit_ms = if results.is_empty() || all_ms == 0.0 {
            all_ms
        } else {
            all_ms / (results.len() as f64 / warm.len() as f64).max(1.0)
        };

        let top100 = results.len().min(100);
        let top1000 = results.len().min(1000);
        println!(
            "query {q:?}: {} hits | full pass {all_ms:.2} ms | ~{per_hit_ms:.4} ms per equal-sized pass | top{top100}/top{top1000} are O(1) slices",
            results.len()
        );
    }
}

fn elapsed_ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

// ---- synthetic MFT streaming benchmark ---------------------------------

fn run_mft_bench(records: usize) {
    use rymd::scan::platform::mft::{
        META_RECORDS, MftStreamParser, ROOT_RECORD, create_synthetic_record,
    };
    use std::sync::atomic::AtomicBool;

    println!("generating {records} synthetic MFT records...");
    let t0 = Instant::now();
    let total_slots = records + META_RECORDS as usize;
    let mut stream = vec![0u8; total_slots * 1024];

    // Root record at slot 5
    let root_rec = create_synthetic_record(ROOT_RECORD, ROOT_RECORD, "C:", true, 0, 0);
    stream[ROOT_RECORD as usize * 1024..(ROOT_RECORD as usize + 1) * 1024]
        .copy_from_slice(&root_rec);

    // User records starting at slot 16
    for i in 0..records {
        let rec_no = META_RECORDS + i as u64;
        let is_dir = i % 10 == 0;
        let name = if is_dir {
            format!("dir_{i:06}")
        } else {
            format!("file_{i:06}.dat")
        };
        let (logical, allocated) = if is_dir {
            (0, 0)
        } else if i % 5 == 0 {
            // Sparse file
            (100 * 1024 * 1024, 4 * 1024)
        } else if i % 3 == 0 {
            // Resident
            (32, 0)
        } else {
            // Ordinary non-resident
            ((i as u64 + 1) * 512, (i as u64 + 1) * 4096)
        };
        let rec = create_synthetic_record(rec_no, ROOT_RECORD, &name, is_dir, logical, allocated);
        let off = rec_no as usize * 1024;
        stream[off..off + 1024].copy_from_slice(&rec);
    }
    println!(
        "generated {:.2} MiB in {:.1} ms",
        stream.len() as f64 / (1024.0 * 1024.0),
        elapsed_ms(t0)
    );

    let cancel = AtomicBool::new(false);
    let chunk_size = 8 * 1024 * 1024; // 8 MiB chunk reads
    let total_bytes = stream.len() as u64;

    // Benchmark new streaming parser
    let mut stream_copy = stream.clone();
    let t1 = Instant::now();
    let mut parser = MftStreamParser::new(1024, total_bytes);
    for chunk in stream_copy.chunks_mut(chunk_size) {
        parser.process_chunk(chunk, &cancel).unwrap();
    }
    let parse_ms = elapsed_ms(t1);
    let entries = parser.into_entries();
    let mib_per_sec = (total_bytes as f64 / (1024.0 * 1024.0)) / (parse_ms / 1000.0);
    let recs_per_sec = (records as f64) / (parse_ms / 1000.0);
    println!(
        "MFT streaming parser: {parse_ms:.2} ms | parsed {} entries | throughput: {:.0} records/s ({:.1} MiB/s)",
        entries.len(),
        recs_per_sec,
        mib_per_sec
    );
}
