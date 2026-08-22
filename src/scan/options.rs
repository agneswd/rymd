/// Options controlling a scan.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Concurrency {
    /// One worker per available parallelism unit (respects cgroup CPU
    /// quotas on Linux). No arbitrary ceiling.
    Auto,
    Fixed(usize),
}

impl Default for Concurrency {
    fn default() -> Self {
        Concurrency::Auto
    }
}

/// Which byte count drives the treemap, table and summary.
///
/// Both values are always collected during the scan, so switching metric
/// never requires new I/O.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeMetric {
    /// Blocks actually allocated (`st_blocks * 512`). Handles sparse files.
    DiskUsage,
    /// Apparent size (`st_size`).
    Apparent,
}

impl SizeMetric {
    pub fn pick(self, logical: u64, allocated: u64) -> u64 {
        match self {
            SizeMetric::DiskUsage => allocated,
            SizeMetric::Apparent => logical,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Do not descend into directories that live on another filesystem
    /// (different `st_dev`). The mount point itself is still listed.
    pub stay_on_filesystem: bool,
    /// Worker count for the scan pool.
    pub concurrency: Concurrency,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            stay_on_filesystem: true,
            concurrency: Concurrency::Auto,
        }
    }
}
