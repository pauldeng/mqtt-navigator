//! Updates in the window: one check at start (and "Check for updates…"), a quiet badge in the
//! title bar, and "Restart to update". The check and the download run in the background. The
//! app never restarts by itself. Errors go to the log. Only the palette command shows them.

use gpui_kit::component::notification::Notification;
use gpui_kit::component::popover::Popover;
use navigator_update::swap::{self, Install, State};
use navigator_update::{Outcome, UpdateMode};

use super::*;

/// What the badge shows.
enum Badge {
    /// The update is staged. "Restart to update" starts the launcher.
    Ready { version: String, install: Install },
    /// This copy cannot update itself. The badge opens the release page.
    Notice {
        version: String,
        url: String,
        reason: String,
    },
}

#[derive(Default)]
pub(super) struct UpdateUi {
    /// A check runs now. The start check and the palette check never overlap.
    checking: bool,
    badge: Option<Badge>,
    /// Set by "Restart to update". The quit handler starts the launcher after it saved the
    /// settings.
    pub(super) restart: Option<Install>,
}

type CheckResult = Result<(Outcome, UpdateMode), navigator_update::Error>;

impl Navigator {
    /// Checks for a new version in the background. `manual`: the user asked (palette), so the
    /// result shows as a notification. The start check is quiet.
    pub(super) fn check_updates(
        &mut self,
        manual: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.update.checking {
            if manual {
                self.notify(Notification::info("A check is running."), window, cx);
            }
            return;
        }
        self.update.checking = true;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { navigator_update::run_check(env!("CARGO_PKG_VERSION")) })
                .await;
            this.update_in(cx, |v, window, cx| {
                v.on_update_result(result, manual, window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn on_update_result(
        &mut self,
        result: CheckResult,
        manual: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update.checking = false;
        let note = match result {
            Ok((Outcome::UpToDate, _)) => Notification::info("MQTT Navigator is up to date."),
            Ok((Outcome::Ready { version }, UpdateMode::SelfUpdate(install))) => {
                let note = Notification::success(format!(
                    "Update {version} is ready. Use the button in the title bar to restart."
                ));
                self.update.badge = Some(Badge::Ready { version, install });
                note
            }
            Ok((
                Outcome::Notice {
                    version,
                    url,
                    reason,
                },
                _,
            )) if navigator_update::is_release_page(&url) => {
                let note = Notification::info(format!("Version {version} is available. {reason}"));
                self.update.badge = Some(Badge::Notice {
                    version,
                    url,
                    reason,
                });
                note
            }
            Ok(_) => Notification::error("The update check gave an unexpected result."),
            Err(e) => {
                tracing::warn!(error = %e, "The update check failed");
                Notification::error(format!("The update check failed. {e}"))
            }
        };
        if manual {
            self.notify(note, window, cx);
        }
        cx.notify();
    }

    /// The first frame is here: tell the launcher that this version runs. The launcher rolls
    /// back a new version that never writes this marker.
    pub(super) fn mark_healthy(&self, cx: &mut Context<Self>) {
        cx.background_executor()
            .spawn(async {
                let Some(dir) = swap::update_dir() else {
                    return;
                };
                // No trial, no marker: a development run leaves no files.
                if matches!(swap::read_state(&dir), State::Started(_))
                    && let Err(e) = swap::write_healthy(&dir, env!("CARGO_PKG_VERSION"))
                {
                    tracing::warn!(error = %e, "Cannot write the health marker");
                }
            })
            .detach();
    }

    /// "Restart to update": quit now. The quit handler starts the launcher.
    fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        if let Some(Badge::Ready { install, .. }) = &self.update.badge {
            self.update.restart = Some(install.clone());
            cx.quit();
        }
    }

    /// The title bar badge. `None`: nothing to show.
    pub(super) fn render_update_badge(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let weak = cx.entity().downgrade();
        Some(match self.update.badge.as_ref()? {
            Badge::Ready { version, .. } => {
                let version = version.clone();
                Popover::new("update-popover")
                    .trigger(
                        Button::new("update-badge")
                            .small()
                            .ghost()
                            .icon(IconName::ArrowUp)
                            .label(format!("Update {version} ready")),
                    )
                    .content(move |_, _, cx| {
                        let (restart_to, later) = (weak.clone(), cx.entity());
                        v_flex()
                            .gap_2()
                            .p_1()
                            .child(
                                div()
                                    .text_sm()
                                    .child(format!("Version {version} is ready to install.")),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("update-restart")
                                            .small()
                                            .primary()
                                            .label("Restart to update")
                                            .on_click(move |_, _, cx| {
                                                restart_to
                                                    .update(cx, |v, cx| v.restart_to_update(cx))
                                                    .ok();
                                            }),
                                    )
                                    .child(
                                        Button::new("update-later")
                                            .small()
                                            .label("Later")
                                            .on_click(move |_, window, cx| {
                                                later.update(cx, |s, cx| s.dismiss(window, cx));
                                            }),
                                    ),
                            )
                            .into_any_element()
                    })
                    .into_any_element()
            }
            Badge::Notice {
                version,
                url,
                reason,
            } => {
                let url = url.clone();
                Button::new("update-badge")
                    .small()
                    .ghost()
                    .icon(IconName::ArrowUp)
                    .label(format!("Update {version} available"))
                    .tooltip(reason.clone())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        // Checked again here: only an https release page of this project opens.
                        if navigator_update::is_release_page(&url) {
                            cx.open_url(&url);
                        }
                    }))
                    .into_any_element()
            }
        })
    }
}
