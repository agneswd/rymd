//! The update flow as the user sees it: the silent startup check, the
//! manual check, the offer dialog, the download card and the restart
//! prompt.
//!
//! No GitHub knowledge lives here. Everything network-facing sits behind
//! `crate::update` and runs on a background thread; this file only moves
//! [`UpdateState`] forward and renders it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, IntoElement, ParentElement as _, SharedString, Styled as _, div, px,
};
use gpui_component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_component::dialog::DialogButtonProps;
use gpui_component::notification::NotificationType;
use gpui_component::progress::Progress;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, WindowExt as _, h_flex,
    v_flex,
};

use crate::ui::shell::AppShell;
use crate::update::{self, Download, UpdateInfo, UpdateState, UpdateStatus};
use crate::util::format_size::format_size;

/// Bytes between UI updates while downloading. Coarse on purpose: the
/// download thread must not wake the GPUI thread for every read.
const PROGRESS_STEP: u64 = 256 * 1024;

impl AppShell {
    // ---- checking --------------------------------------------------------

    /// Ask GitHub whether a newer release exists.
    ///
    /// `manual` is the difference between the two kinds of check: a silent
    /// startup check never reports a failure and never re-offers a version
    /// the user already dismissed, while a check the user asked for always
    /// answers.
    pub fn check_for_update(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.update.is_busy() || (!manual && !update::checks_enabled()) {
            return;
        }
        // An update already found this session needs no second request.
        match &self.update {
            UpdateState::Available(info) => {
                let info = info.clone();
                if manual {
                    self.offer_update(info, cx);
                }
                return;
            }
            // Already downloaded and verified: the only thing left is the
            // restart, so ask for that instead of offering the update again.
            UpdateState::Ready { .. } => {
                if manual {
                    self.prompt_restart(cx);
                }
                return;
            }
            _ => {}
        }

        self.update = UpdateState::Checking;
        cx.notify();

        let current = update::version::current();
        let task = smol::unblock(move || update::check_for_update(&current));
        cx.spawn(async move |this, cx| {
            let status = task.await;
            this.update(cx, |shell, cx| {
                match status {
                    UpdateStatus::UpToDate => {
                        shell.update = UpdateState::Idle;
                        if manual {
                            shell.notify_update(
                                NotificationType::Success,
                                format!(
                                    "You're using the latest version of Rymd ({}).",
                                    update::CURRENT
                                ),
                                cx,
                            );
                        }
                    }
                    UpdateStatus::UpdateAvailable(info) => {
                        let offer =
                            manual || shell.update_dismissed.should_auto_offer(&info.version);
                        shell.update = UpdateState::Available(info.clone());
                        if offer {
                            shell.offer_update(info, cx);
                        }
                    }
                    // A startup check that fails stays completely quiet: no
                    // network is a normal way to run Rymd.
                    UpdateStatus::Unavailable(reason) => {
                        shell.update = UpdateState::Idle;
                        if manual {
                            shell.notify_update(
                                NotificationType::Error,
                                format!("Could not check for updates. {reason}"),
                                cx,
                            );
                        }
                    }
                }
                cx.notify();
            })
        })
        .detach();
    }

    // ---- the offer -------------------------------------------------------

    fn offer_update(&mut self, info: Box<UpdateInfo>, cx: &mut Context<Self>) {
        let kind = update::InstallKind::detect();
        let installable =
            info.is_installable() && update::installer::for_current_install(kind).can_self_update();
        let title = format!("{} is available", info.name);
        let mut body = format!("You are currently using {}.", update::CURRENT);
        if let Some(day) = info
            .published_at
            .as_deref()
            .and_then(|p| p.split('T').next())
        {
            body.push_str(&format!(" Released {day}."));
        }
        let note = (!installable).then(|| kind.manual_reason().to_string());
        let page = info.page_url.clone();
        let version = info.version.clone();
        let weak = cx.entity().downgrade();

        let _ = self.window_handle.update(cx, |_, window, app| {
            window.open_dialog(app, move |dialog, _, _| {
                let (page, note) = (page.clone(), note.clone());
                dialog
                    .title(title.clone())
                    .confirm()
                    .close_button(true)
                    .overlay_closable(true)
                    .button_props(
                        DialogButtonProps::default()
                            .ok_text(if installable {
                                "Update now"
                            } else {
                                "Open release page"
                            })
                            .cancel_text("Later"),
                    )
                    .child(
                        v_flex()
                            .gap_2()
                            .max_w(px(420.))
                            .child(body.clone())
                            .when_some(note, |this, note| {
                                this.child(div().text_sm().opacity(0.75).child(note))
                            })
                            .child(
                                Button::new("update-whats-new")
                                    .ghost()
                                    .small()
                                    .label("What's new")
                                    .on_click({
                                        let page = page.clone();
                                        move |_, _, cx| cx.open_url(&page)
                                    }),
                            ),
                    )
                    .on_cancel({
                        let weak = weak.clone();
                        let version = version.clone();
                        move |_, _, cx| {
                            let _ = weak.update(cx, |shell, _| {
                                shell.update_dismissed.dismiss(&version);
                            });
                            true
                        }
                    })
                    .on_ok({
                        let weak = weak.clone();
                        let page = page.clone();
                        move |_, _, cx| {
                            if installable {
                                let _ =
                                    weak.update(cx, |shell, cx| shell.start_update_download(cx));
                            } else {
                                cx.open_url(&page);
                            }
                            true
                        }
                    })
            });
        });
    }

    // ---- downloading -----------------------------------------------------

    fn start_update_download(&mut self, cx: &mut Context<Self>) {
        let UpdateState::Available(info) = &self.update else {
            return;
        };
        let info = info.clone();
        let handle = Arc::new(Download::default());
        let total = info.asset.as_ref().map(|a| a.size).filter(|s| *s > 0);
        self.update = UpdateState::Downloading {
            info: info.clone(),
            downloaded: 0,
            total,
            download: handle.clone(),
        };
        cx.notify();

        // The download thread reports through a bounded channel and only
        // when the byte count moves a visible amount, so a 20 MB download
        // wakes the UI a few dozen times, not once per read.
        let (tx, rx) = smol::channel::unbounded::<(u64, Option<u64>)>();
        let dl = info.clone();
        let handle_bg = handle.clone();
        let task = smol::unblock(move || {
            let last = AtomicU64::new(0);
            update::download(&dl, &handle_bg, |seen, total| {
                let is_last = total.is_some_and(|t| seen >= t);
                if seen - last.load(Ordering::Relaxed) >= PROGRESS_STEP || is_last {
                    last.store(seen, Ordering::Relaxed);
                    let _ = tx.try_send((seen, total));
                }
            })
        });

        cx.spawn(async move |this, cx| {
            while let Ok((seen, total)) = rx.recv().await {
                this.update(cx, |shell, cx| {
                    if let UpdateState::Downloading {
                        downloaded,
                        total: t,
                        ..
                    } = &mut shell.update
                    {
                        *downloaded = seen;
                        *t = total;
                        cx.notify();
                    }
                })?;
            }
            // The sender is dropped when the download returns, so the
            // result is ready by the time the loop ends.
            let result = task.await;
            this.update(cx, |shell, cx| {
                let UpdateState::Downloading { info, .. } = &shell.update else {
                    return;
                };
                let info = info.clone();
                match result {
                    Ok(artifact) => {
                        shell.update = UpdateState::Ready { info, artifact };
                        shell.prompt_restart(cx);
                    }
                    Err(e) if handle.cancelled.load(Ordering::Relaxed) => {
                        let _ = e;
                        shell.update = UpdateState::Available(info);
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        shell.update = UpdateState::Failed(msg.clone());
                        shell.notify_update(
                            NotificationType::Error,
                            format!("The update could not be downloaded. {msg}"),
                            cx,
                        );
                    }
                }
                cx.notify();
            })
        })
        .detach();
    }

    pub fn cancel_update_download(&mut self, cx: &mut Context<Self>) {
        if let UpdateState::Downloading { download, .. } = &self.update {
            download.cancel();
            cx.notify();
        }
    }

    // ---- installing ------------------------------------------------------

    fn prompt_restart(&mut self, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        let _ = self.window_handle.update(cx, |_, window, app| {
            window.open_dialog(app, move |dialog, _, _| {
                dialog
                    .title("Update ready")
                    .confirm()
                    .close_button(true)
                    .overlay_closable(true)
                    .button_props(
                        DialogButtonProps::default()
                            .ok_text("Restart and update")
                            .cancel_text("Later")
                            .ok_variant(ButtonVariant::Primary),
                    )
                    .child(
                        div()
                            .max_w(px(420.))
                            .child("Rymd needs to restart to finish the update."),
                    )
                    .on_ok({
                        let weak = weak.clone();
                        move |_, _, cx| {
                            let _ = weak.update(cx, |shell, cx| shell.install_update(cx));
                            true
                        }
                    })
            });
        });
    }

    fn install_update(&mut self, cx: &mut Context<Self>) {
        let UpdateState::Ready { info, artifact } = &self.update else {
            return;
        };
        // Re-check the file right before handing it to the installer: it
        // has been sitting on disk since the download finished.
        if let Some(expected) = &info.sha256
            && let Err(e) = update::github::verify_file(artifact, expected)
        {
            let msg = format!("{e:#}");
            self.update = UpdateState::Failed(msg.clone());
            self.notify_update(NotificationType::Error, msg, cx);
            cx.notify();
            return;
        }

        let artifact = artifact.clone();
        match update::install(&artifact) {
            Ok(()) => {
                self.update = UpdateState::Installing;
                cx.quit();
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.update = UpdateState::Failed(msg.clone());
                self.notify_update(
                    NotificationType::Error,
                    format!("The update could not be installed. {msg}"),
                    cx,
                );
                cx.notify();
            }
        }
    }

    // ---- rendering -------------------------------------------------------

    /// A small card over the bottom-right corner while a download runs.
    /// Absent in every other state, so it costs nothing when idle.
    pub fn render_update_progress(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        if let UpdateState::Failed(reason) = &self.update {
            return Some(
                self.update_card(
                    theme,
                    "Update failed",
                    Button::new("update-dismiss")
                        .ghost()
                        .xsmall()
                        .label("Dismiss")
                        .on_click(cx.listener(|shell, _, _, cx| {
                            shell.update = UpdateState::Idle;
                            cx.notify();
                        })),
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(reason.clone())
                        .into_any_element(),
                ),
            );
        }

        let UpdateState::Downloading {
            info,
            downloaded,
            total,
            download,
        } = &self.update
        else {
            return None;
        };
        let cancelling = download.cancelled.load(Ordering::Relaxed);
        let progress = total
            .filter(|t| *t > 0)
            .map(|t| (*downloaded as f32 / t as f32) * 100.);
        let counts = match total {
            Some(t) => format!("{} of {}", format_size(*downloaded), format_size(*t)),
            None => format_size(*downloaded),
        };

        Some(
            self.update_card(
                theme,
                &format!("Updating Rymd to {}", info.version),
                Button::new("update-cancel")
                    .ghost()
                    .xsmall()
                    .label(if cancelling { "Cancelling" } else { "Cancel" })
                    .disabled(cancelling)
                    .on_click(cx.listener(|shell, _, _, cx| shell.cancel_update_download(cx))),
                v_flex()
                    .gap_2()
                    .child(Progress::new("download-progress").value(progress.unwrap_or(0.)))
                    .child(
                        h_flex()
                            .justify_between()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(counts)
                            .when_some(progress, |this, p| {
                                this.child(format!("{}%", p.round() as u32))
                            }),
                    )
                    .into_any_element(),
            ),
        )
    }

    /// The shared shape of the corner card: a title, one action, a body.
    fn update_card(
        &self,
        theme: &gpui_component::theme::Theme,
        title: &str,
        action: Button,
        body: AnyElement,
    ) -> AnyElement {
        div()
            .absolute()
            .bottom(px(48.))
            .right(px(16.))
            .child(
                v_flex()
                    .w(px(300.))
                    .gap_2()
                    .p_3()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .shadow_md()
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(div().font_semibold().child(title.to_string()))
                            .child(action),
                    )
                    .child(body),
            )
            .into_any_element()
    }

    fn notify_update(&self, kind: NotificationType, msg: String, cx: &mut Context<Self>) {
        let _ = self.window_handle.update(cx, |_, window, app| {
            window.push_notification((kind, SharedString::from(msg)), app);
        });
    }
}
