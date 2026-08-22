//! Application actions for menus and keyboard shortcuts.
//!
//! Payload actions carry the target `NodeId` so context-menu items always
//! operate on the row or rectangle that was right-clicked, not on whatever
//! happens to be selected.

pub mod fs_ops;

use crate::model::NodeId;
use gpui::{Action, actions};

actions!(
    rymd,
    [
        OpenFolder,
        Rescan,
        FocusFilter,
        NavBack,
        NavForward,
        NavParent,
        OpenSelected,
        CopySelectedPath,
        TrashSelected,
        DeleteSelected,
        ShowScanIssues,
        CheckForUpdates,
        ToggleMetric,
        MetricDiskUsage,
        MetricApparent,
        StayOnFilesystem,
        ClearContext,
        ZoomIn,
        ZoomOut,
        ZoomReset
    ]
);

/// Start scanning an arbitrary path (quick-scan presets).
#[derive(Action, Clone, PartialEq, Eq)]
#[action(namespace = rymd, no_json)]
pub struct ScanPath(pub std::path::PathBuf);

/// Open a specific node (directory navigation / file open).
#[derive(Action, Clone, PartialEq, Eq)]
#[action(namespace = rymd, no_json)]
pub struct OpenNode(pub NodeId);

/// Reveal a node in the system file manager.
#[derive(Action, Clone, PartialEq, Eq)]
#[action(namespace = rymd, no_json)]
pub struct RevealNode(pub NodeId);

/// Move a node to the trash.
#[derive(Action, Clone, PartialEq, Eq)]
#[action(namespace = rymd, no_json)]
pub struct TrashNode(pub NodeId);

/// Delete a node permanently after confirmation.
#[derive(Action, Clone, PartialEq, Eq)]
#[action(namespace = rymd, no_json)]
pub struct DeleteNode(pub NodeId);

/// Copy the absolute path of a node.
#[derive(Action, Clone, PartialEq, Eq)]
#[action(namespace = rymd, no_json)]
pub struct CopyPathNode(pub NodeId);

/// Delete everything inside a directory, keeping the directory itself.
#[derive(Action, Clone, PartialEq, Eq)]
#[action(namespace = rymd, no_json)]
pub struct ClearDirNode(pub NodeId);
