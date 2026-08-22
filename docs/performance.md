# Performance

This document records what Rymd's scanner and UI actually measure on real
hardware, how those numbers were produced, and what is *not* yet measured.
Numbers without a machine behind them are worth nothing, so every claim
below is labeled:

- **benchmarked** - measured locally, reproducible with `bench/` scripts.
- **implemented / CI validated** - compiles and its tests pass in CI, but
  no local hardware ran it.
- **not yet benchmarked** - deferred until suitable hardware is available.

## Environment (all Linux numbers)

| Item | Value |
| --- | --- |
| CPU | Intel Core i5-9400F @ 2.90 GHz (6 cores / 6 threads) |
| Storage | Intel SSDPEKNW512G8 NVMe (root filesystem) |
| Filesystem | ext4 |
| Kernel | 6.11.0-1027-oem |
| Rust | 1.96.0 |
| Rymd | this tree, release profile (`lto = "fat"`, `codegen-units = 1`) |
| gdu | v5.37.0 (official static binary) |
| dua | 2.42.1 (`cargo install dua-cli`) |
| diskonaut | 0.11.0 (`cargo install`) |

Methodology: hyperfine, 5-7 runs, 1 warm-up run each, warm page cache.
Cold-cache runs require dropping filesystem caches (`sync; echo 3 |
sudo tee /proc/sys/vm/drop_caches`), which needs root; the development
machine for this pass had no root access, so **all scans below are
warm-cache**. The one exception noted for the home directory shows how
large the warm/cold gap is on this NVMe.

Competitor categories, so unlike workloads are not compared as equals:

- **FULL TREE**: builds the whole hierarchy - comparable to what an
  interactive tool must do (`gdu -n`, `dua aggregate`, Rymd).
- **RAW LOWER BOUND**: totals only, retains almost nothing
  (`gdu --summarize`). Shown for context only.
- **diskonaut**: TUI-only; driven through a pty by
  `bench/bench_diskonaut.py` and timed until its post-scan screen draws.
  It is single-threaded and allocates different data structures; treat
  its column as context, not a head-to-head.

## Scanner results (mean ms, lower is better)

Synthetic trees from `bench/make_trees.sh`; harness is
`cargo run --release --bin rymd-bench <path>` which runs the production
scanner end to end (traversal + model build + aggregation).

| Workload | Entries | Rymd (full model) | gdu -n (full) | dua aggregate (full) | gdu -s (lower bound) | diskonaut |
| --- | --- | --- | --- | --- | --- | --- |
| many_small (100k files / 100 dirs) | 100,101 | **57.2** | 82.7 | 59.0 | 82.7 | 265.7 |
| wide (300k files in ONE directory) | 300,002 | **223.1** | 866.9 | 245.3 | 866.3 | 1024.2 |
| tiny_dirs (50k dirs, 1-2 files each) | 125,002 | 152.8 | 148.9 | 202.7 | 145.6 | 427.7 |
| deep (600-deep chain) | 1,201 | **21.3** | 30.9 | 2,260.1 | 30.9 | ~1,617 |
| mixed (realistic random tree) | 2,741 | 11.3¹ | 7.6 | 5.2 | 7.6 | 12.0 |
| sparse + hard links | 21 | 11.1¹ | 5.3 | 1.5 | 5.2 | 4.1 |

¹ These two rows are dominated by fixed process start-up of the bench
binary (dynamic linking of the GUI dependency stack, ~10 ms); the actual
in-process scan of `mixed` is 3.4-4.6 ms and an empty directory scans in
0.6 ms. For the GUI application that start-up cost does not exist per
scan.

### Reading the table

- Rymd wins `many_small`, `wide` and `deep`, ties `tiny_dirs` with gdu
  (within noise), and loses the printed number on tiny trees because the
  CLI harness pays ~10 ms of start-up that Go/Rust-analyzer-free binaries
  do not.
- `wide` is where the architecture change matters most: the previous
  shared-stack scanner serialized all 300k stats through one worker
  (721 ms); metadata chunk spilling now parallelizes them (223 ms,
  faster than everything else tested).
- `dua`'s 2.26 s on `deep` reproduces across runs and appears to be a
  pathological case in its scheduler for linear chains, not a general
  property; included un-cherry-picked for exactly that reason.
- Rymd's totals include building the complete GUI model (arena nodes,
  children lists, aggregate pass). On these workloads aggregation costs
  ≤ 15 ms even at 1.4 M nodes.

### Home directory (real world, 1.42 M entries)

| State | Time | Throughput |
| --- | --- | --- |
| First run (mostly cold metadata) | 7,045 ms | ~200 k entries/s |
| Fully warm repeats | ~815 ms | ~1.75 M entries/s |

The old shared-queue scanner measured 7,095 ms on the same directory
when partially warm; like-for-like warm numbers were not captured before
the rewrite, so the warm-state improvement factor is not claimed beyond
the synthetic-tree comparisons above. Peak RSS for the 1.42 M-entry
model: ~280 MiB.

## Search (in-memory, synthetic models)

`rymd-bench --search N` builds N synthetic nodes and benchmarks index
construction and query passes (i5-9400F):

| Nodes | Index build | Full query pass | Notes |
| --- | --- | --- | --- |
| 100k | 7.2 ms | ~1.0-1.5 ms | |
| 1M | 62.8 ms | 10.9-14.6 ms | target "tens of ms" met |
| 5M | 298.6 ms | 48.7-73.2 ms | ~1.05 GiB RSS incl. model |

Queries typed character-by-character refine the previous result set, so
later keystrokes cost a fraction of the full pass; a full 1M-node query
runs off the UI thread in ~11-15 ms, comfortably under one frame.

## UI frame work

Not instrumented with a frame profiler in this pass (no display attached
to the development shell); changes are reasoned about and verified by
code inspection instead:

- Table cells render from prebuilt `RowView`s: zero model locks while
  scrolling, no per-cell lossy conversions or formatting.
- Treemap weights are cached against a view version; renders reuse them.
- Treemap tiles paint as quads with shaped-label caching; no taffy layout
  per tile per frame, no path reconstruction outside tooltips.
- Idle behavior: after a scan finishes its polling task exits; there is
  no periodic timer left running, so the app does not repaint itself when
  idle. Update checks are one-shot and network failures stay silent.
- **Frame times are NOT yet benchmarked on real hardware** - pending a
  session with a display and a GPU profiler.

## Windows status

Implemented / CI validated in this pass:

- Bulk directory enumeration via
  `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` - names,
  attributes, logical size, allocation size (sparse/compression aware),
  timestamps and file ids come from directory records; one handle per
  directory instead of one `CreateFileW` round trip per file.
  Expected syscall reduction versus the previous per-file identity probe:
  from ~3 calls per entry to ~1 per directory plus buffer reads.
- Allocation sizes come from the records' `AllocationSize` field; the
  hardcoded 4096-byte rounding is gone from the bulk path.
- NTFS MFT fast path: boot sector -> `FSCTL_GET_NTFS_VOLUME_DATA` ->
  `$MFT` run list -> streamed record parsing, hierarchy rebuilt from
  parent references. Falls back cleanly to directory traversal without
  administrator rights, on non-NTFS volumes, or on any malformed
  structure.
- Strict parsers (directory records, MFT fixups/attributes/run lists)
  are unit-tested on both Linux and Windows CI, including truncated
  records, lying offsets, invalid fixups, odd name lengths and hostile
  parent references.
- Hard-link counts are absent from bulk directory records, so links are
  not deduplicated on that path (every link counts its bytes); the MFT
  path counts storage once naturally.

**Windows performance has not yet been benchmarked on real Windows
hardware during this optimization pass. Windows scanner implementations
are compile/test validated in CI. No performance claims against
WinDirStat (or anything else) are made; benchmarking is deferred until
reproducible Windows measurements can be run on real hardware.**

## Reproducing

```sh
./bench/make_trees.sh            # deterministic synthetic trees
cargo build --release --bins
RUNS=7 ./bench/run_benchmarks.sh # full suite vs competitors
./target/release/rymd-bench <path>
./target/release/rymd-bench --search 1000000
python3 bench/bench_diskonaut.py <path>
```

Results land in `bench/results/<timestamp>/` (gitignored).

## Known remaining bottlenecks

- Model memory: ~200 bytes/node (OsString names + per-node Vec children).
  A string arena + compact adjacency would cut this substantially but is
  deliberately deferred until the current shape measurably hurts.
- `tiny_dirs` sits at parity with gdu rather than clearly ahead; job
  granularity (one job per small directory) could be amortized by batching
  sibling directories into single jobs.
- Windows numbers entirely pending hardware.
- Frame-time profiling pending a display/GPU session.
