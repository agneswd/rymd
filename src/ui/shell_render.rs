//! Render methods for [`AppShell`]. Split out of shell.rs so the event and
//! state logic stays readable.

use gpui::prelude::FluentBuilder as _;
use gpui::{div, px, Context, IntoElement, ParentElement as _, SharedString, Styled as _};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::menu::{DropdownMenu as _, PopupMenu};
use gpui_component::resizable::{resizable_panel, v_resizable};
use gpui_component::spinner::Spinner;
use gpui_component::tab::{Tab, TabBar};
use gpui_component::table::Table;
use gpui_component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, TitleBar,
};

use crate::model::NodeId;
use crate::scan::options::SizeMetric;
use crate::scan::scanner::ScanOutcome;
use crate::state::{AppTab, ScanState};
use crate::treemap::layout::TreemapItem;
use crate::treemap::view::TreemapElement;
use crate::util::format_size::{format_count, format_size};
use crate::util::paths::shorten_home;
use std::path::PathBuf;

use super::shell::AppShell;

const TOOLBAR_H: f32 = 42.0;

impl AppShell {
    pub(super) fn render_titlebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        TitleBar::new().child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    gpui::img("rymd.svg").size(px(16.)),
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("Rymd"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Disk usage"),
                ),
        )
    }

    pub(super) fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let scanning = self.scan_job.is_some();

        let mut bar = h_flex()
            .w_full()
            .px_2()
            .gap_2()
            .items_center()
            .h(px(TOOLBAR_H))
            .border_b_1()
            .border_color(theme.border);

        // Back / forward history
        let can_back = !self.state.history_back.is_empty();
        let can_fwd = !self.state.history_forward.is_empty();
        bar = bar
            .child(
                Button::new("nav-back")
                    .ghost()
                    .small()
                    .icon(IconName::ChevronLeft)
                    .disabled(!can_back)
                    .tooltip("Back (Alt+Left)")
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                        this.go_back(cx);
                    })),
            )
            .child(
                Button::new("nav-forward")
                    .ghost()
                    .small()
                    .icon(IconName::ChevronRight)
                    .disabled(!can_fwd)
                    .tooltip("Forward (Alt+Right)")
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                        this.go_forward(cx);
                    })),
            );

        // Open directory
        bar = bar.child(
            Button::new("open-folder")
                .outline()
                .small()
                .icon(IconName::FolderOpen)
                .label("Open directory")
                .tooltip("Choose a directory to scan (Ctrl+O)")
                .on_click(cx.listener(|this, _ev: &gpui::ClickEvent, window, cx| {
                    this.choose_folder(window, cx);
                })),
        );

        // Breadcrumb
        bar = bar.child(
            h_flex()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(self.render_breadcrumb(cx)),
        );

        // Filter input
        let filter_input = Input::new(&self.filter_input)
            .prefix(Icon::new(IconName::Search).small())
            .cleanable(true);
        bar = bar.child(div().w(px(220.)).child(filter_input));

        if scanning {
            bar = bar.child(Spinner::new().small());
            bar = bar.child(
                Button::new("stop-scan")
                    .danger()
                    .small()
                    .label("Stop")
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, _| {
                        this.stop_scan_clicked();
                    })),
            );
        } else {
            let can_rescan = self.model.is_some();
            let mut rescan_btn = Button::new("rescan")
                .ghost()
                .small()
                .icon(IconName::Redo2)
                .tooltip("Rescan this directory (Ctrl+R)");
            if can_rescan {
                rescan_btn =
                    rescan_btn.on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                        let root = this.model.as_ref().map(|m| m.read().root_path.clone());
                        if let Some(root) = root {
                            this.start_scan(root, cx);
                        }
                    }));
            }
            bar = bar.child(rescan_btn);
        }

        // Overflow menu
        let metric = self.state.metric;
        bar = bar.child(
            Button::new("more")
                .ghost()
                .small()
                .icon(IconName::EllipsisVertical)
                .dropdown_menu(move |menu, _, _| overflow_menu(menu, metric)),
        );

        bar
    }

    fn render_breadcrumb(&self, cx: &Context<Self>) -> impl IntoElement {
        use gpui_component::breadcrumb::{Breadcrumb, BreadcrumbItem};
        let theme = cx.theme();

        let Some(model) = &self.model else {
            return div().child("");
        };
        let m = model.read();
        let Some(cur) = self.state.current_node else {
            return div().child("");
        };

        // Walk from root down to the current directory.
        let mut chain: Vec<NodeId> = Vec::new();
        let mut cursor = Some(cur);
        while let Some(id) = cursor {
            chain.push(id);
            cursor = m.node(id).parent;
        }
        chain.reverse();

        let items = chain.iter().map(|&id| {
            let label = if id == NodeId(0) {
                shorten_home(&m.root_path).to_string_lossy().into_owned()
            } else {
                m.node(id).name.to_string_lossy().into_owned()
            };
            BreadcrumbItem::new(label).on_click({
                let weak = cx.entity().downgrade();
                move |_, _, cx| {
                    let _ = weak.update(cx, |shell, cx| shell.navigate_to(id, cx));
                }
            })
        });
        div()
            .max_w_full()
            .overflow_hidden()
            .text_color(theme.foreground)
            .child(Breadcrumb::new().children(items))
    }

    pub(super) fn render_summary(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        let (size_val, files_val, dirs_val): (String, String, String) = if self.scan_job.is_some() {
            (
                format_size(self.progress.bytes_allocated),
                format_count(self.progress.files_seen),
                format_count(self.progress.dirs_seen),
            )
        } else if let (Some(model), Some(cur)) = (&self.model, self.state.current_node) {
            let m = model.read();
            let n = m.node(cur);
            (
                format_size(self.state.metric.pick(n.agg_logical, n.agg_allocated)),
                format_count(n.file_count),
                format_count(n.dir_count),
            )
        } else {
            ("-".into(), "-".into(), "-".into())
        };

        let free_val = self
            .state
            .filesystem
            .as_ref()
            .and_then(|f| f.free_bytes)
            .map(format_size)
            .unwrap_or_else(|| "-".to_string());

        fn stat(
            label: &str,
            value: &str,
            theme_fg: gpui::Hsla,
            theme_muted: gpui::Hsla,
        ) -> impl IntoElement {
            h_flex()
                .gap_1p5()
                .items_baseline()
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme_muted)
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme_fg)
                        .child(value.to_string()),
                )
        }

        h_flex()
            .w_full()
            .px_3()
            .py_1p5()
            .gap_6()
            .border_b_1()
            .border_color(theme.border)
            .text_color(theme.foreground)
            .child(stat(
                "Size",
                &size_val,
                theme.foreground,
                theme.muted_foreground,
            ))
            .child(stat(
                "Files",
                &files_val,
                theme.foreground,
                theme.muted_foreground,
            ))
            .child(stat(
                "Directories",
                &dirs_val,
                theme.foreground,
                theme.muted_foreground,
            ))
            .child(stat(
                "Free",
                &free_val,
                theme.foreground,
                theme.muted_foreground,
            ))
            .when_some(
                (!self.state.filter.is_empty()).then(|| self.state.filter.clone()),
                |row, f| {
                    row.child(
                        div()
                            .text_xs()
                            .text_color(theme.warning)
                            .child(SharedString::from(format!("filter: {f}"))),
                    )
                },
            )
    }

    pub(super) fn render_tab_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let selected = match self.state.active_tab {
            AppTab::Files => 0usize,
            AppTab::Duplicates => 1,
        };
        let weak = cx.entity().downgrade();
        TabBar::new("main-tabs")
            .selected_index(selected)
            .on_click(move |ix: &usize, _, cx| {
                let tab = match ix {
                    0 => AppTab::Files,
                    _ => AppTab::Duplicates,
                };
                let _ = weak.update(cx, |shell, cx| shell.toggle_tab(tab, cx));
            })
            .child(Tab::new().label("Files"))
            .child(Tab::new().label("Duplicates"))
    }

    pub(super) fn render_body(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let scanning = self.scan_job.is_some();

        if self.state.active_tab == AppTab::Duplicates {
            return self.render_duplicates_placeholder(cx).into_any_element();
        }

        if scanning && self.model.is_none() {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .child(Spinner::new())
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(SharedString::from(format!(
                            "Scanning {}...",
                            self.progress.current_path.to_string_lossy()
                        ))),
                )
                .into_any_element();
        }

        if let ScanState::Failed { path, error } = &self.state.scan {
            return self
                .render_error_state(path.clone(), error.clone(), cx)
                .into_any_element();
        }

        if self.model.is_none() {
            return self.render_empty_state(cx).into_any_element();
        }

        self.render_files_split(cx).into_any_element()
    }

    fn render_empty_state(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                Icon::new(IconName::FolderOpen)
                    .size_8()
                    .text_color(theme.muted_foreground),
            )
            .child(
                div()
                    .text_lg()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child("Choose a directory"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("See what is using your disk space."),
            )
            .child(
                Button::new("empty-open")
                    .primary()
                    .label("Open directory")
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, window, cx| {
                        this.choose_folder(window, cx);
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .mt_1()
                    .children(self.empty_state_presets(cx)),
            )
    }

    /// One-click scans for common starting points.
    fn empty_state_presets(&self, cx: &Context<Self>) -> Vec<gpui::AnyElement> {
        let mut out = Vec::new();
        for (ix, (label, path)) in quick_scan_presets().into_iter().enumerate() {
            out.push(
                Button::new(("preset", ix as u64))
                    .outline()
                    .small()
                    .label(label)
                    .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                        this.start_scan(path.clone(), cx);
                    }))
                    .into_any_element(),
            );
        }
        out
    }

    fn render_error_state(
        &self,
        path: PathBuf,
        error: String,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                div()
                    .text_base()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child("Could not scan this directory"),
            )
            .child(div().text_sm().text_color(theme.danger).child(error))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(path.to_string_lossy().into_owned()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .mt_2()
                    .child(
                        Button::new("err-choose")
                            .outline()
                            .label("Choose another directory")
                            .on_click(cx.listener(|this, _: &gpui::ClickEvent, window, cx| {
                                this.choose_folder(window, cx);
                            })),
                    )
                    .child({
                        Button::new("err-retry")
                            .primary()
                            .label("Try again")
                            .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                                let p = path.clone();
                                this.start_scan(p, cx);
                            }))
                    }),
            )
    }

    fn render_duplicates_placeholder(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        if self.state.duplicates_computing {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .child(Spinner::new())
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child("Hashing candidate files..."),
                )
                .into_any_element();
        }

        let Some(ds) = &self.state.duplicates else {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child("Find copies of the same file by size, then BLAKE3 content hashes."),
                )
                .child(
                    Button::new("find-dups")
                        .primary()
                        .label("Find duplicate files")
                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.ensure_duplicates(cx);
                        })),
                )
                .into_any_element();
        };

        if ds.groups.is_empty() {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child("No duplicate files found above 1 MiB."),
                )
                .into_any_element();
        }

        let (sel_nodes, sel_bytes) = self.dup_selection(cx);
        let sel_count = sel_nodes.len();
        let mut col = v_flex().flex_1().min_h_0();

        if sel_count > 0 {
            col = col.child(
                h_flex()
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div().text_sm().font_weight(gpui::FontWeight::MEDIUM).child(
                            SharedString::from(format!(
                                "{} selected · {}",
                                format_count(sel_count as u64),
                                crate::util::format_size::format_size(sel_bytes)
                            )),
                        ),
                    )
                    .flex_1(),
            );
        }

        let table_el = div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(Table::new(&self.dup_table).stripe(false))
            .into_any_element();
        col = col.child(table_el);

        let mut footer = h_flex()
            .px_2()
            .py_1p5()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_color(theme.border);

        let total_groups = ds.groups.len();
        let reclaimable: u64 = ds.groups.iter().map(|g| g.reclaimable).sum();
        footer = footer.child(
            div().text_xs().text_color(theme.muted_foreground).child(
                SharedString::from(format!(
                    "{} groups · {} reclaimable",
                    format_count(total_groups as u64),
                    crate::util::format_size::format_size(reclaimable)
                )),
            ),
        );

        if sel_count > 0 {
            footer = footer
                .child(
                    Button::new("dup-trash")
                        .outline()
                        .small()
                        .label("Move to Trash...")
                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, window, cx| {
                            this.confirm_trash_duplicates(window, cx);
                        })),
                )
                .child(
                    Button::new("dup-delete")
                        .danger()
                        .small()
                        .label("Delete permanently...")
                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, window, cx| {
                            this.confirm_delete_duplicates(window, cx);
                        })),
                );
        }

        col = col.child(footer);
        col.into_any_element()
    }

    fn render_files_split(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();

        // Treemap items for the current directory.
        let treemap_el: gpui::AnyElement = {
            let (items, dir_total, selected) = self.treemap_inputs();
            TreemapElement::new(
                items,
                self.model.clone().expect("model present"),
                dir_total,
                self.state.metric,
                selected,
                cx.entity().downgrade(),
                self.view_version,
            )
            .rounded(px(4.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .into_any_element()
        };

        let table_el = div()
            .flex_1()
            .overflow_hidden()
            .child(Table::new(&self.table).stripe(false))
            .into_any_element();

        v_resizable("files-split")
            .child(
                resizable_panel()
                    .size(px(340.))
                    .size_range(px(80.)..px(4000.))
                    .child(treemap_el),
            )
            .child(table_el)
            .into_any_element()
    }

    /// Treemap weights honor the active name filter like the table does.
    fn treemap_inputs(&self) -> (Vec<TreemapItem>, u64, Option<u32>) {
        let Some(model) = &self.model else {
            return (Vec::new(), 0, None);
        };
        let m = model.read();
        let Some(dir) = self.state.current_node else {
            return (Vec::new(), 0, None);
        };
        let metric = self.state.metric;
        let n = m.node(dir);
        let dir_total = metric.pick(n.agg_logical, n.agg_allocated);
        let filter = self.state.filter.to_lowercase();
        let items: Vec<TreemapItem> = n
            .children
            .iter()
            .filter(|&c| {
                filter.is_empty()
                    || m.node(*c)
                        .name
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&filter)
            })
            .map(|&c| {
                let cn = m.node(c);
                TreemapItem {
                    node_id: c.0,
                    weight: metric.pick(cn.agg_logical, cn.agg_allocated) as f64,
                }
            })
            .filter(|i| i.weight > 0.0)
            .collect();
        (items, dir_total, self.state.selected_node.map(|n| n.0))
    }

    pub(super) fn render_status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let scanning = self.scan_job.is_some();

        let left_text = if scanning {
            SharedString::from(format!(
                "{} files · {} directories · {} errors · scanning",
                format_count(self.progress.files_seen),
                format_count(self.progress.dirs_seen),
                format_count(self.progress.errors),
            ))
        } else if let (Some(model), _) = (&self.model, ()) {
            let m = model.read();
            let root = m.node(NodeId(0));
            let duration = if m.duration_ms >= 1000 {
                format!("{:.1} s", m.duration_ms as f64 / 1000.0)
            } else {
                format!("{} ms", m.duration_ms)
            };
            let issues = m.issues().len();
            let cancelled_note = if m.was_cancelled { " · cancelled" } else { "" };
            SharedString::from(format!(
                "{} files · {} directories · {issues} unreadable · scanned in {duration}{cancelled_note}",
                format_count(root.file_count),
                format_count(root.dir_count),
            ))
        } else {
            SharedString::from("No directory scanned")
        };

        let metric_label = match self.state.metric {
            SizeMetric::DiskUsage => "Disk usage",
            SizeMetric::Apparent => "Apparent size",
        };

        let unreadable = self
            .model
            .as_ref()
            .map(|m| m.read().issues().len())
            .unwrap_or(0);

        let mut bar = h_flex()
            .w_full()
            .px_3()
            .h(px(26.))
            .items_center()
            .justify_between()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .text_size(px(11.5))
            .text_color(theme.muted_foreground);

        bar = bar.child(left_text);

        let mut right = h_flex().gap_3().items_center().child(metric_label);
        if unreadable > 0 && !scanning {
            right = right.child(
                Button::new("show-issues")
                    .link()
                    .xsmall()
                    .label(SharedString::from(format!("{unreadable} unreadable")))
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, window, cx| {
                        this.show_issues_sheet(window, cx);
                    })),
            );
        }
        bar = bar.child(right);
        bar
    }

    pub(crate) fn stop_scan_clicked(&mut self) {
        if let Some(job) = &self.scan_job {
            job.cancel.cancel();
        }
    }
}

fn overflow_menu(mut menu: PopupMenu, metric: SizeMetric) -> PopupMenu {
    use crate::actions::{CheckForUpdates, MetricApparent, MetricDiskUsage, ScanPath};

    menu = menu.label("Size metric");
    menu = menu.menu_with_check(
        "Disk usage",
        metric == SizeMetric::DiskUsage,
        Box::new(MetricDiskUsage),
    );
    menu = menu.menu_with_check(
        "Apparent size",
        metric == SizeMetric::Apparent,
        Box::new(MetricApparent),
    );

    menu = menu.separator();
    menu = menu.label(format!("Rymd {}", crate::update::CURRENT));
    menu = menu.menu("Check for updates...", Box::new(CheckForUpdates));

    menu = menu.separator();
    menu = menu.label("Quick scan");
    for (label, path) in quick_scan_presets() {
        menu = menu.menu(label, Box::new(ScanPath(path)));
    }
    menu
}

pub(crate) fn quick_scan_presets() -> Vec<(&'static str, std::path::PathBuf)> {
    let mut out: Vec<(&'static str, std::path::PathBuf)> =
        vec![("Filesystem (/)", std::path::PathBuf::from("/"))];
    if let Some(home) = directories::UserDirs::new().map(|d| d.home_dir().to_path_buf()) {
        out.push(("Home", home));
    }
    out.push(("Root home (/root)", std::path::PathBuf::from("/root")));
    out
}

#[allow(unused)]
fn _type_touches(_: Option<ScanOutcome>, _: std::path::PathBuf) {}
