use std::sync::Arc;

use std::path::PathBuf;

use gpui::{px, size, AppContext as _, Application, Bounds, KeyBinding, WindowBounds, WindowOptions};
use gpui_component::Root;
use gpui_component_assets::Assets;

mod actions;

mod model;
mod scan;
mod state;
mod treemap;
mod ui;
mod util;

use actions::*;
use ui::shell::AppShell;

use gpui::actions;

actions!(rymd, [Quit]);

fn main() {
    let app = Application::new().with_assets(Assets);

    app.run(move |cx| {
        gpui_component::init(cx);

        cx.bind_keys([
            KeyBinding::new("ctrl-o", OpenFolder, Some("Rymd")),
            KeyBinding::new("ctrl-r", Rescan, Some("Rymd")),
            KeyBinding::new("ctrl-f", FocusFilter, Some("Rymd")),
            KeyBinding::new("alt-left", NavBack, Some("Rymd")),
            KeyBinding::new("alt-right", NavForward, Some("Rymd")),
            KeyBinding::new("backspace", NavParent, Some("Rymd")),
            KeyBinding::new("enter", OpenSelected, Some("Rymd")),
            KeyBinding::new("delete", TrashSelected, Some("Rymd")),
            KeyBinding::new("shift-delete", DeleteSelected, Some("Rymd")),
            KeyBinding::new("ctrl-c", CopySelectedPath, Some("Rymd")),
            KeyBinding::new("escape", ClearContext, Some("Rymd")),
            KeyBinding::new("ctrl-equal", ZoomIn, Some("Rymd")),
            KeyBinding::new("ctrl-plus", ZoomIn, Some("Rymd")),
            KeyBinding::new("ctrl-minus", ZoomOut, Some("Rymd")),
            KeyBinding::new("ctrl-0", ZoomReset, Some("Rymd")),
        ]);

        cx.on_action(|_: &Quit, cx| cx.quit());

        let bounds = Bounds::centered(None, size(px(1240.), px(800.)), cx);
        let initial_path = std::env::args().nth(1).map(PathBuf::from);
        let mut shell_handle = None;
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui_component::TitleBar::title_bar_options()),
                window_min_size: Some(size(px(900.), px(600.))),
                app_id: Some("rymd".into()),
                ..Default::default()
            },
            |window, cx| {
                let shell = cx.new(|cx| AppShell::new(window, cx));
                shell_handle = Some(shell.downgrade());
                cx.new(|cx| Root::new(shell, window, cx))
            },
        )
        .unwrap();

        // Optional immediate scan: `rymd /some/dir`
        if let Some(path) = initial_path {
            if path.is_dir() {
                if let Some(shell) = shell_handle.take() {
                    if let Err(e) = shell.update(cx, |sh, cx| sh.start_scan(path, cx)) {
                        eprintln!("rymd: initial scan kick failed: {e}");
                    }
                } else {
                    eprintln!("rymd: no shell handle captured");
                }
            }
        }

        // Keep the process alive until the window closes.
        cx.activate(true);
    });
}

// Silence unused helper warnings in template code paths.
#[allow(unused)]
fn _unused(_: Arc<()>) {}
