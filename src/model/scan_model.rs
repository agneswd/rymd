/// Arena-based filesystem tree model.
///
/// Nodes are stored in a flat `Vec`. Parents are always created before their
/// children, so a child's index is greater than its parent's index. That
/// invariant holds even with the concurrent scanner, and it is what makes the
/// single reverse-pass aggregation correct.
use std::path::PathBuf;
use std::time::SystemTime;

use super::node::{Node, NodeId, TOMBSTONED};
/// A path that could not be read during the scan.
#[derive(Clone, Debug)]
pub struct ScanIssue {
    pub path: PathBuf,
    pub error: String,
}

/// The full result of a scan. Immutable after completion except for
/// deletion updates applied through [`ScanModel::apply_deletion`].
#[allow(dead_code)] // totals()/children()/... are part of the model API used by tests and future views
#[derive(Debug)]
pub struct ScanModel {
    pub root_path: PathBuf,
    pub root_device: u64,
    nodes: Vec<Node>,
    issues: Vec<ScanIssue>,
    /// Free bytes on the scanned filesystem, queried at scan start.
    pub free_space: Option<u64>,
    /// Wall time of the last completed scan.
    pub duration_ms: u64,
    /// Time spent in final aggregation (part of `duration_ms`).
    pub aggregate_ms: f64,
    /// Which backend produced this model ("work-stealing", "ntfs-mft").
    pub backend: &'static str,
    /// True when the scan stopped early because the user cancelled it.
    pub was_cancelled: bool,
}

#[allow(dead_code)]
impl ScanModel {
    pub(crate) fn new(root_path: PathBuf, root_device: u64) -> Self {
        Self {
            root_path,
            root_device,
            nodes: Vec::new(),
            issues: Vec::new(),
            free_space: None,
            duration_ms: 0,
            aggregate_ms: 0.0,
            backend: "",
            was_cancelled: false,
        }
    }

    /// Placeholder used when handing a finished model out of the scanner's
    /// mutex without a deep clone.
    pub(crate) fn empty() -> Self {
        Self::new(PathBuf::new(), 0)
    }

    pub(crate) fn take(&mut self) -> ScanModel {
        std::mem::replace(self, ScanModel::empty())
    }

    pub(crate) fn nodes_mut(&mut self) -> &mut Vec<Node> {
        &mut self.nodes
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    pub fn root(&self) -> NodeId {
        NodeId(0)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn issues(&self) -> &[ScanIssue] {
        &self.issues
    }

    pub(crate) fn push_issue(&mut self, issue: ScanIssue) {
        if self.issues.len() < 5_000 {
            self.issues.push(issue);
        }
    }

    /// Rebuild the absolute path of a node by walking parents back to the
    /// scan root. Only used for display and filesystem actions; never stored
    /// per node.
    pub fn path_of(&self, id: NodeId) -> PathBuf {
        let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
        let mut cur = Some(id);
        while let Some(nid) = cur {
            let node = self.node(nid);
            parts.push(node.name.as_os_str());
            cur = node.parent;
        }
        let mut path = self.root_path.clone();
        for part in parts.iter().rev().skip(1) {
            path.push(part);
        }
        path
    }

    /// Direct children sorted by aggregate allocated size, descending.
    /// Files and directories intermix; callers re-sort per column as needed.
    pub fn children(&self, id: NodeId) -> Vec<NodeId> {
        self.node(id).children.clone()
    }

    /// Aggregate numbers for the current directory.
    pub fn totals(&self, id: NodeId) -> (u64, u64, u64, u64) {
        let n = self.node(id);
        (n.agg_logical, n.agg_allocated, n.file_count, n.dir_count)
    }

    /// Sum of a subtree's aggregates without walking it: aggregation keeps
    /// these fields current on every ancestor.
    pub fn subtree_allocated(&self, id: NodeId) -> u64 {
        self.node(id).agg_allocated
    }

    /// Add aggregated totals up the ancestor chain after a reverse pass or
    /// a deletion adjustment. `sign` is +1 during aggregation, -1 after a
    /// deletion removes a subtree.
    pub(crate) fn adjust_ancestors(
        &mut self,
        start: NodeId,
        logical: i128,
        allocated: i128,
        files: i128,
        dirs: i128,
    ) {
        let mut cur = self.node(start).parent;
        while let Some(pid) = cur {
            let p = &mut self.nodes[pid.index()];
            p.agg_logical = (p.agg_logical as i128 + logical).max(0) as u64;
            p.agg_allocated = (p.agg_allocated as i128 + allocated).max(0) as u64;
            p.file_count = (p.file_count as i128 + files).max(0) as u64;
            p.dir_count = (p.dir_count as i128 + dirs).max(0) as u64;
            cur = p.parent;
        }
    }

    /// Reverse-pass aggregation: parents precede children in the arena, so
    /// walking from the last index to the root finalizes each subtree before
    /// it is added to its parent.
    pub(crate) fn aggregate(&mut self) {
        if self.nodes.is_empty() {
            return;
        }
        for ix in (1..self.nodes.len()).rev() {
            let (logical, allocated, files, dirs, parent) = {
                let n = &self.nodes[ix];
                (
                    n.agg_logical,
                    n.agg_allocated,
                    n.file_count,
                    n.dir_count,
                    n.parent,
                )
            };
            if let Some(pid) = parent {
                let p = &mut self.nodes[pid.index()];
                p.agg_logical += logical;
                p.agg_allocated += allocated;
                p.file_count += files;
                p.dir_count += dirs;
            }
        }
    }

    /// Detach a subtree and return its parent so navigation can recover.
    pub fn apply_deletion_return_parent(&mut self, target: NodeId) -> Option<NodeId> {
        let parent = self.node(target).parent;
        self.apply_deletion(target);
        parent
    }

    /// Remove a deleted entry from the model without rescanning:
    /// detach from the parent, tombstone the subtree, and subtract its
    /// aggregates from every ancestor. Returns false when `target` is the
    /// root or no longer present.
    pub fn apply_deletion(&mut self, target: NodeId) -> bool {
        if target == self.root() || target.index() >= self.nodes.len() {
            return false;
        }
        if self.node(target).flags & TOMBSTONED != 0 {
            return false;
        }
        let (logical, allocated, files, dirs, parent) = {
            let n = &mut self.nodes[target.index()];
            n.flags |= TOMBSTONED;
            (
                n.agg_logical,
                n.agg_allocated,
                n.file_count,
                n.dir_count,
                n.parent,
            )
        };
        // Tombstone every descendant so stale ids are rejected.
        let mut stack = vec![target];
        while let Some(id) = stack.pop() {
            let kids = {
                let n = &mut self.nodes[id.index()];
                n.flags |= TOMBSTONED;
                std::mem::take(&mut n.children)
            };
            stack.extend(kids);
        }
        if let Some(pid) = parent {
            self.nodes[pid.index()].children.retain(|c| *c != target);
        }
        self.adjust_ancestors(
            target,
            -(logical as i128),
            -(allocated as i128),
            -(files as i128),
            -(dirs as i128),
        );
        true
    }

    /// The modified timestamp formatted for tooltips.
    pub fn modified_of(&self, id: NodeId) -> Option<SystemTime> {
        self.node(id).modified_ms.and_then(|ms| {
            SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(ms as u64))
        })
    }
}
