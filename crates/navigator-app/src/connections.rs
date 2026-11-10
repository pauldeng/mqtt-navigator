//! The connection sheet: it edits a DRAFT copy of one profile. Nothing changes until the user
//! presses Save. The sheet reads no file: it only keeps the paths. The core reads them at connect.

use std::cell::Cell;
use std::path::PathBuf;
use std::str::FromStr;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::form::{Field, field, v_form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::sheet::Sheet;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tab::TabBar;
use gpui_kit::component::{ActiveTheme, Placement, Sizable, WindowExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use navigator_core::Qos;
use navigator_core::net::ConnectConfig;
use navigator_core::profile::{FieldError, Profile, Transport, Will};
use uuid::Uuid;

use crate::secrets::Edit;

/// What the title bar select offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Profile(Uuid),
    /// The last item: "Manage connections…".
    Manage,
}

#[derive(Clone)]
pub struct ProfileItem {
    pub choice: Choice,
    pub title: SharedString,
}

impl SearchableListItem for ProfileItem {
    type Value = Choice;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Choice {
        &self.choice
    }
}

/// What the sheet tells the app.
#[derive(Clone, Debug)]
pub enum SheetEvent {
    Save {
        profile: Box<Profile>,
        password: Edit,
        passphrase: Edit,
        connect: bool,
    },
    Delete(Uuid),
    Export,
    Import,
}

/// Which certificate file a picker sets.
#[derive(Clone, Copy)]
enum FileKind {
    Ca,
    Cert,
    Key,
}

/// An action that can lose the draft. The app asks first when there are unsaved changes.
#[derive(Clone, Copy)]
enum Act {
    Close,
    Select(Uuid),
    New,
}

/// The fields in the order that focus goes to the first wrong one.
const FIELD_ORDER: [&str; 11] = [
    "name",
    "host",
    "port",
    "username",
    "password",
    "client_cert",
    "ws_path",
    "client_id",
    "session_expiry",
    "keepalive",
    "will.topic",
];
const ADVANCED: [&str; 5] = [
    "ws_path",
    "session_expiry",
    "keepalive",
    "will.topic",
    "will.delay_secs",
];

fn err(field: &'static str, message: &str) -> FieldError {
    FieldError {
        field,
        message: message.into(),
    }
}

/// Parses a number field. An empty field is 0 when `empty_is_zero`.
fn number<T: FromStr + Default>(text: &str, empty_is_zero: bool) -> Option<T> {
    let text = text.trim();
    if text.is_empty() && empty_is_zero {
        return Some(T::default());
    }
    text.parse().ok()
}

pub struct ConnectionSheet {
    /// The saved profiles.
    profiles: Vec<Profile>,
    /// The profile that the form was loaded from. It gives the ID and the subscriptions.
    origin: Profile,
    /// What `read` gave right after the load. A different value means unsaved changes.
    baseline: Profile,
    is_new: bool,
    transport: Transport,
    verify_tls: bool,
    clean_start: bool,
    will_retain: bool,
    will_qos: Qos,
    ca_file: Option<PathBuf>,
    client_cert: Option<PathBuf>,
    client_key: Option<PathBuf>,
    /// A Will payload that is not UTF-8. The sheet keeps the bytes as they are.
    will_binary: Option<Vec<u8>>,
    clear_password: bool,
    clear_passphrase: bool,
    advanced: bool,
    errors: Vec<FieldError>,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    client_id: Entity<InputState>,
    key_passphrase: Entity<InputState>,
    ws_path: Entity<InputState>,
    session_expiry: Entity<InputState>,
    keepalive: Entity<InputState>,
    will_topic: Entity<InputState>,
    will_payload: Entity<InputState>,
    will_delay: Entity<InputState>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<SheetEvent> for ConnectionSheet {}

impl ConnectionSheet {
    /// `select` is the profile to edit. `None`: a new profile.
    pub fn new(
        profiles: Vec<Profile>,
        select: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = |window: &mut Window, cx: &mut Context<Self>, placeholder: &'static str| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let name = input(window, cx, "prod-broker");
        let host = input(window, cx, "broker.example.com");
        let port = input(window, cx, "8883");
        let username = input(window, cx, "Optional");
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Optional")
                .masked(true)
        });
        let client_id = input(window, cx, "Empty: the broker assigns one");
        let key_passphrase = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Only for an encrypted key")
                .masked(true)
        });
        let ws_path = input(window, cx, "/mqtt");
        let session_expiry = input(window, cx, "0");
        let keepalive = input(window, cx, "60");
        let will_topic = input(window, cx, "Empty: no will message");
        let will_payload = input(window, cx, "Payload text");
        let will_delay = input(window, cx, "0");
        // A change of a field removes its error message.
        let watched = [
            ("name", &name),
            ("host", &host),
            ("port", &port),
            ("client_id", &client_id),
            ("ws_path", &ws_path),
            ("session_expiry", &session_expiry),
            ("keepalive", &keepalive),
            ("will.topic", &will_topic),
            ("will.delay_secs", &will_delay),
        ];
        let subs = watched
            .into_iter()
            .map(|(field, input)| {
                cx.subscribe(input, move |s, _, ev: &InputEvent, cx| {
                    if matches!(ev, InputEvent::Change) {
                        s.errors.retain(|e| e.field != field);
                        cx.notify();
                    }
                })
            })
            .collect();
        let draft = Profile::new();
        let mut this = Self {
            profiles,
            origin: draft.clone(),
            baseline: draft,
            is_new: true,
            transport: Transport::Tls,
            verify_tls: true,
            clean_start: true,
            will_retain: false,
            will_qos: Qos::AtMostOnce,
            ca_file: None,
            client_cert: None,
            client_key: None,
            will_binary: None,
            clear_password: false,
            clear_passphrase: false,
            advanced: false,
            errors: Vec::new(),
            name,
            host,
            port,
            username,
            password,
            client_id,
            key_passphrase,
            ws_path,
            session_expiry,
            keepalive,
            will_topic,
            will_payload,
            will_delay,
            _subs: subs,
        };
        match select.and_then(|id| this.profiles.iter().find(|p| p.id == id).cloned()) {
            Some(p) => this.load(p, false, window, cx),
            None => this.load(Profile::new(), true, window, cx),
        }
        this
    }

    /// Opens the sheet for `entity`. Call it again to show the same draft after a close.
    /// `focus_name`: put the focus in the name field (not when a dialog opens on top).
    pub fn open(entity: &Entity<Self>, focus_name: bool, window: &mut Window, cx: &mut App) {
        let first = Cell::new(focus_name);
        let entity = entity.clone();
        window.open_sheet_at(Placement::Right, cx, move |sheet: Sheet, window, cx| {
            if first.replace(false) {
                let name = entity.read(cx).name.clone();
                name.update(cx, |i, cx| i.focus(window, cx));
            }
            let on_close = entity.clone();
            sheet
                .title("Connections")
                .size(px(540.))
                // A click outside must not drop the draft. Escape and the close button can
                // still close the sheet: `dismissed` brings it back if the draft has changes.
                .overlay_closable(false)
                .on_close(move |_, window, cx| {
                    let entity = on_close.clone();
                    window.defer(cx, move |window, cx| {
                        entity.update(cx, |s, cx| s.dismissed(&entity, window, cx));
                    });
                })
                .footer(Self::footer(&entity))
                .child(entity.clone())
        });
    }

    fn footer(entity: &Entity<Self>) -> impl IntoElement + use<> {
        let button = |id: &'static str,
                      label: &'static str,
                      f: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            let entity = entity.clone();
            Button::new(id)
                .small()
                .label(label)
                .on_click(move |_, window, cx| {
                    entity.update(cx, |s, cx| f(s, window, cx));
                })
        };
        h_flex()
            .w_full()
            .gap_2()
            .child(
                h_flex()
                    .flex_1()
                    .gap_1()
                    .child(
                        button("export", "Export…", |_, _, cx| {
                            cx.emit(SheetEvent::Export)
                        })
                        .ghost(),
                    )
                    .child(
                        button("import", "Import…", |_, _, cx| {
                            cx.emit(SheetEvent::Import)
                        })
                        .ghost(),
                    ),
            )
            .child(button("cancel", "Cancel", |s, window, cx| {
                s.guard(Act::Close, window, cx)
            }))
            .child(button("save", "Save", |s, window, cx| {
                s.submit(false, window, cx)
            }))
            .child(
                button("save-connect", "Save and connect", |s, window, cx| {
                    s.submit(true, window, cx)
                })
                .primary(),
            )
    }

    /// The saved profiles changed (a delete or an import). The draft stays.
    pub fn set_profiles(
        &mut self,
        profiles: Vec<Profile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.profiles = profiles;
        if !self.is_new && !self.profiles.iter().any(|p| p.id == self.origin.id) {
            match self.profiles.first().cloned() {
                Some(p) => self.load(p, false, window, cx),
                None => self.load(Profile::new(), true, window, cx),
            }
        }
        cx.notify();
    }

    fn set_text(
        input: &Entity<InputState>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        input.update(cx, |i, cx| i.set_value(text, window, cx));
    }

    /// Puts a profile in the form. The secret fields start empty: the sheet never shows a secret.
    fn load(&mut self, p: Profile, is_new: bool, window: &mut Window, cx: &mut Context<Self>) {
        Self::set_text(&self.name, &p.name, window, cx);
        Self::set_text(&self.host, &p.host, window, cx);
        Self::set_text(&self.port, &p.port.to_string(), window, cx);
        Self::set_text(
            &self.username,
            p.username.as_deref().unwrap_or(""),
            window,
            cx,
        );
        Self::set_text(&self.password, "", window, cx);
        let hint = if is_new {
            "Optional"
        } else {
            "Empty: keep the saved password"
        };
        self.password
            .update(cx, |i, cx| i.set_placeholder(hint, window, cx));
        Self::set_text(&self.client_id, &p.client_id, window, cx);
        Self::set_text(&self.key_passphrase, "", window, cx);
        Self::set_text(&self.ws_path, &p.ws_path, window, cx);
        Self::set_text(
            &self.session_expiry,
            &p.session_expiry_secs.to_string(),
            window,
            cx,
        );
        Self::set_text(&self.keepalive, &p.keepalive_secs.to_string(), window, cx);
        let will = p.will.clone().unwrap_or_default();
        Self::set_text(&self.will_topic, &will.topic, window, cx);
        let text = String::from_utf8(will.payload.clone());
        Self::set_text(
            &self.will_payload,
            text.as_deref().unwrap_or(""),
            window,
            cx,
        );
        self.will_binary = text.is_err().then_some(will.payload);
        Self::set_text(&self.will_delay, &will.delay_secs.to_string(), window, cx);
        self.will_qos = will.qos;
        self.will_retain = will.retain;
        self.transport = p.transport;
        self.verify_tls = p.verify_tls;
        self.clean_start = p.clean_start;
        self.ca_file = p.ca_file.clone();
        self.client_cert = p.client_cert.clone();
        self.client_key = p.client_key.clone();
        self.clear_password = false;
        self.clear_passphrase = false;
        self.advanced = false;
        self.errors.clear();
        self.is_new = is_new;
        self.origin = p;
        self.baseline = self.read(cx).0;
        cx.notify();
    }

    /// Builds a profile from the form. The errors are number fields that do not parse.
    fn read(&self, cx: &App) -> (Profile, Vec<FieldError>) {
        let text = |i: &Entity<InputState>| i.read(cx).value().to_string();
        let mut errors = Vec::new();
        let mut p = self.origin.clone();
        p.name = text(&self.name).trim().to_string();
        p.host = text(&self.host).trim().to_string();
        p.port = number(&text(&self.port), false).unwrap_or_else(|| {
            errors.push(err("port", "The port must be from 1 to 65535."));
            0
        });
        p.transport = self.transport;
        p.ws_path = text(&self.ws_path);
        p.client_id = text(&self.client_id);
        let user = text(&self.username);
        p.username = (!user.is_empty()).then_some(user);
        p.ca_file = self.ca_file.clone();
        p.client_cert = self.client_cert.clone();
        p.client_key = self.client_key.clone();
        p.verify_tls = self.verify_tls;
        p.clean_start = self.clean_start;
        p.session_expiry_secs = number(&text(&self.session_expiry), true).unwrap_or_else(|| {
            errors.push(err("session_expiry", "Enter a number of seconds."));
            0
        });
        p.keepalive_secs = number(&text(&self.keepalive), true).unwrap_or_else(|| {
            errors.push(err("keepalive", "Enter a number from 0 to 65535."));
            0
        });
        let topic = text(&self.will_topic);
        p.will = (!topic.is_empty()).then(|| Will {
            topic,
            payload: self
                .will_binary
                .clone()
                .unwrap_or_else(|| text(&self.will_payload).into_bytes()),
            qos: self.will_qos,
            retain: self.will_retain,
            delay_secs: number(&text(&self.will_delay), true).unwrap_or_else(|| {
                errors.push(err("will.delay_secs", "Enter a number of seconds."));
                0
            }),
        });
        (p, errors)
    }

    fn secret_edit(input: &Entity<InputState>, clear: bool, cx: &App) -> Edit {
        let text = input.read(cx).value().to_string();
        if !text.is_empty() {
            Edit::Set(text)
        } else if clear {
            Edit::Clear
        } else {
            Edit::Keep
        }
    }

    fn dirty(&self, cx: &App) -> bool {
        self.read(cx).0 != self.baseline
            || Self::secret_edit(&self.password, self.clear_password, cx) != Edit::Keep
            || Self::secret_edit(&self.key_passphrase, self.clear_passphrase, cx) != Edit::Keep
    }

    /// Checks the draft. It runs when a button is pressed, never while the user types.
    fn submit(&mut self, connect: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (profile, mut errors) = self.read(cx);
        if let Err(more) = profile.validate(&self.profiles) {
            errors.extend(more);
        }
        let password = Self::secret_edit(&self.password, self.clear_password, cx);
        // A longer password makes the MQTT library panic at the connect.
        if let Edit::Set(text) = &password {
            let check = ConnectConfig {
                password: Some(text.clone()),
                ..ConnectConfig::from(profile.clone())
            };
            if let Err(why) = check.validate()
                && why.contains("password")
            {
                errors.push(err("password", &why));
            }
        }
        let mut seen = Vec::new();
        errors.retain(|e| {
            let new = !seen.contains(&e.field);
            seen.push(e.field);
            new
        });
        if !errors.is_empty() {
            self.advanced |= errors.iter().any(|e| ADVANCED.contains(&e.field));
            let first = FIELD_ORDER
                .iter()
                .find(|f| errors.iter().any(|e| e.field == **f))
                .copied();
            self.errors = errors;
            self.focus_field(first, window, cx);
            cx.notify();
            return;
        }
        self.errors.clear();
        cx.emit(SheetEvent::Save {
            profile: Box::new(profile),
            password,
            passphrase: Self::secret_edit(&self.key_passphrase, self.clear_passphrase, cx),
            connect,
        });
    }

    fn focus_field(&self, field: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let input = match field {
            Some("name") => &self.name,
            Some("host") => &self.host,
            Some("port") => &self.port,
            Some("username") => &self.username,
            Some("password") => &self.password,
            Some("ws_path") => &self.ws_path,
            Some("client_id") => &self.client_id,
            Some("session_expiry") => &self.session_expiry,
            Some("keepalive") => &self.keepalive,
            Some("will.topic") => &self.will_topic,
            // The certificate fields have no input. Their message shows next to the buttons.
            _ => return,
        };
        // An input in a section that has just opened exists only after the next frame.
        let input = input.clone();
        cx.defer_in(window, move |_, window, cx| {
            input.update(cx, |i, cx| i.focus(window, cx));
        });
    }

    /// Runs `act`, or asks first when the draft has unsaved changes.
    fn guard(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dirty(cx) {
            self.run(act, window, cx);
            return;
        }
        let entity = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let entity = entity.clone();
            alert
                .title("Discard your changes?")
                .description("The changes to this connection are not saved.")
                .confirm()
                .ok_text("Discard")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("Keep editing")
                .on_ok(move |_, window, cx| {
                    entity.update(cx, |s, cx| s.run(act, window, cx));
                    true
                })
        });
    }

    fn run(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        match act {
            Act::Close => window.close_sheet(cx),
            Act::New => self.load(Profile::new(), true, window, cx),
            Act::Select(id) => {
                if let Some(p) = self.profiles.iter().find(|p| p.id == id).cloned() {
                    self.load(p, false, window, cx);
                }
            }
        }
    }

    /// Escape or the close button closed the sheet. A draft with changes comes back with a
    /// question.
    fn dismissed(&mut self, entity: &Entity<Self>, window: &mut Window, cx: &mut Context<Self>) {
        if self.dirty(cx) {
            Self::open(entity, false, window, cx);
            self.guard(Act::Close, window, cx);
        }
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (id, name) = (self.origin.id, self.baseline.name.clone());
        let entity = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let entity = entity.clone();
            alert
                .title(format!("Delete connection {name}?"))
                .description("The saved password and key passphrase are removed too.")
                .confirm()
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    entity.update(cx, |_, cx| cx.emit(SheetEvent::Delete(id)));
                    true
                })
        });
    }

    fn set_transport(&mut self, transport: Transport, window: &mut Window, cx: &mut Context<Self>) {
        // `Profile::set_transport` has the port rule.
        let mut p = self.read(cx).0;
        p.set_transport(transport);
        Self::set_text(&self.port, &p.port.to_string(), window, cx);
        self.transport = transport;
        self.errors.retain(|e| e.field != "port");
        cx.notify();
    }

    fn pick(&mut self, kind: FileKind, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |s, cx| {
                match kind {
                    FileKind::Ca => s.ca_file = Some(path),
                    FileKind::Cert => s.client_cert = Some(path),
                    FileKind::Key => s.client_key = Some(path),
                }
                s.errors.retain(|e| e.field != "client_cert");
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn clear_file(&mut self, kind: FileKind, cx: &mut Context<Self>) {
        match kind {
            FileKind::Ca => self.ca_file = None,
            FileKind::Cert => self.client_cert = None,
            FileKind::Key => self.client_key = None,
        }
        self.errors.retain(|e| e.field != "client_cert");
        cx.notify();
    }

    fn error_text(&self, key: &str, cx: &App) -> Option<Div> {
        self.errors.iter().find(|e| e.field == key).map(|e| {
            div()
                .text_xs()
                .text_color(cx.theme().danger)
                .child(e.message.clone())
        })
    }

    fn input_field(
        &self,
        label: &'static str,
        key: &'static str,
        input: &Entity<InputState>,
        cx: &App,
    ) -> Field {
        field().label(label).child(
            v_flex()
                .gap_1()
                .child(Input::new(input).small())
                .children(self.error_text(key, cx)),
        )
    }

    fn file_field(
        &self,
        label: &'static str,
        kind: FileKind,
        path: &Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Field {
        let theme = cx.theme();
        let (id_pick, id_clear) = match kind {
            FileKind::Ca => ("ca-pick", "ca-clear"),
            FileKind::Cert => ("cert-pick", "cert-clear"),
            FileKind::Key => ("key-pick", "key-clear"),
        };
        // The file name only: the full path can be long and private.
        let shown = path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned());
        field().label(label).child(
            h_flex()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .when(shown.is_none(), |d| d.text_color(theme.muted_foreground))
                        .child(shown.clone().unwrap_or_else(|| "None".into())),
                )
                .child(
                    Button::new(id_pick)
                        .small()
                        .label("Choose…")
                        .on_click(cx.listener(move |s, _, window, cx| s.pick(kind, window, cx))),
                )
                .when(shown.is_some(), |d| {
                    d.child(
                        Button::new(id_clear)
                            .small()
                            .ghost()
                            .label("Clear")
                            .on_click(cx.listener(move |s, _, _, cx| s.clear_file(kind, cx))),
                    )
                }),
        )
    }

    fn section(title: &'static str, cx: &App) -> Div {
        div()
            .pt_2()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(title)
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .flex_wrap()
            .gap_1()
            .children(self.profiles.iter().map(|p| {
                let id = p.id;
                let active = !self.is_new && self.origin.id == id;
                let b = Button::new(SharedString::from(format!("profile:{id}")))
                    .small()
                    .label(p.name.clone())
                    .on_click(cx.listener(move |s, _, window, cx| {
                        if s.is_new || s.origin.id != id {
                            s.guard(Act::Select(id), window, cx);
                        }
                    }));
                if active { b.primary() } else { b.ghost() }
            }))
            .child(
                Button::new("new-profile")
                    .small()
                    .label("New connection")
                    .when(self.is_new, |b| b.primary())
                    .when(!self.is_new, |b| b.ghost())
                    .on_click(cx.listener(|s, _, window, cx| {
                        if !s.is_new {
                            s.guard(Act::New, window, cx);
                        }
                    })),
            )
            .when(self.profiles.is_empty(), |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("No saved connections."),
                )
            })
    }
}

impl Render for ConnectionSheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (warning, muted) = {
            let t = cx.theme();
            (t.warning, t.muted_foreground)
        };
        let transport_ix = match self.transport {
            Transport::Tls => 0,
            Transport::Wss => 1,
            Transport::Tcp => 2,
            Transport::Ws => 3,
        };
        let encrypted = self.transport.is_encrypted();
        let basic = v_form()
            .child(
                self.input_field("Name", "name", &self.name, cx)
                    .required(true),
            )
            .child(
                self.input_field("Host", "host", &self.host, cx)
                    .required(true),
            )
            .child(
                self.input_field("Port", "port", &self.port, cx)
                    .required(true),
            )
            .child(
                field().label("Transport").child(
                    TabBar::new("transport")
                        .segmented()
                        .small()
                        .selected_index(transport_ix)
                        .on_click(cx.listener(|s, ix: &usize, window, cx| {
                            let t = match *ix {
                                0 => Transport::Tls,
                                1 => Transport::Wss,
                                2 => Transport::Tcp,
                                _ => Transport::Ws,
                            };
                            s.set_transport(t, window, cx);
                        }))
                        .child("TLS")
                        .child("WSS")
                        .child("TCP")
                        .child("WS"),
                ),
            )
            .child(self.input_field("Username", "username", &self.username, cx))
            .child(
                field().label("Password").child(
                    v_flex()
                        .gap_1()
                        .child(Input::new(&self.password).small().mask_toggle())
                        .children(self.error_text("password", cx))
                        .when(!self.is_new, |d| {
                            d.child(
                                Button::new("clear-password")
                                    .xsmall()
                                    .ghost()
                                    .label(if self.clear_password {
                                        "The saved password is removed on save. Undo."
                                    } else {
                                        "Remove the saved password"
                                    })
                                    .on_click(cx.listener(|s, _, _, cx| {
                                        s.clear_password = !s.clear_password;
                                        cx.notify();
                                    })),
                            )
                        }),
                ),
            )
            .child(
                field().label("Client ID").child(
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().flex_1().child(Input::new(&self.client_id).small()))
                                .child(
                                    Button::new("new-client-id")
                                        .small()
                                        .label("New ID")
                                        .tooltip("Make a new random client ID")
                                        .on_click(cx.listener(|s, _, window, cx| {
                                            let id = Profile::new().client_id;
                                            Self::set_text(&s.client_id, &id, window, cx);
                                            s.errors.retain(|e| e.field != "client_id");
                                            cx.notify();
                                        })),
                                ),
                        )
                        .children(self.error_text("client_id", cx)),
                ),
            );

        let warn = |text: &'static str| div().text_sm().text_color(warning).child(text);
        let tls = v_form()
            .child(
                field().child(
                    Switch::new("verify-tls")
                        .checked(self.verify_tls)
                        .label("Verify the server certificate")
                        .on_click(cx.listener(|s, v: &bool, _, cx| {
                            s.verify_tls = *v;
                            cx.notify();
                        })),
                ),
            )
            .child(self.file_field("CA file", FileKind::Ca, &self.ca_file, cx))
            .child(self.file_field("Client certificate", FileKind::Cert, &self.client_cert, cx))
            .child(
                self.file_field("Client key", FileKind::Key, &self.client_key, cx)
                    .description("Set the certificate and the key together."),
            )
            .child(
                field().label("Key passphrase").child(
                    v_flex()
                        .gap_1()
                        .child(Input::new(&self.key_passphrase).small().mask_toggle())
                        .when(!self.is_new, |d| {
                            d.child(
                                Button::new("clear-passphrase")
                                    .xsmall()
                                    .ghost()
                                    .label(if self.clear_passphrase {
                                        "The saved passphrase is removed on save. Undo."
                                    } else {
                                        "Remove the saved passphrase"
                                    })
                                    .on_click(cx.listener(|s, _, _, cx| {
                                        s.clear_passphrase = !s.clear_passphrase;
                                        cx.notify();
                                    })),
                            )
                        }),
                ),
            );

        let advanced = v_form()
            .child(self.input_field("WebSocket path", "ws_path", &self.ws_path, cx))
            .child(
                field().child(
                    Switch::new("clean-start")
                        .checked(self.clean_start)
                        .label("Clean start")
                        .on_click(cx.listener(|s, v: &bool, _, cx| {
                            s.clean_start = *v;
                            cx.notify();
                        })),
                ),
            )
            .child(self.input_field(
                "Session expiry (seconds)",
                "session_expiry",
                &self.session_expiry,
                cx,
            ))
            .child(self.input_field("Keep alive (seconds)", "keepalive", &self.keepalive, cx))
            .child(self.input_field("Will topic", "will.topic", &self.will_topic, cx))
            .child(
                field().label("Will payload").child(
                    v_flex()
                        .gap_1()
                        .child(
                            Input::new(&self.will_payload)
                                .small()
                                .disabled(self.will_binary.is_some()),
                        )
                        .when(self.will_binary.is_some(), |d| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("The payload is binary. It is kept as it is."),
                            )
                        }),
                ),
            )
            .child(
                field().label("Will QoS").child(
                    TabBar::new("will-qos")
                        .segmented()
                        .small()
                        .selected_index(usize::from(self.will_qos == Qos::AtLeastOnce))
                        .on_click(cx.listener(|s, ix: &usize, _, cx| {
                            s.will_qos = if *ix == 0 {
                                Qos::AtMostOnce
                            } else {
                                Qos::AtLeastOnce
                            };
                            cx.notify();
                        }))
                        .child("QoS 0")
                        .child("QoS 1"),
                ),
            )
            .child(
                field().child(
                    Switch::new("will-retain")
                        .checked(self.will_retain)
                        .label("Retain the will message")
                        .on_click(cx.listener(|s, v: &bool, _, cx| {
                            s.will_retain = *v;
                            cx.notify();
                        })),
                ),
            )
            .child(self.input_field(
                "Will delay (seconds)",
                "will.delay_secs",
                &self.will_delay,
                cx,
            ));

        v_flex()
            .gap_3()
            .pb_4()
            .child(self.render_list(cx))
            .child(basic)
            .when(!encrypted, |d| {
                d.child(warn(
                    "Not encrypted. Passwords and messages are sent in clear text.",
                ))
            })
            .when(encrypted, |d| {
                d.child(Self::section("TLS", cx))
                    .child(tls)
                    .when(!self.verify_tls, |d| {
                        d.child(warn("Certificate verification is off."))
                    })
                    .children(self.error_text("client_cert", cx))
            })
            .child(
                Button::new("advanced")
                    .small()
                    .ghost()
                    .label(if self.advanced {
                        "Advanced (hide)"
                    } else {
                        "Advanced (show)"
                    })
                    .on_click(cx.listener(|s, _, _, cx| {
                        s.advanced = !s.advanced;
                        cx.notify();
                    })),
            )
            .when(self.advanced, |d| d.child(advanced))
            .children(self.error_text("subscriptions", cx))
            .when(!self.is_new, |d| {
                d.child(
                    div().pt_2().child(
                        Button::new("delete")
                            .small()
                            .danger()
                            .label("Delete connection")
                            .on_click(cx.listener(|s, _, window, cx| s.confirm_delete(window, cx))),
                    ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::number;

    #[test]
    fn number_fields_parse() {
        assert_eq!(number::<u16>("1883", false), Some(1883));
        assert_eq!(number::<u16>(" 60 ", true), Some(60));
        assert_eq!(number::<u16>("", true), Some(0));
        assert_eq!(number::<u16>("", false), None);
        assert_eq!(number::<u16>("70000", true), None);
        assert_eq!(number::<u32>("-1", true), None);
        assert_eq!(number::<u32>("abc", true), None);
    }
}
