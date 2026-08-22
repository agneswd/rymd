//! The file list table, built on gpui-component's virtualized DataTable.

use std::rc::Rc;

use gpui::{
    App, Context, IntoElement, ParentElement as _, Styled as _, WeakEntity, Window, div, px,
};
use gpui_component::menu::PopupMenu;
use gpui_component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};

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

/// Rows shown by the table for the current directory. The delegate keeps a
/// materialized `Vec<NodeId>` so sorting and filtering never touch the model.
pub struct FileTableDelegate {
    pub model: Option<Rc<RwLock<ScanModel>>>,
    pub rows: Vec<NodeId>,
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

    /// Rebuild rows from the model for `dir`, applying filter and current sort.
    pub fn set_directory(&mut self, dir: NodeId) {
        self.dir = Some(dir);
        self.rebuild_rows();
    }

    pub fn rebuild_rows(&mut self) {
        let Some(model) = &self.model else { return };
        let Some(dir) = self.dir else { return };
        let m = model.read();
        self.dir_total = match self.metric {
            SizeMetric::DiskUsage => m.node(dir).agg_allocated,
            SizeMetric::Apparent => m.node(dir).agg_logical,
        };

        let filter = self.filter.to_lowercase();
        let mut kids: Vec<NodeId> = m
            .node(dir)
            .children
            .iter()
            .copied()
            .filter(|&c| {
                if filter.is_empty() {
                    true
                } else {
                    m.node(c)
                        .name
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&filter)
                }
            })
            .collect();

        let metric = self.metric;
        let asc = self.ascending;
        let dir_id = dir;
        kids.sort_by(|&a, &b| {
            let na = m.node(a);
            let nb = m.node(b);
            // Directories always sort above files within name ordering.
            let ord = match self.sort_col {
                COL_NAME => na.name.to_string_lossy().cmp(&nb.name.to_string_lossy()),
                COL_SIZE | COL_PERCENT => metric
                    .pick(nb.agg_logical, nb.agg_allocated)
                    .cmp(&metric.pick(na.agg_logical, na.agg_allocated)),
                COL_ITEMS => (nb.file_count + nb.dir_count).cmp(&(na.file_count + na.dir_count)),
                COL_MODIFIED => nb.modified_ms.cmp(&na.modified_ms),
                _ => std::cmp::Ordering::Equal,
            };
            if self.sort_col == COL_NAME {
                if asc { ord } else { ord.reverse() }
            } else {
                if asc { ord.reverse() } else { ord }
            }
        });
        let _ = dir_id;
        self.rows = kids;
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
        self.rows.iter().position(|&r| r == node)
    }

    fn cell_size(&self, node: NodeId) -> u64 {
        let Some(model) = &self.model else { return 0 };
        let m = model.read();
        let n = m.node(node);
        self.metric.pick(n.agg_logical, n.agg_allocated)
    }
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
        let Some(&node) = self.rows.get(row_ix) else {
            return div().into_any_element();
        };
        let theme = cx.theme();

        match col_ix {
            COL_NAME => {
                let (name, kind, flags) = {
                    let Some(model) = &self.model else {
                        return div().into_any_element();
                    };
                    let m = model.read();
                    let n = m.node(node);
                    (n.name.to_string_lossy().into_owned(), n.kind, n.flags)
                };
                let icon = match kind {
                    NodeKind::Directory => IconName::Folder,
                    NodeKind::File => IconName::File,
                    NodeKind::Symlink => IconName::ExternalLink,
                    NodeKind::Other => IconName::File,
                };
                let mut row = h_flex().gap_1p5().items_center().overflow_hidden().child(
                    Icon::new(icon)
                        .small()
                        .text_color(if kind == NodeKind::Directory {
                            theme.primary
                        } else {
                            theme.muted_foreground
                        }),
                );
                row = row.child(div().truncate().child(name));
                if flags & HARDLINK_SHARED != 0 {
                    row = row.child(
                        Icon::new(IconName::Copy)
                            .xsmall()
                            .text_color(theme.muted_foreground),
                    );
                }
                if flags & MOUNT_BOUNDARY != 0 {
                    row = row.child(Icon::new(IconName::Globe).xsmall().text_color(theme.info));
                }
                row.into_any_element()
            }
            COL_SIZE => div()
                .text_right()
                .child(format_size(self.cell_size(node)))
                .into_any_element(),
            COL_PERCENT => div()
                .text_right()
                .text_color(theme.muted_foreground)
                .child(format_percent(self.cell_size(node), self.dir_total))
                .into_any_element(),
            COL_ITEMS => {
                let items = {
                    let Some(model) = &self.model else {
                        return div().into_any_element();
                    };
                    let m = model.read();
                    let n = m.node(node);
                    if n.is_dir() {
                        n.file_count + n.dir_count
                    } else {
                        return div()
                            .text_right()
                            .text_color(theme.muted_foreground)
                            .child("-")
                            .into_any_element();
                    }
                };
                div()
                    .text_right()
                    .child(format_count(items))
                    .into_any_element()
            }
            COL_MODIFIED => {
                let ms = {
                    let Some(model) = &self.model else {
                        return div().into_any_element();
                    };
                    model.read().node(node).modified_ms
                };
                div()
                    .child(format_modified(ms, chrono_now_ms()))
                    .into_any_element()
            }
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
        let Some(&node) = self.rows.get(row_ix) else {
            return menu;
        };
        let (is_dir, hardlink) = {
            let Some(model) = &self.model else {
                return menu;
            };
            let m = model.read();
            let n = m.node(node);
            (n.is_dir(), n.flags & HARDLINK_SHARED != 0)
        };
        menus::node_context_menu(menu, node, is_dir, hardlink)
    }
}

fn chrono_now_ms() -> i64 {
    use chrono::Utc;
    Utc::now().timestamp_millis()
}
