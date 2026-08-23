//! Duplicates tab table: group headers plus selectable file rows.

use std::collections::HashSet;

use gpui::{
    App, Context, IntoElement, ParentElement as _, Styled as _, WeakEntity, Window, div, px,
};
use gpui_component::checkbox::Checkbox;
use gpui_component::menu::PopupMenu;
use gpui_component::table::{Column, TableDelegate, TableState};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};

use crate::duplicates::DuplicateFile;
use crate::model::NodeId;
use crate::ui::menus;
use crate::ui::shell::AppShell;
use crate::util::format_size::{format_count, format_size};

pub enum DupRow {
    Group {
        group_ix: usize,
        text: String,
        all_selected: bool,
    },
    File {
        file: DuplicateFile,
        size: u64,
        selected: bool,
    },
}

pub struct DuplicatesDelegate {
    pub rows: Vec<DupRow>,
    pub columns: Vec<Column>,
    pub shell: Option<WeakEntity<AppShell>>,
    /// Node ids currently ticked for deletion.
    pub selected: HashSet<NodeId>,
}

impl DuplicatesDelegate {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            columns: vec![
                Column::new("select", "").width(px(48.)),
                Column::new("path", "Path").width(px(560.)),
                Column::new("size", "Size").width(px(110.)).text_right(),
                Column::new("status", "Status").width(px(110.)),
            ],
            shell: None,
            selected: HashSet::new(),
        }
    }

    pub fn set_selected(&mut self, node: NodeId, checked: bool) {
        if checked {
            self.selected.insert(node);
        } else {
            self.selected.remove(&node);
        }
    }

    pub fn selected(&self) -> &HashSet<NodeId> {
        &self.selected
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
    }

    /// Materialize display rows from groups and the current selection.
    pub fn set_groups(
        &mut self,
        groups: &[crate::duplicates::DuplicateGroup],
        selected: &HashSet<crate::model::NodeId>,
    ) {
        let mut rows = Vec::new();
        for (ix, g) in groups.iter().enumerate() {
            let picked = g
                .files
                .iter()
                .filter(|f| selected.contains(&f.node_id))
                .count();
            rows.push(DupRow::Group {
                group_ix: ix,
                text: format!(
                    "{} x {}",
                    format_count(g.files.len() as u64),
                    format_size(g.size)
                ),
                all_selected: picked == g.files.len() && picked > 0,
            });
            for f in &g.files {
                rows.push(DupRow::File {
                    file: f.clone(),
                    size: g.size,
                    selected: selected.contains(&f.node_id),
                });
            }
        }
        self.rows = rows;
    }
}

impl Default for DuplicatesDelegate {
    fn default() -> Self {
        Self::new()
    }
}

impl TableDelegate for DuplicatesDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let Some(row) = self.rows.get(row_ix) else {
            return div().into_any_element();
        };

        match (row, col_ix) {
            (DupRow::Group { .. }, 0) => {
                let (group_ix, checked) = match row {
                    DupRow::Group {
                        group_ix,
                        all_selected,
                        ..
                    } => (*group_ix, *all_selected),
                    _ => unreachable!(),
                };
                let shell = self.shell.clone();
                div()
                    .child(
                        Checkbox::new(("dup-group", group_ix as u64))
                            .checked(checked)
                            .on_click(move |checked, _, cx| {
                                if let Some(sh) = &shell {
                                    let _ = sh.update(cx, |s, cx| {
                                        s.toggle_dup_group(group_ix, *checked, cx)
                                    });
                                }
                            }),
                    )
                    .into_any_element()
            }
            (DupRow::Group { text, .. }, 1) => h_flex()
                .gap_2()
                .items_center()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(
                    Icon::new(IconName::Copy)
                        .small()
                        .text_color(theme.muted_foreground),
                )
                .child(text.clone())
                .into_any_element(),
            (DupRow::Group { .. }, 2) | (DupRow::Group { .. }, 3) => div().into_any_element(),

            (DupRow::File { file, .. }, 0) => {
                let node = file.node_id;
                let checked = matches!(row, DupRow::File { selected: true, .. });
                let shell = self.shell.clone();
                div()
                    .child(
                        Checkbox::new(("dup-check", node.0 as u64))
                            .checked(checked)
                            .on_click(move |checked, _, cx| {
                                if let Some(sh) = &shell {
                                    let _ = sh.update(cx, |s, cx| {
                                        s.toggle_dup_select(node, *checked, cx)
                                    });
                                }
                            }),
                    )
                    .into_any_element()
            }
            (DupRow::File { file, .. }, 1) => div()
                .truncate()
                .child(file.path.to_string_lossy().into_owned())
                .into_any_element(),
            (DupRow::File { size, .. }, 2) => div()
                .text_right()
                .child(format_size(*size))
                .into_any_element(),
            (DupRow::File { file, .. }, 3) => div()
                .text_color(theme.muted_foreground)
                .child(if file.hard_linked { "Hard link" } else { "" })
                .into_any_element(),

            _ => div().into_any_element(),
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        match self.rows.get(row_ix) {
            Some(DupRow::File { file, .. }) => {
                menus::node_context_menu(menu, file.node_id, false, file.hard_linked)
            }
            _ => menu,
        }
    }
}
