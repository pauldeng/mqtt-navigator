//! Profiles, settings and secrets of the main window. All file and keyring work runs in the
//! background. Its result comes back as an event that calls one of these methods.

use gpui_kit::component::WindowExt;
use gpui_kit::component::notification::Notification;
use navigator_core::net::Transport;
use navigator_core::profile::{dedupe_names, export_profiles, import_profiles};
use navigator_core::settings::Load;

use gpui_kit::component::menu::PopupMenu;

use super::*;
use crate::secrets::{self, Edit, Stored};

/// The smallest window that the app restores. It is the minimum size of the window.
const MIN_WINDOW: (f32, f32) = (960., 600.);
/// An import file larger than this is not a list of connections.
const MAX_IMPORT_BYTES: u64 = 8 * 1024 * 1024;

/// The size to restore. A damaged value (NaN, a negative number) gives the minimum.
fn clamp_size(w: f32, h: f32) -> (f32, f32) {
    let fix = |v: f32, min: f32| if v.is_finite() { v.max(min) } else { min };
    (fix(w, MIN_WINDOW.0), fix(h, MIN_WINDOW.1))
}

fn count_text(n: usize, verb: &str) -> String {
    match n {
        1 => format!("{verb} 1 connection."),
        n => format!("{verb} {n} connections."),
    }
}

/// True if `a` and `b` give the same connection. The name and the subscriptions do not count:
/// the user changes the subscriptions while connected.
fn same_connection(a: &Profile, b: &Profile) -> bool {
    let strip = |p: &Profile| Profile {
        name: String::new(),
        subscriptions: Vec::new(),
        subscription_options: std::collections::BTreeMap::new(),
        session_filters: Vec::new(),
        ..p.clone()
    };
    strip(a) == strip(b)
}

pub(super) fn display_name(p: &Profile) -> String {
    if p.name.trim().is_empty() {
        p.host.clone()
    } else {
        p.name.clone()
    }
}

/// "host:port". An IPv6 host shows in brackets.
pub(super) fn host_port(p: &Profile) -> String {
    if p.host.contains(':') && !p.host.starts_with('[') {
        format!("[{}]:{}", p.host, p.port)
    } else {
        format!("{}:{}", p.host, p.port)
    }
}

/// The text of the connection menu button when no connection is active. `connections` counts
/// the saved ones and the "Command line" one.
pub(super) fn choose_text(connections: usize) -> &'static str {
    if connections == 0 {
        "Add a connection"
    } else {
        "Choose a connection"
    }
}

/// A Save of a new connection makes it active only when nothing is live or on its way: the
/// state is Disconnected or Failed, no keyring read is pending and no Connect is outstanding.
pub(super) fn save_selects(is_new: bool, state: &ConnState, connecting: bool) -> bool {
    is_new && !connecting && matches!(state, ConnState::Disconnected | ConnState::Failed(_))
}

impl Navigator {
    // -----------------------------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------------------------

    /// Starts the background load of the settings and the listener of writer errors.
    /// Shows the errors of the settings writer as notifications.
    pub(super) fn spawn_write_errors(
        &mut self,
        mut write_errors: tokio::sync::mpsc::Receiver<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            while let Some(text) = write_errors.recv().await {
                let alive = cx.update(|window, cx| {
                    this.update(cx, |v, cx| v.notify(Notification::error(text), window, cx))
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn notify(&self, note: Notification, window: &mut Window, cx: &mut Context<Self>) {
        window.push_notification(note, cx);
    }

    /// The settings are here (`main` loaded them before the window opened). Apply them.
    pub(super) fn apply_settings(
        &mut self,
        load: Load,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings = load.settings;
        self.read_only = load.read_only;
        self.settings_loaded = true;
        if let Some(notice) = load.notice {
            // The window has no notification host until its first frame.
            cx.defer_in(window, move |v, window, cx| {
                v.notify(Notification::warning(notice), window, cx)
            });
        }
        let ui = self.settings.ui.clone();
        if ui.theme_mode == ThemeMode::System {
            // `main` had no window to ask. The window knows its appearance better.
            self.apply_theme(window, cx);
        }
        self.view = match ui.view {
            ViewMode::Messages => View::Messages,
            ViewMode::Tree => View::Tree,
        };
        self.sync_filter_placeholder(window, cx);
        // Core clamps the saved value. The clamped one shows. A lower limit makes work.
        self.apply_memory_limit(ui.store_budget_mib);
        self.kick_work();
        if self.user_resized {
            // The user was faster than the file. The new size wins.
            self.settings.ui.window_size = Some(self.last_size);
            self.persist();
        } else if let Some((w, h)) = ui.window_size {
            let (w, h) = clamp_size(w, h);
            self.last_size = (w, h);
            window.resize(size(px(w), px(h)));
        }
        if self.profile.is_none() {
            let saved = &self.settings.profiles;
            let pick = self
                .settings
                .last_profile_id
                .and_then(|id| saved.iter().find(|p| p.id == id))
                .or_else(|| saved.first())
                .cloned();
            if pick.is_some() {
                self.set_active(pick);
                // Metrics only (see `autoselect`): connect without a click.
                if self.autoselect.is_some() {
                    self.connect(window, cx);
                }
            }
        }
        // One check at start, never on a timer. A metrics run makes no network traffic.
        if ui.check_updates && std::env::var_os("MQTT_NAVIGATOR_METRICS").is_none() {
            self.check_updates(false, window, cx);
        }
        cx.notify();
    }

    /// Gives the writer the newest settings. It saves after 500 ms of quiet. It never saves
    /// when the file could not be read (`read_only`).
    pub(super) fn persist(&self) {
        if self.settings_loaded
            && !self.read_only
            && let Some(writer) = &self.writer
        {
            writer.save(self.settings.clone());
        }
    }

    pub(super) fn on_resize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let s = window.viewport_size();
        let now = (f32::from(s.width), f32::from(s.height));
        if now == self.last_size {
            return;
        }
        self.last_size = now;
        // A size change before the first frame, or soon after it, comes from the window system.
        if self.resize_settled_at.is_none_or(|t| Instant::now() < t) {
            return;
        }
        self.user_resized = true;
        if self.settings.ui.window_size != Some(now) {
            self.settings.ui.window_size = Some(now);
            // A drag sends many events. The save waits for the last one (a copy of all settings
            // is made for each save). The timer starts from the event and ends with the save.
            self.resize_save = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(RESIZE_SAVE).await;
                this.update(cx, |v, _| {
                    v.resize_save = None;
                    v.persist();
                })
                .ok();
            }));
        }
    }

    /// Applies the saved preset and appearance (see `crate::themes`).
    pub(super) fn apply_theme(&self, window: &mut Window, cx: &mut Context<Self>) {
        crate::themes::apply(
            &self.settings.ui.theme_preset,
            self.settings.ui.theme_mode,
            Some(window),
            cx,
        );
    }

    pub(super) fn set_appearance(
        &mut self,
        mode: ThemeMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.ui.theme_mode = mode;
        self.apply_theme(window, cx);
        self.persist();
        cx.notify();
    }

    pub(super) fn set_preset(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.ui.theme_preset = crate::themes::preset_by_id(id).id.into();
        self.apply_theme(window, cx);
        self.persist();
        cx.notify();
    }

    // -----------------------------------------------------------------------------------------
    // The profile list and the title bar select
    // -----------------------------------------------------------------------------------------

    /// The session profile (command line) first, then the saved profiles.
    pub(super) fn all_profiles(&self) -> impl Iterator<Item = &Profile> {
        self.session.iter().chain(self.settings.profiles.iter())
    }

    pub(super) fn find_profile(&self, id: Uuid) -> Option<&Profile> {
        self.all_profiles().find(|p| p.id == id)
    }

    /// The rows of the connection menus: the title bar menu and the menu button of the empty
    /// state "Choose a connection". One function builds both.
    pub(super) fn connection_menu(
        &self,
        trigger: &FocusHandle,
        cx: &Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let rows: Vec<(Uuid, SharedString, SharedString)> = self
            .all_profiles()
            .map(|p| (p.id, display_name(p).into(), host_port(p).into()))
            .collect();
        let active = self.profile.as_ref().map(|p| p.id);
        let command_line = self.session.as_ref().map(|s| s.id) == active;
        let edit_name = self.profile.as_ref().map(display_name);
        let weak = cx.entity().downgrade();
        // The trigger is the action context: Escape and a chosen action return the focus to it.
        let focus = trigger.clone();
        move |menu, window, _| {
            let rem = window.rem_size();
            let mut menu = menu
                .action_context(focus.clone())
                .scrollable(true)
                .min_w(ui(300.).to_pixels(rem))
                .max_h(ui(320.).to_pixels(rem));
            for (id, name, addr) in rows.iter().cloned() {
                let w = weak.clone();
                menu = menu.item(
                    PopupMenuItem::element(move |_, cx| {
                        h_flex()
                            .w_full()
                            .gap_4()
                            .justify_between()
                            .child(name.clone())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(addr.clone()),
                            )
                    })
                    .checked(active == Some(id))
                    .on_click(move |_, window, cx| {
                        w.update(cx, |v, cx| v.request_switch(id, false, window, cx))
                            .ok();
                    }),
                );
            }
            if !rows.is_empty() {
                menu = menu.separator();
            }
            if let (Some(name), Some(id)) = (&edit_name, active) {
                let w = weak.clone();
                let back = focus.clone();
                menu = menu.item(if command_line {
                    PopupMenuItem::new("Command line: not saved").disabled(true)
                } else {
                    PopupMenuItem::new(format!("Edit \"{name}\"…")).on_click(
                        move |_, window, cx| {
                            // The sheet returns the focus to the focus at its opening. The menu
                            // is still open now, so focus the trigger first.
                            back.focus(window, cx);
                            w.update(cx, |v, cx| v.open_sheet(Some(id), false, window, cx))
                                .ok();
                        },
                    )
                });
            }
            let w2 = weak.clone();
            let back2 = focus.clone();
            menu.item(PopupMenuItem::new("New connection…").action(Box::new(NewConnection)))
                .item(
                    PopupMenuItem::new("Manage connections…").on_click(move |_, window, cx| {
                        back2.focus(window, cx);
                        w2.update(cx, |v, cx| v.open_sheet(None, false, window, cx))
                            .ok();
                    }),
                )
        }
    }

    /// Makes `profile` the active one and shows its subscriptions. It starts no connection.
    pub(super) fn set_active(&mut self, profile: Option<Profile>) {
        // `rev` stays monotonic: the cached sub list compares it, and a new `Subs` would restart it.
        let rev = self.subs.rev;
        self.subs = Subs::of_profile(profile.as_ref());
        self.subs.rev += rev + 1;
        self.profile = profile;
    }

    /// The net thread reports the filters that the broker may hold for the persistent session
    /// of the connection `generation`. Saves them in that profile, through the debounced writer.
    /// The unsaved "Command line" profile keeps nothing.
    pub(super) fn save_session_filters(&mut self, generation: u64, filters: &[String]) {
        let Some(id) = self
            .connect_owners
            .iter()
            .find(|(g, _)| *g == generation)
            .map(|(_, id)| *id)
        else {
            return;
        };
        if self.session.as_ref().is_some_and(|s| s.id == id) {
            return;
        }
        if let Some(saved) = self.settings.profiles.iter_mut().find(|p| p.id == id)
            && saved.session_filters != filters
        {
            saved.session_filters = filters.to_vec();
            self.persist();
        }
        if let Some(p) = self.profile.as_mut().filter(|p| p.id == id) {
            p.session_filters = filters.to_vec();
        }
    }

    /// Writes the subscription list that the user sees into the saved profile, through the
    /// debounced writer. The unsaved "Command line" profile is never written.
    pub(super) fn persist_subs(&mut self) {
        let Some(id) = self.profile.as_ref().map(|p| p.id) else {
            return;
        };
        if self.session.as_ref().is_some_and(|s| s.id == id) {
            return;
        }
        let (wanted, options) = (self.subs.wanted(), self.subs.wanted_options());
        if let Some(saved) = self.settings.profiles.iter_mut().find(|p| p.id == id)
            && (saved.subscriptions != wanted || saved.subscription_options != options)
        {
            saved.subscriptions = wanted;
            saved.subscription_options = options;
            self.persist();
        }
    }

    /// A connection is up or on its way (see `connection_live`).
    pub(super) fn is_live(&self) -> bool {
        connection_live(&self.status.state, &self.gate, self.status.generation)
    }

    /// Data belongs to one profile. A switch cancels the connection (always, even if no status
    /// has come yet) and removes the messages, the selection, the composer and the subscription
    /// results.
    fn clear_data(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.banner_closed = false;
        self.cancel_connection();
        // Dropping a full store (up to 128 MiB) takes long. A background thread does it.
        let old = self.store.take();
        cx.background_executor()
            .spawn(async move { drop(old) })
            .detach();
        self.sel_msg = None;
        self.sel_topic = None;
        self.detail_of = DetailOf::Nothing;
        self.anchor = None;
        // The new store does not count rows: Follow and its counter start again together.
        self.follow = Follow::default();
        self.list_scroll
            .scroll_to_item_strict(0, ScrollStrategy::Top);
        self.reconnected_at = None;
        self.last_result = None;
        self.sub_error = None;
        self.rate = Rate::default();
        self.status = Status::default();
        // The composer and History belong to the profile too.
        self.pane.update(cx, |p, cx| p.reset(cx));
        self.sync_details(cx);
        self.comp.reset(window, cx);
        self.sub_input
            .update(cx, |i, cx| i.set_value("", window, cx));
    }

    /// The user chose another profile. While a connection is live, the app asks first.
    pub(super) fn request_switch(
        &mut self,
        id: Uuid,
        connect: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let same = self.profile.as_ref().is_some_and(|p| p.id == id);
        if same && !connect {
            return;
        }
        let Some(target) = self.find_profile(id).map(display_name) else {
            return;
        };
        if !self.is_live() {
            if same {
                self.connect(window, cx);
            } else {
                self.switch_to(id, connect, window, cx);
            }
            return;
        }
        let from = self.profile.as_ref().map(display_name).unwrap_or_default();
        let weak = cx.entity().downgrade();
        // The dialog gives the focus back to the element that had it when it opened, after its
        // close animation. So the focus goes to the menu button first, for Switch and Cancel.
        self.menu_focus.focus(window, cx);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let weak = weak.clone();
            alert
                .title(if same {
                    format!("Reconnect to {target}?")
                } else {
                    format!("Disconnect from {from} and connect to {target}?")
                })
                .description(format!("Messages from {from} are removed."))
                .confirm()
                .ok_text(if same {
                    "Reconnect"
                } else {
                    "Switch connection"
                })
                .cancel_text("Cancel")
                .on_ok(move |_, window, cx| {
                    weak.update(cx, |v, cx| v.switch_to(id, true, window, cx))
                        .ok();
                    true
                })
        });
    }

    fn switch_to(&mut self, id: Uuid, connect: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.find_profile(id).cloned() else {
            return;
        };
        let saved = self.session.as_ref().is_none_or(|s| s.id != id);
        let same = self.profile.as_ref().is_some_and(|p| p.id == id);
        if same {
            // A reconnect of the same profile keeps its data (ui-design §3): only the old
            // connection goes. Its late data has an old generation, so the gate drops it.
            self.cancel_connection();
        } else {
            // `clear_data` disconnects: the old broker must not stay connected while the
            // keyring answers, and a Connect that has no status yet must not reach the new
            // profile.
            self.clear_data(window, cx);
        }
        self.set_active(Some(profile));
        if saved {
            self.settings.last_profile_id = Some(id);
            self.persist();
        }
        if connect {
            self.connect(window, cx);
        }
        self.connect_focus.focus(window, cx);
        cx.notify();
    }

    /// The focus goes to the primary button. It goes to the menu button when the primary
    /// button is hidden or off.
    fn focus_primary(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.profile.is_some() && self.status.state != ConnState::Disconnecting {
            self.connect_focus.focus(window, cx);
        } else {
            self.menu_focus.focus(window, cx);
        }
    }

    /// The active profile was deleted.
    fn deselect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_data(window, cx);
        self.set_active(None);
    }

    // -----------------------------------------------------------------------------------------
    // Connect and secrets
    // -----------------------------------------------------------------------------------------

    /// Connects the active profile with its saved settings. The keyring read runs in the
    /// background. The connect command goes out when it ends, and only if no switch, edit or
    /// cancel has happened since (the request token).
    pub(super) fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.profile.as_ref().map(|p| p.id) else {
            return;
        };
        self.banner_closed = false;
        // Edits made while the old connection was live apply now.
        if let Some(saved) = self.find_profile(id).cloned() {
            self.profile = Some(saved);
        }
        // Every profile can have a password (MQTT 5 allows one without a user name), so the
        // keyring is always read once. A missing entry is fine.
        if self.secret_cache.get(&id).is_some_and(|c| c.loaded) {
            self.connect_now(window, cx);
            return;
        }
        if self.secret_req.is_some_and(|(_, req)| req == id) {
            return;
        }
        let token = self.next_secret_token;
        self.next_secret_token += 1;
        self.secret_req = Some((token, id));
        let reply = self.secrets.load(id);
        cx.spawn_in(window, async move |this, cx| {
            let loaded = reply.await.unwrap_or_else(|_| secrets::Loaded {
                secrets: Cached::default(),
                error: Some(secrets::busy()),
            });
            cx.update(|window, cx| {
                this.update(cx, |v, cx| v.secrets_loaded(token, id, loaded, window, cx))
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn connect_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(why) = self.send_connect() {
            self.notify(
                Notification::error(format!("Cannot connect. {why}")),
                window,
                cx,
            );
        }
    }

    fn secrets_loaded(
        &mut self,
        token: u64,
        id: Uuid,
        loaded: secrets::Loaded,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.secret_req == Some((token, id));
        if current {
            self.secret_req = None;
        }
        match loaded.error {
            // A reply of an old request fills the cache (a deleted profile gets nothing). It
            // never connects.
            None => {
                if self.find_profile(id).is_some() {
                    self.secret_cache
                        .entry(id)
                        .or_default()
                        .merge_loaded(loaded.secrets);
                }
            }
            Some(why) => {
                tracing::warn!(error = %why, "Cannot read the keyring");
                // A profile with a user name or a key expects a secret. For others the miss is
                // normal (no keyring on this system).
                let expects = self
                    .find_profile(id)
                    .is_some_and(|p| p.username.is_some() || p.client_key.is_some());
                if current && expects {
                    self.notify(
                        Notification::warning(format!(
                            "The saved password cannot be read ({why}). The app connects without it."
                        )),
                        window,
                        cx,
                    );
                }
            }
        }
        if current && self.profile.as_ref().is_some_and(|p| p.id == id) {
            self.connect_now(window, cx);
        }
        cx.notify();
    }

    // -----------------------------------------------------------------------------------------
    // The connection sheet
    // -----------------------------------------------------------------------------------------

    /// Opens the sheet. `select`: the profile to edit (default: the active one). `new`: start a
    /// new profile.
    pub(super) fn open_sheet(
        &mut self,
        select: Option<Uuid>,
        new: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.settings_loaded {
            self.notify(
                Notification::info("The settings are loading. Try again in a moment."),
                window,
                cx,
            );
            return;
        }
        let profiles = self.settings.profiles.clone();
        let select = if new {
            None
        } else {
            select.or(self.profile.as_ref().map(|p| p.id))
        };
        let sheet = cx.new(|cx| ConnectionSheet::new(profiles, select, window, cx));
        cx.subscribe_in(&sheet, window, |v, _, ev: &SheetEvent, window, cx| {
            v.on_sheet_event(ev.clone(), window, cx);
        })
        .detach();
        self.sheet = Some(sheet.downgrade());
        ConnectionSheet::open(&sheet, true, window, cx);
    }

    fn on_sheet_event(&mut self, ev: SheetEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            SheetEvent::Save {
                profile,
                password,
                passphrase,
                connect,
            } => self.save_profile(*profile, password, passphrase, connect, window, cx),
            SheetEvent::Delete(id) => self.delete_profile(id, window, cx),
            SheetEvent::Export => self.export(window, cx),
            SheetEvent::Import => self.import(window, cx),
        }
    }

    /// The saved profiles changed: show them in the select and in the open sheet.
    fn profiles_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.persist();
        let profiles = self.settings.profiles.clone();
        if let Some(sheet) = self.sheet.as_ref().and_then(WeakEntity::upgrade) {
            sheet.update(cx, |s, cx| s.set_profiles(profiles, window, cx));
        }
        cx.notify();
    }

    fn save_profile(
        &mut self,
        profile: Profile,
        password: Edit,
        passphrase: Edit,
        connect: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = profile.id;
        let mut profile = profile;
        let select = save_selects(
            !self.settings.profiles.iter().any(|p| p.id == id),
            &self.status.state,
            self.connecting(),
        );
        match self.settings.profiles.iter_mut().find(|p| p.id == id) {
            Some(slot) => {
                // The sheet does not edit the subscriptions. Keep the ones that the user
                // changed while connected.
                profile.subscriptions = slot.subscriptions.clone();
                profile.subscription_options = slot.subscription_options.clone();
                profile.carry_session_from(slot);
                *slot = profile.clone();
            }
            None => self.settings.profiles.push(profile.clone()),
        }
        // An edit ends a pending keyring read: its reply must not connect.
        if self.secret_req.is_some_and(|(_, req)| req == id) {
            self.secret_req = None;
        }
        if self.profile.as_ref().is_some_and(|p| p.id == id) {
            if self.is_live() {
                // The live connection keeps its settings and its endpoint. The saved profile
                // changes. The user applies it with Reconnect.
                let secrets_edited = password != Edit::Keep || passphrase != Edit::Keep;
                if let Some(runtime) = self.profile.as_mut() {
                    self.reconnect_pending |= secrets_edited || !same_connection(runtime, &profile);
                    runtime.name = profile.name.clone();
                }
            } else {
                self.subs.write_to(&mut profile);
                self.profile = Some(profile);
                self.reconnect_pending = false;
            }
        }
        // The session keeps the secret even if the keyring refuses it.
        self.secret_cache
            .entry(id)
            .or_default()
            .apply(&password, &passphrase);
        if password != Edit::Keep || passphrase != Edit::Keep {
            let reply = self.secrets.save(id, password.clone(), passphrase.clone());
            cx.spawn_in(window, async move |this, cx| {
                let stored = reply.await.unwrap_or_else(|_| Stored {
                    password: Err(secrets::busy()),
                    passphrase: Err(secrets::busy()),
                });
                cx.update(|window, cx| {
                    this.update(cx, |v, cx| {
                        v.secrets_saved(&password, &passphrase, stored, window, cx)
                    })
                })
                .ok();
            })
            .detach();
        }
        self.profiles_changed(window, cx);
        window.close_sheet(cx);
        // The sheet gives the focus back after it closes, so move it one step later. A switch
        // question (Save and connect while connected) keeps the focus.
        cx.defer_in(window, |v, window, cx| {
            if !window.has_active_dialog(cx) {
                v.focus_primary(window, cx);
            }
        });
        if connect {
            self.request_switch(id, true, window, cx);
        } else if select {
            // The same path as a choice in the menu: the old data does not stay.
            self.switch_to(id, false, window, cx);
        }
    }

    /// A secret counts as saved only after a successful write. A failure is reported.
    fn secrets_saved(
        &mut self,
        password: &Edit,
        passphrase: &Edit,
        stored: Stored,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (what, edit, result) in [
            ("Password", password, stored.password),
            ("Key passphrase", passphrase, stored.passphrase),
        ] {
            let Err(why) = result else {
                continue;
            };
            tracing::warn!(error = %why, "Keyring write failed");
            let text = match edit {
                Edit::Set(_) => format!(
                    "{what} not saved: no system keyring. It is kept for this session only."
                ),
                Edit::Clear => format!(
                    "The saved {} could not be removed ({why}).",
                    what.to_lowercase()
                ),
                Edit::Keep => continue,
            };
            self.notify(Notification::warning(text), window, cx);
        }
    }

    fn delete_profile(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.profiles.retain(|p| p.id != id);
        if self.settings.last_profile_id == Some(id) {
            self.settings.last_profile_id = None;
        }
        self.secret_cache.remove(&id);
        if self.secret_req.is_some_and(|(_, req)| req == id) {
            self.secret_req = None;
        }
        let reply = self.secrets.delete(id);
        cx.spawn_in(window, async move |this, cx| {
            let stored = reply.await.unwrap_or_else(|_| Stored {
                password: Err(secrets::busy()),
                passphrase: Err(secrets::busy()),
            });
            let failed = stored.password.err().or(stored.passphrase.err());
            if let Some(why) = failed {
                cx.update(|window, cx| {
                    this.update(cx, |v, cx| {
                        tracing::warn!(error = %why, "Keyring delete failed");
                        v.notify(
                            Notification::warning(format!(
                                "The saved secrets could not be removed from the keyring ({why})."
                            )),
                            window,
                            cx,
                        );
                    })
                })
                .ok();
            }
        })
        .detach();
        if self.profile.as_ref().is_some_and(|p| p.id == id) {
            self.deselect(window, cx);
        }
        self.profiles_changed(window, cx);
    }

    // -----------------------------------------------------------------------------------------
    // Import and export
    // -----------------------------------------------------------------------------------------

    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profiles = self.settings.profiles.clone();
        if profiles.is_empty() {
            self.notify(
                Notification::info("There are no saved connections to export."),
                window,
                cx,
            );
            return;
        }
        let dir = dirs::home_dir().unwrap_or_default();
        let picked = cx.prompt_for_new_path(&dir, Some("mqtt-navigator-connections.json"));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = picked.await else {
                return;
            };
            let n = profiles.len();
            let json = export_profiles(&profiles);
            let result = cx
                .background_executor()
                .spawn(async move { std::fs::write(path, json) })
                .await;
            cx.update(|window, cx| {
                this.update(cx, |v, cx| {
                    let note = match result {
                        Ok(()) => Notification::success(count_text(n, "Exported")),
                        Err(e) => Notification::error(format!("The export failed ({e}).")),
                    };
                    v.notify(note, window, cx);
                })
            })
            .ok();
        })
        .detach();
    }

    fn import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self.settings.profiles.clone();
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let result = cx
                .background_executor()
                .spawn(async move {
                    let too_big =
                        std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_IMPORT_BYTES);
                    if too_big {
                        return Err("The file is too large for a list of connections.".to_string());
                    }
                    let text = std::fs::read_to_string(&path)
                        .map_err(|e| format!("The file cannot be read ({e})."))?;
                    import_profiles(&text, &existing)
                })
                .await;
            cx.update(|window, cx| {
                this.update(cx, |v, cx| match result {
                    Ok(mut new) => {
                        // The list can have changed while the file was read.
                        dedupe_names(&mut new, &v.settings.profiles);
                        let n = new.len();
                        v.settings.profiles.extend(new);
                        v.profiles_changed(window, cx);
                        v.notify(Notification::success(count_text(n, "Imported")), window, cx);
                    }
                    Err(why) => v.notify(
                        Notification::error(format!("The import failed. {why}")),
                        window,
                        cx,
                    ),
                })
            })
            .ok();
        })
        .detach();
    }

    // -----------------------------------------------------------------------------------------
    // Empty state
    // -----------------------------------------------------------------------------------------

    /// "No profiles" state (docs/ui-design.md, section 7). "New connection…" is the one primary
    /// action. The localhost shortcut is a plain button next to it.
    pub(super) fn render_no_profile(&self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.all_profiles().count();
        if count > 0 {
            return empty_state(
                "Choose a connection",
                "Your connections are in this menu.",
                [
                    Button::new("choose-empty")
                        .small()
                        .label(choose_text(count))
                        .dropdown_caret(true)
                        .track_focus(&self.choose_focus)
                        .dropdown_menu(self.connection_menu(&self.choose_focus, cx))
                        .into_any_element(),
                    Button::new("new-empty")
                        .small()
                        .label("New connection…")
                        .on_click(
                            cx.listener(|v, _, window, cx| v.open_sheet(None, true, window, cx)),
                        )
                        .into_any_element(),
                ],
            );
        }
        empty_state(
            "Add a broker to start",
            "Topics and messages show here.",
            [
                Button::new("new-connection")
                    .small()
                    .primary()
                    .label("New connection…")
                    .on_click(cx.listener(|v, _, window, cx| v.open_sheet(None, true, window, cx)))
                    .into_any_element(),
                Button::new("connect-local")
                    .small()
                    .label("Connect to localhost:1883 (not encrypted)")
                    .on_click(cx.listener(|v, _, window, cx| v.quick_connect(window, cx)))
                    .into_any_element(),
            ],
        )
    }

    /// Saves a plain TCP profile for localhost and connects it.
    fn quick_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut p = Profile::new();
        p.name = "localhost".into();
        p.host = "localhost".into();
        p.set_transport(Transport::Tcp);
        let id = p.id;
        self.settings.profiles.push(p);
        self.profiles_changed(window, cx);
        self.switch_to(id, true, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{choose_text, clamp_size, count_text, host_port, same_connection, save_selects};
    use crate::ui::ConnState;
    use navigator_core::net::Transport;
    use navigator_core::profile::Profile;

    #[test]
    fn name_and_subscriptions_are_not_a_connection_change() {
        let a = Profile::new();
        let mut b = a.clone();
        b.name = "other".into();
        b.subscriptions.clear();
        assert!(same_connection(&a, &b));
        b.host = "elsewhere".into();
        assert!(!same_connection(&a, &b));
        let mut c = a.clone();
        c.set_transport(Transport::Tcp);
        assert!(!same_connection(&a, &c));
    }

    #[test]
    fn restored_size_is_at_least_the_minimum() {
        assert_eq!(clamp_size(1280., 800.), (1280., 800.));
        assert_eq!(clamp_size(100., 100.), (960., 600.));
        assert_eq!(clamp_size(f32::NAN, f32::INFINITY), (960., 600.));
        assert_eq!(clamp_size(-5., 700.), (960., 700.));
    }

    #[test]
    fn counts_read_well() {
        assert_eq!(count_text(1, "Imported"), "Imported 1 connection.");
        assert_eq!(count_text(3, "Imported"), "Imported 3 connections.");
    }

    #[test]
    fn menu_button_text_follows_the_connection_count() {
        // Nothing saved and no "Command line" connection.
        assert_eq!(choose_text(0), "Add a connection");
        // Saved connections, or only the "Command line" one.
        assert_eq!(choose_text(1), "Choose a connection");
        assert_eq!(choose_text(3), "Choose a connection");
    }

    #[test]
    fn ipv6_host_shows_in_brackets() {
        let mut p = Profile::new();
        p.host = "fd00::1".into();
        p.port = 1883;
        assert_eq!(host_port(&p), "[fd00::1]:1883");
        p.host = "localhost".into();
        assert_eq!(host_port(&p), "localhost:1883");
    }

    #[test]
    fn save_selects_a_new_connection_only_when_idle() {
        let idle = [ConnState::Disconnected, ConnState::Failed("x".into())];
        for s in &idle {
            assert!(save_selects(true, s, false));
            // A pending keyring read or an outstanding Connect.
            assert!(!save_selects(true, s, true));
            // An edit of a saved connection.
            assert!(!save_selects(false, s, false));
        }
        let busy = [
            ConnState::Connected,
            ConnState::Connecting,
            ConnState::Reconnecting {
                attempt: 1,
                reason: "x".into(),
            },
            ConnState::Disconnecting,
        ];
        for s in &busy {
            assert!(!save_selects(true, s, false));
        }
    }
}
