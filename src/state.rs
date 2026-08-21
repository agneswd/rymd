//! Business state, kept separate from GPUI component state.

use std::path::PathBuf;
use std::time::Instant;

use crate::model::NodeId;
use crate::scan::options::SizeMetric;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppTab {
    Files,
    Duplicates,
}

#[derive(Clone, Debug)]
pub enum ScanState {
    Idle,
    Scanning { started: Instant },
    Complete,
    Failed { path: PathBuf, error: String },
}

pub struct FilesystemStats {
    pub free_bytes: Option<u64>,
}

pub struct AppState {
    pub scan: ScanState,
    /// Current directory shown by table and treemap.
    pub current_node: Option<NodeId>,
    pub selected_node: Option<NodeId>,
    pub history_back: Vec<NodeId>,
    pub history_forward: Vec<NodeId>,
    pub active_tab: AppTab,
    pub metric: SizeMetric,
    /// Case-insensitive substring filter over direct children.
    pub filter: String,
    pub filesystem: Option<FilesystemStats>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            scan: ScanState::Idle,
            current_node: None,
            selected_node: None,
            history_back: Vec::new(),
            history_forward: Vec::new(),
            active_tab: AppTab::Files,
            metric: SizeMetric::DiskUsage,
            filter: String::new(),
            filesystem: None,
        }
    }
}
