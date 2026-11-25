//! The command palette, the one place that runs actions (`run_cmd`), and the diagnostics.
//! The table of actions is `crate::palette`. Key bindings and the palette call `run_cmd`.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::Ordering;

use gpui_kit::component::notification::Notification;

use super::*;
use crate::diagnostics::{self, Info};
use crate::palette::{self, Cmd};

/// Id, label, shortcut text and "is the current choice" of one palette row.
type PaletteRow = (
    &'static str,
    &'static str,
    Option<String>,
    bool,
    Option<&'static str>,
);

/// What the palette actions act on, taken when the palette opens (see `run_palette_cmd`).
#[derive(Clone, PartialEq)]
pub(super) struct PaletteTarget {
    shown: Option<DetailKey>,
    topic: Option<String>,
    sub: Option<String>,
}

impl Navigator {
    /// Runs one action. Every action of the table is handled here.
    pub(super) fn run_cmd(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        match cmd {
            Cmd::Connect | Cmd::Disconnect | Cmd::CancelConnect => {
                self.toggle_connection(window, cx)
            }
            Cmd::ManageConnections => self.open_sheet(None, false, window, cx),
            Cmd::NewConnection => self.open_sheet(None, true, window, cx),
            Cmd::FocusSubscribe => self.sub_input.update(cx, |i, cx| i.focus(window, cx)),
            Cmd::ViewMessages => self.set_view(View::Messages, window, cx),
            Cmd::ViewTree => self.set_view(View::Tree, window, cx),
            Cmd::DirectionAll => self.set_direction(DirectionFilter::All, cx),
            Cmd::DirectionReceived => self.set_direction(DirectionFilter::Received, cx),
            Cmd::DirectionSent => self.set_direction(DirectionFilter::Sent, cx),
            Cmd::ClearFilters => self.clear_filters(window, cx),
            Cmd::FocusFilter => self.filter_input.update(cx, |i, cx| i.focus(window, cx)),
            Cmd::Pause | Cmd::Resume => self.toggle_pause(cx),
            Cmd::JumpToNewest => self.jump_to_newest(window, cx),
            Cmd::RowHeight1 => self.set_row_height(false, cx),
            Cmd::RowHeight2 => self.set_row_height(true, cx),
            Cmd::ToggleDetails => self.toggle_details(window, cx),
            Cmd::DetailsOpen => self.open_details(window, cx),
            Cmd::ToggleExpand => self.set_expanded(!self.expanded, window, cx),
            Cmd::Decoder(pick) => {
                self.in_pane(window, cx, move |p, w, cx| p.set_decoder(pick, w, cx))
            }
            Cmd::NumberKind(kind) => self.in_pane(window, cx, move |p, w, cx| {
                p.set_number(Some(kind), None, w, cx)
            }),
            Cmd::NumberEndian(endian) => self.in_pane(window, cx, move |p, w, cx| {
                p.set_number(None, Some(endian), w, cx)
            }),
            Cmd::WrapOn => self.in_pane(window, cx, |p, w, cx| p.set_wrap(true, w, cx)),
            Cmd::WrapOff => self.in_pane(window, cx, |p, w, cx| p.set_wrap(false, w, cx)),
            Cmd::ShowAll => self.in_pane(window, cx, |p, w, cx| p.show_all(w, cx)),
            Cmd::ShowMore => self.in_pane(window, cx, |p, w, cx| p.show_more(w, cx)),
            Cmd::CopyValue => self.in_pane(window, cx, |p, w, cx| p.copy_value(w, cx)),
            Cmd::CopyTopic => self.in_pane(window, cx, |p, w, cx| p.copy_topic(w, cx)),
            Cmd::PublishToTopic => self.publish_to_topic(window, cx),
            Cmd::EditAndPublish => self.edit_and_publish(window, cx),
            Cmd::Reply => self.reply(window, cx),
            Cmd::ClearRetained => self.confirm_clear_retained(window, cx),
            Cmd::FocusPublishTopic => self.focus_publish_topic(window, cx),
            // A closed composer opens first: the user must see the draft that goes out.
            Cmd::Send if !self.comp.vis.open => self.open_composer(window, cx),
            Cmd::Send => self.send(window, cx),
            Cmd::PayloadMode(mode) => self.set_payload_mode(mode, window, cx),
            Cmd::FormatJson(pretty) => self.format_json(pretty, window, cx),
            Cmd::NewMessage => self.open_composer(window, cx),
            Cmd::ComposerProperties => self.open_properties(window, cx),
            Cmd::ComposerHistory => self.open_history(window, cx),
            Cmd::CloseComposer => self.close_composer(window, cx),
            Cmd::SubQos(_) | Cmd::SubOptions | Cmd::SubRetry | Cmd::SubRemove => {
                self.run_sub_cmd(cmd, window, cx)
            }
            Cmd::Theme(mode) => self.set_appearance(mode, window, cx),
            Cmd::ThemePreset(id) => self.set_preset(id, window, cx),
            Cmd::MemoryLimit(mib) => self.set_memory_limit(mib, cx),
            Cmd::ResetLayout => {
                self.expanded = false;
                self.expand_restore = None;
                // The next frame sets the pane widths: the details pane may be hidden now.
                self.pane_reset = true;
                cx.notify();
            }
            Cmd::OpenLogFolder => self.open_log_folder(window, cx),
            Cmd::CopyDiagnostics => self.copy_diagnostics(window, cx),
            Cmd::CheckUpdates => self.check_updates(true, window, cx),
            Cmd::UpdateCheckAtStart(on) => {
                self.settings.ui.check_updates = on;
                self.persist();
                cx.notify();
            }
            Cmd::Quit => cx.quit(),
        }
    }

    /// A subscription action of the palette. It acts on the subscription in Show only.
    fn run_sub_cmd(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        let Some(filter) = self.show_only.clone() else {
            return;
        };
        match cmd {
            Cmd::SubQos(qos) => self.set_sub_qos(&filter, qos, cx),
            Cmd::SubOptions => self.open_sub_options(&filter, window, cx),
            Cmd::SubRetry => self.retry_sub(&filter, cx),
            Cmd::SubRemove => self.remove_sub(&filter, cx),
            _ => {}
        }
    }

    pub(super) fn set_direction(&mut self, d: DirectionFilter, cx: &mut Context<Self>) {
        self.direction = d;
        self.apply_filters(cx);
    }

    /// Sets the RAM limit of the store. It applies at once. A lower limit makes the store evict
    /// in small steps (`TopicStore::work`), so the frame loop gets a kick. Core clamps the value,
    /// and the clamped value is the one that shows and persists.
    pub(super) fn set_memory_limit(&mut self, mib: u32, cx: &mut Context<Self>) {
        self.apply_memory_limit(mib);
        self.persist();
        self.kick_work();
        cx.notify();
    }

    /// Gives the store the limit in MiB and keeps the clamped value in the settings.
    pub(super) fn apply_memory_limit(&mut self, mib: u32) {
        let applied = self.store.set_budget(mib as usize * MIB);
        self.settings.ui.store_budget_mib = u32::try_from(applied / MIB).unwrap_or(u32::MAX);
    }

    pub(super) fn set_row_height(&mut self, two_lines: bool, cx: &mut Context<Self>) {
        self.settings.ui.row_height = if two_lines {
            RowHeight::TwoLines
        } else {
            RowHeight::OneLine
        };
        self.persist();
        cx.notify();
    }

    /// What the palette actions act on: the shown message, the selected topic and the
    /// subscription in "Show only".
    pub(super) fn palette_target(&self, cx: &App) -> PaletteTarget {
        PaletteTarget {
            shown: self.shown(cx).map(|s| s.key),
            topic: self.selected_topic().map(|(t, _)| t),
            sub: self.show_only.clone(),
        }
    }

    /// Runs an action that the user chose in the palette. The list is a snapshot from when the
    /// palette opened (`at_open`), and the state can change while it is open: the connection
    /// drops, a new message shows, another subscription is in "Show only". The action runs only
    /// if it still applies to the same object and is still available now. Else it does nothing
    /// and says why ("Disconnect" must never start a connection).
    pub(super) fn run_palette_cmd(
        &mut self,
        cmd: Cmd,
        at_open: &PaletteTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = self.palette_target(cx);
        let moved = match cmd {
            Cmd::CopyValue | Cmd::EditAndPublish | Cmd::Reply => now.shown != at_open.shown,
            // Show all and Show more act on the subject, not on one message.
            Cmd::CopyTopic
            | Cmd::PublishToTopic
            | Cmd::ClearRetained
            | Cmd::ShowAll
            | Cmd::ShowMore => now.topic != at_open.topic,
            Cmd::SubQos(_) | Cmd::SubOptions | Cmd::SubRetry | Cmd::SubRemove => {
                now.sub != at_open.sub
            }
            _ => false,
        };
        let facts = self.facts(cx);
        let why = match palette::COMMANDS.iter().find(|e| e.cmd == cmd) {
            _ if moved => Some("The selection changed. Open the commands again.".to_string()),
            // Copy the value says itself why the value is not ready.
            Some(e) if e.cmd == Cmd::CopyValue => None,
            Some(e) if !e.available(&facts) => Some(format!("“{}” is not available now.", e.label)),
            Some(e) => palette::disabled_reason(e, &facts).map(str::to_string),
            None => None,
        };
        match why {
            Some(text) => window.push_notification(Notification::warning(text), cx),
            None => self.run_cmd(cmd, window, cx),
        }
    }

    /// The user chose a palette row that showed a reason. Say the reason now, or that the state
    /// changed since the palette opened.
    fn palette_cmd_off(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        let facts = self.facts(cx);
        let reason = palette::COMMANDS
            .iter()
            .find(|e| e.cmd == cmd)
            .and_then(|e| palette::disabled_reason(e, &facts))
            .unwrap_or("The state changed. Open the commands again.");
        window.push_notification(Notification::warning(reason), cx);
    }

    /// Opens the command palette (Ctrl/Cmd+K). It lists the actions that are available now.
    pub(super) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let entries = palette::available(&self.facts(cx));
        let target = Rc::new(self.palette_target(cx));
        let cmds: Rc<Vec<Cmd>> = Rc::new(entries.iter().map(|e| e.cmd).collect());
        let facts = self.facts(cx);
        let rows: Rc<Vec<PaletteRow>> = Rc::new(
            entries
                .iter()
                .map(|e| {
                    (
                        e.id,
                        e.label,
                        bound_action(e.cmd)
                            .and_then(|(a, ctx)| shortcut_text(a.as_ref(), ctx, window)),
                        palette::is_current(e, &facts),
                        palette::disabled_reason(e, &facts),
                    )
                })
                .collect(),
        );
        let state = cx.new(|cx| CommandState::new(window, cx));
        let weak = cx.entity().downgrade();
        let first = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                state.update(cx, |s, cx| s.focus(window, cx));
            }
            let items = rows.iter().map(|(id, label, shortcut, current, reason)| {
                let (shortcut, label, id, current, reason) =
                    (shortcut.clone(), *label, *id, *current, *reason);
                CommandItem::new()
                    .label(label)
                    .keywords([id])
                    .child(move |_, cx| {
                        let muted = cx.theme().muted_foreground;
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(div().flex_1().child(label))
                            .when(current, |d| d.child(Icon::new(IconName::Check).small()))
                            .when_some(reason, |d, r| {
                                d.child(div().text_xs().text_color(muted).child(r))
                            })
                            .when_some(shortcut.clone(), |d, s| {
                                d.child(div().text_xs().text_color(muted).child(s))
                            })
                    })
            });
            let (cmds, weak, target, rows) =
                (cmds.clone(), weak.clone(), target.clone(), rows.clone());
            dialog.child(
                PaletteList::new(&state)
                    .items(items)
                    .placeholder("Type a command…")
                    .on_confirm(move |ix, window, cx| {
                        let Some(cmd) = cmds.get(ix.row).copied() else {
                            return;
                        };
                        window.close_dialog(cx);
                        // A row that showed a reason does not run, also if the state changed
                        // since: the user did not choose an action that looked available.
                        let shown_off = rows.get(ix.row).is_some_and(|r| r.4.is_some());
                        let (weak, target) = (weak.clone(), target.clone());
                        // The action may open a dialog or move the focus: run it after the
                        // palette has closed.
                        window.defer(cx, move |window, cx| {
                            weak.update(cx, |v, cx| {
                                if shown_off {
                                    v.palette_cmd_off(cmd, window, cx);
                                } else {
                                    v.run_palette_cmd(cmd, &target, window, cx);
                                }
                            })
                            .ok();
                        });
                    }),
            )
        });
    }

    /// Creates the log folder in the background if it is missing, then opens it.
    fn open_log_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = crate::logging::log_dir() else {
            window.push_notification(Notification::warning("This system has no log folder."), cx);
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let made = cx
                .background_executor()
                .spawn(async move { navigator_core::private_fs::create_dir(&dir).map(|()| dir) })
                .await;
            this.update_in(cx, |_, window, cx| match made {
                Ok(dir) => cx.open_with_system(&dir),
                Err(e) => window.push_notification(
                    Notification::error(format!("Cannot create the log folder: {e}")),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    /// Copies the diagnostics text (see `crate::diagnostics`) to the clipboard.
    fn copy_diagnostics(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let gpu = diagnostics::gpu_text(window.gpu_specs().as_ref().map(|g| {
            (
                g.device_name.as_str(),
                g.driver_name.as_str(),
                g.driver_info.as_str(),
                g.is_software_emulated,
            )
        }));
        let net = &self.net.stats;
        let info = Info::collect(
            gpu,
            &self.status.state,
            self.store.stats(),
            net.drops.load(Ordering::Relaxed),
            net.event_drops.load(Ordering::Relaxed),
        );
        cx.write_to_clipboard(ClipboardItem::new_string(info.text()));
        window.push_notification(Notification::success("Diagnostics copied."), cx);
    }
}

/// The key binding that runs a palette command: its action and the key context of the binding.
/// The palette shows the shortcut from the key map, so it cannot differ from the key.
pub(super) fn bound_action(cmd: Cmd) -> Option<(Box<dyn Action>, Option<&'static str>)> {
    Some(match cmd {
        Cmd::NewConnection => (Box::new(NewConnection), Some(CONTEXT)),
        Cmd::ViewMessages => (Box::new(ShowMessages), Some(CONTEXT)),
        Cmd::ViewTree => (Box::new(ShowTree), Some(CONTEXT)),
        Cmd::FocusFilter => (Box::new(FocusFilter), Some(CONTEXT)),
        Cmd::NewMessage => (Box::new(NewMessage), Some(CONTEXT)),
        Cmd::Send => (Box::new(SendMessage), Some("Composer")),
        Cmd::Quit => (Box::new(crate::Quit), None),
        _ => return None,
    })
}
