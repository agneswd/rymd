//! The file list table, built on gpui-component's virtualized DataTable.

use gpui::{
    App, Context, IntoElement, ParentElement as _, Styled as _, WeakEntity, Window, div, px,
};
use gpui_component::menu::PopupMenu;
use gpui_component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};

use std::sync::Arc;

use parking_lot::RwLock;

use crate::model::{HARDLINK_SHARED, MOUNT_BOUNDARY, NodeId, NodeKind, ScanModel};
use crate::scan::options::SizeMetric;
use crate::ui::menus;
use crate::ui::shell::AppShell;
use crate::util::format_size::{format_count, format_modified, format_percent, format_size};

pub const COL_NAME: usize = 0;
pub const COL_SIZE: usize = 1;
pub const COL_PERCENT: usize = 2;
pub const COL_ITEMS: usize = 3;
pub const COL_MODIFIED: usize = 4;

/// Lightweight per-row display data, rebuilt only when the directory,
/// filter, sort or metric changes. Rendering reads exclusively from this,
/// so ordinary scrolling never locks the model.
pub struct RowView {
    pub id: NodeId,
    pub name: String,
    pub kind: NodeKind,
    pub flags: u16,
    pub size: u64,
    pub items: u64,
    pub modified_ms: Option<i64>,
}

/// Rows shown by the table for the current directory.
pub struct FileTableDelegate {
    pub model: Option<Arc<RwLock<ScanModel>>>,
    pub rows: Vec<RowView>,
    pub dir: Option<NodeId>,
    pub dir_total: u64,
    pub metric: SizeMetric,
    pub sort_col: usize,
    pub ascending: bool,
    pub filter: String,
    pub columns: Vec<Column>,
    /// Handle back to the shell so context menus can run actions.
    pub shell: Option<WeakEntity<AppShell>>,
}

impl Default for RowView {
    fn default() -> Self {
        Self {
            id: NodeId(0),
            name: String::new(),
            kind: NodeKind::File,
            flags: 0,
            size: 0,
            items: 0,
            modified_ms: None,
        }
    }
}

impl FileTableDelegate {
    pub fn new() -> Self {
        Self {
            model: None,
            rows: Vec::new(),
            dir: None,
            dir_total: 0,
            metric: SizeMetric::DiskUsage,
            sort_col: COL_SIZE,
            ascending: false,
            filter: String::new(),
            shell: None,
            columns: vec![
                Column::new("name", "Name").width(px(340.)).sortable(),
                Column::new("size", "Size")
                    .width(px(110.))
                    .text_right()
                    .sortable()
                    .descending(),
                Column::new("percent", "%")
                    .width(px(80.))
                    .text_right()
                    .sortable()
                    .descending(),
                Column::new("items", "Items")
                    .width(px(90.))
                    .text_right()
                    .sortable()
                    .descending(),
                Column::new("modified", "Modified")
                    .width(px(130.))
                    .sortable()
                    .descending(),
            ],
        }
    }

    /// Rebuild rows from the model for `dir`, applying filter and current
    /// sort. The model is read once; everything the renderer needs is
    /// copied into [`RowView`] so painting never touches it again.
    /// Point the table at `dir` and rebuild from the model.
    pub fn set_directory(&mut self, dir: NodeId) {
        self.dir = Some(dir);
        self.rebuild_rows();
    }

    pub fn rebuild_rows(&mut self) {
        let Some(model) = &self.model else { return };
        let Some(dir) = self.dir else { return };
        let m = model.read();
        let node = m.node(dir);
        self.dir_total = match self.metric {
            SizeMetric::DiskUsage => node.agg_allocated,
            SizeMetric::Apparent => node.agg_logical,
        };

        let filter = crate::search::normalize_query(&self.filter);
        let metric = self.metric;
        let mut rows: Vec<RowView> = node
            .children
            .iter()
            .map(|&c| {
                let n = m.node(c);
                RowView {
                    id: c,
                    name: n.name.to_string_lossy().into_owned(),
                    kind: n.kind,
                    flags: n.flags,
                    size: metric.pick(n.agg_logical, n.agg_allocated),
                    items: n.file_count + n.dir_count,
                    modified_ms: n.modified_ms,
                }
            })
            .filter(|row| {
                filter.is_empty() || {
                    let normalized = crate::search::normalize_query(&row.name);
                    memmem_like(&normalized, &filter)
                }
            })
            .collect();

        let asc = self.ascending;
        match self.sort_col {
            COL_NAME => {
                // Case-insensitive by normalized form; directories first.
                rows.sort_by(|a, b| dir_first(a, b).then(a.name.cmp(&b.name)));
                if !asc {
                    rows.reverse();
                }
            }
            COL_SIZE | COL_PERCENT => {
                rows.sort_by_key(|r| std::cmp::Reverse(r.size));
                if asc {
                    rows.reverse();
                }
            }
            COL_ITEMS => {
                rows.sort_by_key(|r| std::cmp::Reverse(r.items));
                if asc {
                    rows.reverse();
                }
            }
            COL_MODIFIED => {
                rows.sort_by_key(|r| std::cmp::Reverse(r.modified_ms));
                if asc {
                    rows.reverse();
                }
            }
            _ => {}
        }
        self.rows = rows;
    }

    pub fn set_metric(&mut self, metric: SizeMetric) {
        self.metric = metric;
        self.rebuild_rows();
    }

    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.rebuild_rows();
    }

    pub fn row_of_node(&self, node: NodeId) -> Option<usize> {
        self.rows.iter().position(|r| r.id == node)
    }
}

fn dir_first(a: &RowView, b: &RowView) -> std::cmp::Ordering {
    let a_dir = a.kind == NodeKind::Directory;
    let b_dir = b.kind == NodeKind::Directory;
    b_dir.cmp(&a_dir)
}

/// Case-insensitive substring test on already-normalized bytes.
fn memmem_like(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

impl Default for FileTableDelegate {
    fn default() -> Self {
        Self::new()
    }
}

impl TableDelegate for FileTableDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> &Column {
        &self.columns[col_ix]
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) {
        self.sort_col = col_ix;
        self.ascending = matches!(sort, ColumnSort::Ascending);
        self.rebuild_rows();
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.rows.get(row_ix) else {
            return div().into_any_element();
        };
        let theme = cx.theme();

        match col_ix {
            COL_NAME => {
                let icon = match row.kind {
                    NodeKind::Directory => IconName::Folder,
                    NodeKind::File => IconName::File,
                    NodeKind::Symlink => IconName::ExternalLink,
                    NodeKind::Other => IconName::File,
                };
                let mut cell = h_flex().gap_1p5().items_center().overflow_hidden().child(
                    Icon::new(icon)
                        .small()
                        .text_color(if row.kind == NodeKind::Directory {
                            theme.primary
                        } else {
                            theme.muted_foreground
                        }),
                );
                cell = cell.child(div().truncate().child(row.name.clone()));
                if row.flags & HARDLINK_SHARED != 0 {
                    cell = cell.child(
                        Icon::new(IconName::Copy)
                            .xsmall()
                            .text_color(theme.muted_foreground),
                    );
                }
                if row.flags & MOUNT_BOUNDARY != 0 {
                    cell = cell.child(Icon::new(IconName::Globe).xsmall().text_color(theme.info));
                }
                cell.into_any_element()
            }
            COL_SIZE => div()
                .text_right()
                .child(format_size(row.size))
                .into_any_element(),
            COL_PERCENT => div()
                .text_right()
                .text_color(theme.muted_foreground)
                .child(format_percent(row.size, self.dir_total))
                .into_any_element(),
            COL_ITEMS => {
                if row.kind == NodeKind::Directory {
                    div()
                        .text_right()
                        .child(format_count(row.items))
                        .into_any_element()
                } else {
                    div()
                        .text_right()
                        .text_color(theme.muted_foreground)
                        .child("-")
                        .into_any_element()
                }
            }
            COL_MODIFIED => div()
                .child(format_modified(row.modified_ms, chrono_now_ms()))
                .into_any_element(),
            _ => "".into_any_element(),
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(row) = self.rows.get(row_ix) else {
            return menu;
        };
        menus::node_context_menu(
            menu,
            row.id,
            row.kind == NodeKind::Directory,
            row.flags & HARDLINK_SHARED != 0,
        )
    }
}

fn chrono_now_ms() -> i64 {
    use chrono::Utc;
    Utc::now().timestamp_millis()
}
