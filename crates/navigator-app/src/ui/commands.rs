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
            Cmd::ToggleExpand => self.set_expanded(!self.insp.expanded, window, cx),
            Cmd::Decoder(pick) => self.set_decoder(pick, window, cx),
            Cmd::NumberKind(kind) => self.set_number(Some(kind), None, window, cx),
            Cmd::NumberEndian(endian) => self.set_number(None, Some(endian), window, cx),
            Cmd::WrapOn => self.set_wrap(true, cx),
            Cmd::WrapOff => self.set_wrap(false, cx),
            Cmd::ShowAll => self.show_all(cx),
            Cmd::ShowMore => self.show_more(window, cx),
            Cmd::CopyValue => self.copy_value(window, cx),
            Cmd::CopyTopic => self.copy_topic(window, cx),
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
                self.insp.expanded = false;
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
        self.two_lines = two_lines;
        self.settings.ui.row_height = if two_lines {
            RowHeight::TwoLines
        } else {
            RowHeight::OneLine
        };
        self.persist();
        cx.notify();
    }

    /// Opens the command palette (Ctrl/Cmd+K). It lists the actions that are available now.
    pub(super) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let entries = palette::available(&self.facts());
        let cmds: Rc<Vec<Cmd>> = Rc::new(entries.iter().map(|e| e.cmd).collect());
        let facts = self.facts();
        let rows: Rc<Vec<PaletteRow>> = Rc::new(
            entries
                .iter()
                .map(|e| {
                    (
                        e.id,
                        e.label,
                        palette::shortcut_text(e),
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
            let (cmds, weak, rows) = (cmds.clone(), weak.clone(), rows.clone());
            dialog.child(
                PaletteList::new(&state)
                    .items(items)
                    .placeholder("Type a command…")
                    .on_confirm(move |ix, window, cx| {
                        let Some(cmd) = cmds.get(ix.row).copied() else {
                            return;
                        };
                        window.close_dialog(cx);
                        if let Some(Some(reason)) = rows.get(ix.row).map(|r| r.4) {
                            window.push_notification(Notification::warning(reason), cx);
                            return;
                        }
                        let weak = weak.clone();
                        // The action may open a dialog or move the focus: run it after the
                        // palette has closed.
                        window.defer(cx, move |window, cx| {
                            weak.update(cx, |v, cx| v.run_cmd(cmd, window, cx)).ok();
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
