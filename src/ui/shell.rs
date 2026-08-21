//! Application shell: owns business state, wires the toolbar, summary
//! strip, treemap, table and status bar together, and drives scans.
//!
//! Filesystem work never runs here directly: scans happen on scanner
//! threads, destructive operations run through `smol::unblock`, and this
//! view only renders state and forwards user intent.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use gpui::{
    div, px, AnyWindowHandle, AppContext as _, Context, Entity, FocusHandle,
    InteractiveElement as _,
    ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _,
    Window,
};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::table::{TableEvent, TableState};
use gpui_component::{
    button::ButtonVariant,
    dialog::DialogButtonProps,
    notification::NotificationType,
    ActiveTheme as _, Root, WindowExt as _,
};
use gpui_component::v_flex;
use parking_lot::RwLock;

use crate::actions::*;
use crate::model::{NodeId, ScanModel};
use crate::scan::options::{ScanOptions, SizeMetric};
use crate::scan::progress::ScanProgress;
use crate::scan::scanner::{CancelHandle, ScanLive, ScanOutcome};
use crate::state::{AppTab, AppState, ScanState};
use crate::ui::file_table::FileTableDelegate;
use crate::util::format_size::{format_count, format_size};

pub struct AppShell {
    pub state: AppState,
    pub model: Option<Rc<RwLock<ScanModel>>>,
    pub scan_job: Option<ActiveScan>,
    pub progress: ScanProgress,

    pub filter_input: Entity<InputState>,
    pub table: Entity<TableState<FileTableDelegate>>,

    pub focus_handle: FocusHandle,
    pub window_handle: AnyWindowHandle,
    /// UI scale factor applied through the rem size.
    pub ui_scale: f32,
}

pub struct ActiveScan {
    pub live: ScanLive,
    pub cancel: CancelHandle,
    pub rx: std::sync::mpsc::Receiver<ScanOutcome>,
}

impl AppShell {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter this directory"));
        let delegate = FileTableDelegate::new();
        let table = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .col_resizable(true)
                .sortable(true)
                .row_selectable(true)
        });

        // Table events drive selection and navigation.
        cx.subscribe_in(&table, window, |this, _table, event, window, cx| match event {
            TableEvent::SelectRow(row) => this.on_table_select(*row, cx),
            TableEvent::DoubleClickedRow(row) => this.on_table_activate(*row, window, cx),
            _ => {}
        })
        .detach();

        // Filter input changes re-filter the current directory.
        cx.subscribe_in(&filter_input, window, |this, _input, event, _window, cx| {
            if let InputEvent::Change = event {
                let text = this.filter_input.read(cx).value().to_string();
                this.state.filter = text.clone();
                this.table.update(cx, |t, _| t.delegate_mut().set_filter(text));
                cx.notify();
            }
        })
        .detach();

        // Keep light/dark in sync with the system while we run.
        window
            .observe_window_appearance(|window, cx| {
                gpui_component::theme::Theme::sync_system_appearance(Some(window), cx);
            })
            .detach();

        let focus_handle = cx.focus_handle();
        // Receive global shortcuts without clicking first.
        window.focus(&focus_handle);

        Self {
            state: AppState::default(),
            model: None,
            scan_job: None,
            progress: ScanProgress::default(),
            filter_input,
            table,
            focus_handle,
            window_handle: window.window_handle(),
            ui_scale: 1.0,
        }
    }

    // ---- scanning -------------------------------------------------------

    pub fn start_scan(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.scan_job = None;
        self.model = None;
        self.progress = ScanProgress::default();
        let started = Instant::now();
        self.state = AppState::default();
        self.state.scan = ScanState::Scanning { started };

        let job = crate::scan::scanner::spawn_scan(root, ScanOptions::default());
        self.scan_job = Some(ActiveScan {
            live: job.live.clone(),
            cancel: job.cancel,
            rx: job.rx,
        });

        cx.spawn(async move |this, cx| loop {
            smol::Timer::after(Duration::from_millis(100)).await;

            let finished = this
                .update(cx, |shell, cx| {
                    let Some(job) = &shell.scan_job else {
                        return true;
                    };
                    shell.progress = job.live.progress();
                    match job.rx.try_recv() {
                        Ok(outcome) => {
                            shell.finish_scan(outcome, cx);
                            true
                        }
                        Err(TryRecvError::Empty) => {
                            cx.notify();
                            false
                        }
                        Err(TryRecvError::Disconnected) => true,
                    }
                })
                .unwrap_or(true);

            if finished {
                return;
            }
        })
        .detach();

        cx.notify();
    }

    fn finish_scan(&mut self, outcome: ScanOutcome, cx: &mut Context<Self>) {
        self.scan_job = None;
        match outcome {
            ScanOutcome::Failed { path, error } => {
                self.state.scan = ScanState::Failed { path, error };
            }
            ScanOutcome::Completed { model, cancelled: _ } => {
                self.state.filesystem = Some(crate::state::FilesystemStats {
                    free_bytes: model.free_space,
                });
                self.state.scan = ScanState::Complete;
                let rc = Rc::new(RwLock::new(*model));
                self.model = Some(rc.clone());
                let shell_weak = cx.entity().downgrade();
                self.table.update(cx, |t, _| {
                    let d = t.delegate_mut();
                    d.model = Some(rc.clone());
                    d.shell = Some(shell_weak);
                    d.set_directory(NodeId(0));
                });
                // Land the view on the scan root.
                self.state.current_node = Some(NodeId(0));
            }
        }
        cx.notify();
    }

    pub fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picker.await {
                if let Some(path) = paths.into_iter().next() {
                    this.update(cx, |shell, cx| shell.start_scan(path, cx)).ok();
                }
            }
        })
        .detach();
    }

    // ---- navigation -----------------------------------------------------

    pub fn navigate_to(&mut self, node: NodeId, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        {
            let m = model.read();
            if m.node(node).has_flag(crate::model::TOMBSTONED) || !m.node(node).is_dir() {
                return;
            }
            if Some(node) == self.state.current_node {
                return;
            }
        }
        if let Some(cur) = self.state.current_node {
            self.state.history_back.push(cur);
        }
        self.state.history_forward.clear();
        self.state.selected_node = None;
        self.set_current_dir(node, cx);
    }

    fn set_current_dir(&mut self, node: NodeId, cx: &mut Context<Self>) {
        self.state.current_node = Some(node);
        self.table.update(cx, |t, _| t.delegate_mut().set_directory(node));
        cx.notify();
    }

    pub fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(prev) = self.state.history_back.pop() {
            if let Some(cur) = self.state.current_node {
                self.state.history_forward.push(cur);
            }
            self.state.selected_node = None;
            self.set_current_dir(prev, cx);
        }
    }

    pub fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(next) = self.state.history_forward.pop() {
            if let Some(cur) = self.state.current_node {
                self.state.history_back.push(cur);
            }
            self.state.selected_node = None;
            self.set_current_dir(next, cx);
        }
    }

    pub fn go_parent(&mut self, cx: &mut Context<Self>) {
        let parent = match (&self.model, self.state.current_node) {
            (Some(model), Some(cur)) => model.read().node(cur).parent,
            _ => None,
        };
        if let Some(parent) = parent {
            self.navigate_to(parent, cx);
        }
    }

    /// Double-click behavior: directories navigate, files open with the system app.
    pub fn activate_node(&mut self, node: NodeId, cx: &mut Context<Self>) {
        let kind_is_dir = self
            .model
            .as_ref()
            .map(|m| m.read().node(node).is_dir())
            .unwrap_or(false);
        if kind_is_dir {
            self.navigate_to(node, cx);
        } else if let (Some(model), Some(window_cx)) = (&self.model, None::<()>) {
            let _ = window_cx;
            let path = model.read().path_of(node);
            cx.open_with_system(&path);
        }
    }

    // ---- selection ------------------------------------------------------

    pub fn select_node(&mut self, node: NodeId, cx: &mut Context<Self>) {
        self.state.selected_node = Some(node);
        let row = self.table.read(cx).delegate().row_of_node(node);
        if let Some(row) = row {
            self.table.update(cx, |t, cx| {
                if t.selected_row() != Some(row) {
                    t.set_selected_row(row, cx);
                }
            });
        }
        cx.notify();
    }

    fn on_table_select(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(&n) = self.table.read(cx).delegate().rows.get(row) {
            if self.state.selected_node != Some(n) {
                self.state.selected_node = Some(n);
                cx.notify();
            }
        }
    }

    fn on_table_activate(&mut self, row: usize, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(&n) = self.table.read(cx).delegate().rows.get(row) {
            self.activate_node(n, cx);
        }
    }

    // ---- simple actions --------------------------------------------------

    pub fn reveal_node(&mut self, node: NodeId, cx: &mut Context<Self>) {
        if let Some(model) = &self.model {
            let path = model.read().path_of(node);
            cx.reveal_path(&path);
        }
    }

    pub fn copy_node_path(&mut self, node: NodeId, cx: &mut Context<Self>) {
        if let Some(model) = &self.model {
            let path = model.read().path_of(node);
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                path.to_string_lossy().into_owned(),
            ));
        }
    }

    pub fn open_node(&mut self, node: NodeId, cx: &mut Context<Self>) {
        self.activate_node(node, cx);
    }

    // ---- deletion --------------------------------------------------------

    /// Confirmation dialog before moving to Trash. Trash is recoverable,
    /// but it is still a removal, so it always asks first.
    pub fn confirm_trash(&mut self, node: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        if !crate::actions::fs_ops::verify_unchanged(&model.read(), node) {
            self.warn_changed(cx);
            return;
        }
        let (name, size, path, is_dir) = {
            let m = model.read();
            let n = m.node(node);
            (
                n.name.to_string_lossy().into_owned(),
                n.agg_allocated,
                m.path_of(node),
                n.is_dir(),
            )
        };
        let kind = if is_dir { "directory" } else { "file" };
        let body = format!(
            "Move this {kind} to Trash?\n\n{}\n{}\n{}\n\nSpace is reclaimed once the Trash is emptied.",
            name,
            path.to_string_lossy(),
            format_size(size)
        );
        let weak = cx.entity().downgrade();

        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Move to Trash?")
                .confirm()
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Move to Trash")
                        .cancel_text("Cancel"),
                )
                .child(div().max_w(px(420.)).child(body.clone()))
                .on_ok({
                    let w = weak.clone();
                    move |_, _, cx| {
                        let _ = w.update(cx, |shell, cx| shell.trash_node(node, cx));
                        false
                    }
                })
        });
    }

    pub fn trash_node(&mut self, node: NodeId, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        if !crate::actions::fs_ops::verify_unchanged(&model.read(), node) {
            self.warn_changed(cx);
            return;
        }
        let path = model.read().path_of(node);
        if crate::actions::fs_ops::is_protected(&model.read().root_path, &path) {
            self.notify(NotificationType::Error, "The scan root cannot be deleted.", cx);
            return;
        }

        let p = path.clone();
        let task = smol::unblock(move || crate::actions::fs_ops::move_to_trash(&p));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, move |shell, cx| match result {
                Ok(()) => {
                    let name = shell.node_display(node);
                    shell.apply_deletion_and_refresh(node, cx);
                    shell.notify(
                        NotificationType::Success,
                        &format!("Moved to Trash: {name}. Space is reclaimed once Trash is emptied."),
                        cx,
                    );
                }
                Err(e) => shell.notify(
                    NotificationType::Error,
                    &format!("Could not move this item to Trash: {e}. Nothing was deleted."),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    pub fn confirm_delete(&mut self, node: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        if !crate::actions::fs_ops::verify_unchanged(&model.read(), node) {
            self.warn_changed(cx);
            return;
        }
        let (name, size, path) = {
            let m = model.read();
            (
                m.node(node).name.to_string_lossy().into_owned(),
                m.node(node).agg_allocated,
                m.path_of(node),
            )
        };
        let body = format!(
            "{}\n{}\n{}\n\nThis cannot be undone.",
            name,
            path.to_string_lossy(),
            format_size(size)
        );
        let weak = cx.entity().downgrade();

        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Delete permanently?")
                .alert()
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Delete permanently")
                        .cancel_text("Cancel")
                        .ok_variant(ButtonVariant::Danger),
                )
                .child(div().max_w(px(420.)).child(body.clone()))
                .on_ok({
                    let w = weak.clone();
                    move |_, _, cx| {
                        let _ = w.update(cx, |shell, cx| shell.delete_permanently(node, cx));
                        false
                    }
                })
        });
    }

    pub fn confirm_clear_dir(&mut self, node: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        if !crate::actions::fs_ops::verify_unchanged(&model.read(), node) {
            self.warn_changed(cx);
            return;
        }
        let (name, items, size, _path) = {
            let m = model.read();
            let n = m.node(node);
            (
                n.name.to_string_lossy().into_owned(),
                n.file_count + n.dir_count - 1, // exclude the directory itself
                n.agg_allocated,
                m.path_of(node),
            )
        };
        let body = format!(
            "This will permanently delete:\n{} items\n{}\n\nThe {} directory itself will remain.",
            format_count(items.max(0)),
            format_size(size),
            name
        );
        let weak = cx.entity().downgrade();

        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(format!("Clear {}?", name))
                .alert()
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Clear contents")
                        .cancel_text("Cancel")
                        .ok_variant(ButtonVariant::Danger),
                )
                .child(div().max_w(px(420.)).child(body.clone()))
                .on_ok({
                    let w = weak.clone();
                    move |_, _, cx| {
                        let _ = w.update(cx, |shell, cx| shell.clear_contents(node, cx));
                        false
                    }
                })
        });
    }

    fn delete_permanently(&mut self, node: NodeId, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        let path = model.read().path_of(node);
        if crate::actions::fs_ops::is_protected(&model.read().root_path, &path) {
            self.notify(NotificationType::Error, "The scan root cannot be deleted.", cx);
            return;
        }
        let kind = model.read().node(node).kind();
        let task = smol::unblock(move || crate::actions::fs_ops::delete_permanently(&path, kind));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, move |shell, cx| match result {
                Ok(()) => {
                    let name = shell.node_display(node);
                    shell.apply_deletion_and_refresh(node, cx);
                    shell.notify(NotificationType::Success, &format!("Deleted: {name}"), cx);
                }
                Err(e) => shell.notify(
                    NotificationType::Error,
                    &format!("Delete failed: {e}. Nothing changed."),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    fn clear_contents(&mut self, node: NodeId, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        let path = model.read().path_of(node);
        let task = smol::unblock(move || crate::actions::fs_ops::clear_directory(&path));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, move |shell, cx| match result {
                Ok((count, _bytes)) => {
                    // Children vanished on disk: rescan just this subtree by
                    // refreshing views with adjusted numbers is not enough.
                    // Mark the whole directory stale so the user can rescan.
                    shell.notify(
                        NotificationType::Success,
                        &format!("Removed {} items. Rescan to refresh sizes.", count),
                        cx,
                    );
                    shell.refresh_after_clear(node, cx);
                }
                Err(e) => shell.notify(
                    NotificationType::Error,
                    &format!("Clear failed: {e}. Nothing changed."),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    /// Remove every child of `node` from the model (they were deleted on
    /// disk by `clear_directory`).
    fn refresh_after_clear(&mut self, node: NodeId, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        {
            let mut m = model.write();
            let children = m.node(node).children.clone();
            for child in children {
                m.apply_deletion(child);
            }
        }
        self.table.update(cx, |t, _| t.delegate_mut().rebuild_rows());
        cx.notify();
    }

    pub fn apply_deletion_and_refresh(&mut self, node: NodeId, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        let parent = model.write().apply_deletion_return_parent(node);

        // If we were inside the deleted subtree, walk up to the nearest
        // surviving ancestor.
        while let Some(cur) = self.state.current_node {
            if !model.read().node(cur).has_flag(crate::model::TOMBSTONED) {
                break;
            }
            match parent {
                Some(p) => self.state.current_node = Some(p),
                None => break,
            }
        }

        self.state.selected_node = None;
        self.table.update(cx, |t, _| t.delegate_mut().rebuild_rows());
        cx.notify();
    }

    fn warn_changed(&mut self, cx: &mut Context<Self>) {
        self.notify(
            NotificationType::Warning,
            "This item changed since the scan. Rescan before deleting it.",
            cx,
        );
    }

    fn notify(&self, kind: NotificationType, msg: &str, cx: &mut Context<Self>) {
        let _ = self.window_handle.update(cx, |_, window, app| {
            window.push_notification((kind, SharedString::from(msg.to_string())), app);
        });
    }

    fn node_display(&self, node: NodeId) -> String {
        self.model
            .as_ref()
            .map(|m| m.read().node(node).name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    // ---- misc --------------------------------------------------------------

    pub fn set_metric(&mut self, metric: SizeMetric, cx: &mut Context<Self>) {
        if self.state.metric != metric {
            self.state.metric = metric;
            self.table.update(cx, |t, _| t.delegate_mut().set_metric(metric));
            cx.notify();
        }
    }

    pub fn toggle_tab(&mut self, tab: AppTab, cx: &mut Context<Self>) {
        if self.state.active_tab != tab {
            self.state.active_tab = tab;
            cx.notify();
        }
    }

    pub fn focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_input.update(cx, |input, cx| input.focus(window, cx));
    }

    pub fn clear_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state.selected_node = None;
        if !self.state.filter.is_empty() {
            self.state.filter.clear();
            self.table.update(cx, |t, _| t.delegate_mut().set_filter(String::new()));
            self.filter_input
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
    }

    pub fn selected_node(&self) -> Option<NodeId> {
        self.state.selected_node
    }

    pub(crate) fn show_issues_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        let issues: Vec<(String, String)> = model
            .read()
            .issues()
            .iter()
            .map(|i| (i.path.to_string_lossy().into_owned(), i.error.clone()))
            .collect();
        window.open_sheet(cx, move |sheet, _, _| {
            sheet.title("Unreadable paths").child(div().flex().flex_col().gap_2().children(
                issues.iter().map(|(p, e)| {
                    div()
                        .border_b_1()
                        .border_color(gpui::black().opacity(0.08))
                        .py_1()
                        .child(
                            v_flex()
                                .child(div().text_size(px(12.)).truncate().child(p.clone()))
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(gpui::red())
                                        .child(e.clone()),
                                ),
                        )
                }),
            ))
        });
    }

    // ---- action handlers -----------------------------------------------

    fn on_open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_folder(window, cx);
    }

    fn on_rescan(&mut self, _: &Rescan, _: &mut Window, cx: &mut Context<Self>) {
        // Rescan whichever directory is open right now, not the original root.
        let target = match (&self.model, self.state.current_node) {
            (Some(m), Some(cur)) => m.read().path_of(cur),
            (Some(m), None) => m.read().root_path.clone(),
            _ => return,
        };
        self.start_scan(target, cx);
    }

    fn on_focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_filter(window, cx);
    }

    fn on_nav_back(&mut self, _: &NavBack, _: &mut Window, cx: &mut Context<Self>) {
        self.go_back(cx);
    }

    fn on_nav_forward(&mut self, _: &NavForward, _: &mut Window, cx: &mut Context<Self>) {
        self.go_forward(cx);
    }

    fn on_nav_parent(&mut self, _: &NavParent, _: &mut Window, cx: &mut Context<Self>) {
        self.go_parent(cx);
    }

    fn on_open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(n) = self.selected_node() {
            self.activate_node(n, cx);
        }
    }

    fn on_copy_path(&mut self, _: &CopySelectedPath, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(n) = self.selected_node() {
            self.copy_node_path(n, cx);
        }
    }

    fn on_trash_selected(&mut self, _: &TrashSelected, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(n) = self.selected_node() {
            self.confirm_trash(n, window, cx);
        }
    }

    fn on_delete_selected(&mut self, _: &DeleteSelected, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(n) = self.selected_node() {
            self.confirm_delete(n, window, cx);
        }
    }

    fn on_escape(&mut self, _: &ClearContext, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_escape(window, cx);
    }

    fn on_toggle_metric(&mut self, _: &ToggleMetric, _: &mut Window, cx: &mut Context<Self>) {
        let next = match self.state.metric {
            SizeMetric::DiskUsage => SizeMetric::Apparent,
            SizeMetric::Apparent => SizeMetric::DiskUsage,
        };
        self.set_metric(next, cx);
    }

    fn on_show_issues(&mut self, _: &ShowScanIssues, window: &mut Window, cx: &mut Context<Self>) {
        self.show_issues_sheet(window, cx);
    }

    // Payload actions from context menus.

    fn on_open_node(&mut self, a: &OpenNode, _: &mut Window, cx: &mut Context<Self>) {
        self.activate_node(a.0, cx);
    }

    fn on_reveal_node(&mut self, a: &RevealNode, _: &mut Window, cx: &mut Context<Self>) {
        self.reveal_node(a.0, cx);
    }

    fn on_copy_path_node(&mut self, a: &CopyPathNode, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_node_path(a.0, cx);
    }

    fn on_trash_node(&mut self, a: &TrashNode, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_trash(a.0, window, cx);
    }

    fn on_delete_node(&mut self, a: &DeleteNode, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_delete(a.0, window, cx);
    }

    fn on_clear_dir_node(&mut self, a: &ClearDirNode, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_clear_dir(a.0, window, cx);
    }

    fn on_metric_disk_usage(&mut self, _: &MetricDiskUsage, _: &mut Window, cx: &mut Context<Self>) {
        self.set_metric(SizeMetric::DiskUsage, cx);
    }

    fn on_metric_apparent(&mut self, _: &MetricApparent, _: &mut Window, cx: &mut Context<Self>) {
        self.set_metric(SizeMetric::Apparent, cx);
    }

    fn on_scan_path(&mut self, a: &ScanPath, _: &mut Window, cx: &mut Context<Self>) {
        let p = a.0.clone();
        self.start_scan(p, cx);
    }

    // ---- zoom -------------------------------------------------------------

    fn apply_zoom(&mut self, scale: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.ui_scale = scale.clamp(0.6, 2.5);
        window.set_rem_size(px(16.0 * self.ui_scale));
        cx.notify();
    }

    fn on_zoom_in(&mut self, _: &ZoomIn, window: &mut Window, cx: &mut Context<Self>) {
        let next = ((self.ui_scale * 1.1) * 10.0).round() / 10.0;
        self.apply_zoom(next, window, cx);
    }

    fn on_zoom_out(&mut self, _: &ZoomOut, window: &mut Window, cx: &mut Context<Self>) {
        let next = ((self.ui_scale / 1.1) * 10.0).round() / 10.0;
        self.apply_zoom(next, window, cx);
    }

    fn on_zoom_reset(&mut self, _: &ZoomReset, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_zoom(1.0, window, cx);
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let theme = cx.theme();

        div()
            .id("app-shell")
            .key_context("Rymd")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(Self::on_open_folder))
            .on_action(cx.listener(Self::on_rescan))
            .on_action(cx.listener(Self::on_focus_filter))
            .on_action(cx.listener(Self::on_nav_back))
            .on_action(cx.listener(Self::on_nav_forward))
            .on_action(cx.listener(Self::on_nav_parent))
            .on_action(cx.listener(Self::on_open_selected))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_trash_selected))
            .on_action(cx.listener(Self::on_delete_selected))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_toggle_metric))
            .on_action(cx.listener(Self::on_show_issues))
            .on_action(cx.listener(Self::on_open_node))
            .on_action(cx.listener(Self::on_reveal_node))
            .on_action(cx.listener(Self::on_copy_path_node))
            .on_action(cx.listener(Self::on_trash_node))
            .on_action(cx.listener(Self::on_delete_node))
            .on_action(cx.listener(Self::on_clear_dir_node))
            .on_action(cx.listener(Self::on_metric_disk_usage))
            .on_action(cx.listener(Self::on_metric_apparent))
            .on_action(cx.listener(Self::on_scan_path))
            .on_action(cx.listener(Self::on_zoom_in))
            .on_action(cx.listener(Self::on_zoom_out))
            .on_action(cx.listener(Self::on_zoom_reset))
            .child(self.render_titlebar(cx))
            .child(self.render_toolbar(cx))
            .child(self.render_summary(cx))
            .child(self.render_tab_bar(cx))
            .child(self.render_body(cx))
            .child(self.render_status_bar(cx))
            // Modal, sheet and toast layers owned by gpui-component.
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_sheet_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}
