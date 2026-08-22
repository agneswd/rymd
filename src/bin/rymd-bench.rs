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
    let Some(root) = args.next().map(PathBuf::from) else {
        eprintln!("usage: rymd-bench <path> [--runs N] [--json]");
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
