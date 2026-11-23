//! The connection sheet: it edits a DRAFT copy of one profile. Nothing changes until the user
//! presses Save. The sheet reads no file: it only keeps the paths. The core reads them at connect.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::str::FromStr;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants, Toggle, ToggleGroup};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::form::{Field, field, v_form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::sheet::Sheet;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tab::TabBar;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Placement, Selectable, Sizable, WindowExt, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use navigator_core::Qos;
use navigator_core::net::{ConnectConfig, Protocol};
use navigator_core::profile::{FieldError, Profile, Transport, Will};
use uuid::Uuid;

use crate::secrets::Edit;

#[derive(Clone)]
struct ProtocolItem(Protocol);

impl SearchableListItem for ProtocolItem {
    type Value = Protocol;

    fn title(&self) -> SharedString {
        protocol_text(self.0).into()
    }

    fn value(&self) -> &Protocol {
        &self.0
    }
}

pub(crate) fn protocol_text(p: Protocol) -> &'static str {
    match p {
        Protocol::V311 => "MQTT 3.1.1",
        Protocol::V5 => "MQTT 5",
    }
}

const PROTOCOLS: [Protocol; 2] = [Protocol::V311, Protocol::V5];

/// The label of the profile field `clean_start`. In MQTT 3.1.1 the same CONNECT flag is Clean
/// Session. Clean Session off asks the broker for a persistent session.
fn clean_label(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::V311 => "Clean session",
        Protocol::V5 => "Clean start",
    }
}

/// Keep alive 0 turns off our ping, unless an MQTT 5 broker sets its own (Server Keep Alive).
/// An empty field means 0.
fn keep_alive_is_off(text: &str) -> bool {
    number::<u16>(text, true) == Some(0)
}

const KEEP_ALIVE_OFF_NOTE: &str = "Keep alive 0 sends no ping. An MQTT 5 broker can set a different keep alive. Without a ping, the app finds a lost connection only when an operation times out or the system reports an error.";

/// Session expiry and will delay exist in MQTT 5 only.
fn has_expiry_and_will_delay(protocol: Protocol) -> bool {
    protocol == Protocol::V5
}

/// A field that exists in MQTT 5 only: session expiry or will delay. A number that does not
/// parse is `None` (an error) in MQTT 5. In MQTT 3.1.1 the field is hidden: the number is not
/// sent, so a wrong text is no error, and the profile keeps `saved`.
fn seconds(text: &str, protocol: Protocol, saved: u32) -> Option<u32> {
    match number(text, true) {
        Some(n) => Some(n),
        None if has_expiry_and_will_delay(protocol) => None,
        None => Some(saved),
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
#[derive(Clone, Copy, PartialEq)]
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
const FIELD_ORDER: [&str; 12] = [
    "name",
    "host",
    "port",
    "username",
    "password",
    "client_cert",
    "client_id",
    "ws_path",
    "session_expiry",
    "keepalive",
    "will.topic",
    "will.delay_secs",
];
const ADVANCED: [&str; 6] = [
    "client_id",
    "ws_path",
    "session_expiry",
    "keepalive",
    "will.topic",
    "will.delay_secs",
];

/// A saved secret that the sheet can remove.
#[derive(Clone, Copy)]
enum Secret {
    Password,
    Passphrase,
}

/// A collapsed section of the sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Advanced,
    ClientCert,
}

/// The collapsed section that holds a field. `None`: the field is always visible.
fn section_of(field: &str) -> Option<Section> {
    if ADVANCED.contains(&field) {
        Some(Section::Advanced)
    } else if field == "client_cert" {
        Some(Section::ClientCert)
    } else {
        None
    }
}

/// The icon of a disclosure button: down while the section is open, right while it is closed.
fn disclosure_icon(open: bool) -> IconName {
    if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    }
}

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
    protocol: Protocol,
    protocol_select: Entity<SelectState<Vec<ProtocolItem>>>,
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
    /// The collapsed "Client certificate" section (certificate, key, passphrase).
    client_cert_open: bool,
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
    /// The "Choose…" button of the certificate. A certificate error focuses it.
    cert_focus: FocusHandle,
    /// The scroll position of the form. It has its own scroll area: see `render`.
    scroll: ScrollHandle,
    /// The top edge of each field of `FIELD_ORDER` in the window, from the last frame it was drawn.
    tops: HashMap<&'static str, Rc<Cell<Option<Pixels>>>>,
    /// A wrong field to scroll to, and whether one frame has drawn it already.
    reveal: Option<(&'static str, bool)>,
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
        // Every single-line field. A change of a field removes its error message (`None`: the
        // field has no error). Enter in any of them runs "Save and connect".
        let inputs = [
            (Some("name"), &name),
            (Some("host"), &host),
            (Some("port"), &port),
            (Some("username"), &username),
            (Some("password"), &password),
            (Some("client_id"), &client_id),
            (None, &key_passphrase),
            (Some("ws_path"), &ws_path),
            (Some("session_expiry"), &session_expiry),
            (Some("keepalive"), &keepalive),
            (Some("will.topic"), &will_topic),
            (None, &will_payload),
            (Some("will.delay_secs"), &will_delay),
        ];
        let protocol_select = cx.new(|cx| {
            let items = PROTOCOLS.map(ProtocolItem).to_vec();
            SelectState::new(items, None, window, cx)
        });
        let protocol_sub = cx.subscribe(
            &protocol_select,
            |s, _, ev: &SelectEvent<Vec<ProtocolItem>>, cx| {
                let SelectEvent::Confirm(Some(p)) = ev else {
                    return;
                };
                s.protocol = *p;
                cx.notify();
            },
        );
        let mut subs: Vec<Subscription> = inputs
            .into_iter()
            .map(|(field, input)| {
                cx.subscribe_in(
                    input,
                    window,
                    move |s, _, ev: &InputEvent, window, cx| match ev {
                        InputEvent::Change => {
                            s.errors.retain(|e| Some(e.field) != field);
                            cx.notify();
                        }
                        InputEvent::PressEnter {
                            secondary: false,
                            shift: false,
                        } => s.submit(true, window, cx),
                        _ => {}
                    },
                )
            })
            .collect();
        subs.push(protocol_sub);
        // New text in a secret field cancels "Remove the saved ..." (see `clear_secret_box`).
        subs.push(cx.subscribe(&password, |s, input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) && !input.read(cx).value().is_empty() {
                s.clear_password = false;
            }
        }));
        subs.push(
            cx.subscribe(&key_passphrase, |s, input, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) && !input.read(cx).value().is_empty() {
                    s.clear_passphrase = false;
                }
            }),
        );
        let draft = Profile::new();
        let draft_protocol = draft.protocol;
        let scroll = ScrollHandle::new();
        let tops = FIELD_ORDER.iter().map(|f| (*f, Rc::default())).collect();
        let mut this = Self {
            profiles,
            origin: draft.clone(),
            baseline: draft,
            is_new: true,
            transport: Transport::Tls,
            verify_tls: true,
            protocol: draft_protocol,
            protocol_select,
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
            client_cert_open: false,
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
            cert_focus: cx.focus_handle(),
            scroll,
            tops,
            reveal: None,
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
            let title = entity.read(cx).title();
            sheet
                .title(title)
                .size(crate::ui::ui(540.))
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

    /// For the window tests: what the user sees. The focused field, the field errors, whether
    /// the two collapsed sections are open, and the title.
    #[cfg(test)]
    pub(crate) fn probe(
        &self,
        window: &Window,
        cx: &App,
    ) -> (Option<&'static str>, Vec<&'static str>, [bool; 2], String) {
        let on = |i: &Entity<InputState>| i.read(cx).focus_handle(cx).is_focused(window);
        let focused = [
            ("name", &self.name),
            ("host", &self.host),
            ("port", &self.port),
            ("will.delay_secs", &self.will_delay),
        ]
        .into_iter()
        .find(|(_, i)| on(i))
        .map(|(f, _)| f)
        .or(self.cert_focus.is_focused(window).then_some("client_cert"));
        let errors = self.errors.iter().map(|e| e.field).collect();
        (
            focused,
            errors,
            [self.advanced, self.client_cert_open],
            self.title(),
        )
    }

    /// For the window tests: the two "Remove the saved ..." boxes.
    #[cfg(test)]
    pub(crate) fn removal_boxes(&self) -> [bool; 2] {
        [self.clear_password, self.clear_passphrase]
    }

    /// For the window tests: the password field takes the focus.
    #[cfg(test)]
    pub(crate) fn focus_password(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.password.update(cx, |i, cx| i.focus(window, cx));
    }

    /// For the window tests: MQTT 5 with a wrong Will delay, or a certificate without a key.
    #[cfg(test)]
    pub(crate) fn make_invalid(
        &mut self,
        will_delay: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if will_delay {
            self.protocol = Protocol::V5;
            Self::set_text(&self.will_topic, "a/b", window, cx);
            Self::set_text(&self.will_delay, "abc", window, cx);
        } else {
            self.client_cert = Some("/c.pem".into());
        }
        cx.notify();
    }

    /// For the window tests: the transport, as the user picks it in the tab bar.
    #[cfg(test)]
    pub(crate) fn use_transport(
        &mut self,
        t: Transport,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_transport(t, window, cx);
    }

    /// "New connection", or "Edit lab". It follows the draft.
    fn title(&self) -> String {
        if self.is_new {
            "New connection".into()
        } else {
            let p = &self.origin;
            format!(
                "Edit {}",
                if p.name.trim().is_empty() {
                    &p.host
                } else {
                    &p.name
                }
            )
        }
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
        self.protocol = p.protocol;
        self.protocol_select
            .update(cx, |s, cx| s.set_selected_value(&p.protocol, window, cx));
        self.ca_file = p.ca_file.clone();
        self.client_cert = p.client_cert.clone();
        self.client_key = p.client_key.clone();
        self.clear_password = false;
        self.clear_passphrase = false;
        self.advanced = false;
        self.client_cert_open = false;
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
        p.protocol = self.protocol;
        p.clean_start = self.clean_start;
        // A hidden field keeps the value of the draft. A wrong value is no error while hidden.
        match seconds(
            &text(&self.session_expiry),
            self.protocol,
            self.origin.session_expiry_secs,
        ) {
            Some(n) => p.session_expiry_secs = n,
            None => {
                errors.push(err("session_expiry", "Enter a number of seconds."));
                p.session_expiry_secs = 0;
            }
        }
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
            delay_secs: seconds(
                &text(&self.will_delay),
                self.protocol,
                self.origin.will.as_ref().map_or(0, |w| w.delay_secs),
            )
            .unwrap_or_else(|| {
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
            // A wrong field in a collapsed section opens that section first.
            for section in errors.iter().filter_map(|e| section_of(e.field)) {
                match section {
                    Section::Advanced => self.advanced = true,
                    Section::ClientCert => self.client_cert_open = true,
                }
            }
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

    fn focus_field(&mut self, field: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(field) = field else {
            return;
        };
        let input = match field {
            "name" => &self.name,
            "host" => &self.host,
            "port" => &self.port,
            "username" => &self.username,
            "password" => &self.password,
            "ws_path" => &self.ws_path,
            "client_id" => &self.client_id,
            "session_expiry" => &self.session_expiry,
            "keepalive" => &self.keepalive,
            "will.topic" => &self.will_topic,
            "will.delay_secs" => &self.will_delay,
            // The certificate fields have no input: the focus goes to "Choose…".
            "client_cert" => &self.name,
            _ => return,
        };
        let input = (field != "client_cert").then(|| input.clone());
        let handle = self.cert_focus.clone();
        // An input in a section that has just opened exists only after the next frame. The same
        // is true for its place: `render` scrolls to it after the frame that draws it.
        if let Some((key, top)) = self.tops.get_key_value(field) {
            top.take();
            self.reveal = Some((*key, false));
        }
        cx.defer_in(window, move |_, window, cx| match &input {
            Some(input) => input.update(cx, |i, cx| i.focus(window, cx)),
            None => handle.focus(window, cx),
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
                .ok_text("Discard changes")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("Cancel")
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
                .debug_selector(move || format!("field:{key}"))
                .child(self.marker(key))
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
                        .when(kind == FileKind::Cert, |b| b.track_focus(&self.cert_focus))
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

    /// "Remove the saved password". New text in the field cancels the removal, and the box is off
    /// while the field has text, so the two never disagree: the text always wins, and the box
    /// shows it.
    fn clear_secret_box(&self, which: Secret, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (id, label, on, input) = match which {
            Secret::Password => (
                "clear-password",
                "Remove the saved password",
                self.clear_password,
                &self.password,
            ),
            Secret::Passphrase => (
                "clear-passphrase",
                "Remove the saved passphrase",
                self.clear_passphrase,
                &self.key_passphrase,
            ),
        };
        let typed = !input.read(cx).value().is_empty();
        div().debug_selector(move || id.into()).child(
            Checkbox::new(id)
                .checked(on)
                .label(label)
                .disabled(typed)
                .on_change(cx.listener(move |s, on: &bool, _, cx| {
                    match which {
                        Secret::Password => s.clear_password = *on,
                        Secret::Passphrase => s.clear_passphrase = *on,
                    }
                    cx.notify();
                })),
        )
    }

    /// An empty box at the top of a field. It records where the field is (see `reveal`).
    fn marker(&self, field: &'static str) -> impl IntoElement + use<> {
        let top = self.tops.get(field).cloned();
        canvas(
            move |bounds, _, _| {
                if let Some(top) = &top {
                    top.set(Some(bounds.origin.y));
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_0()
    }

    /// Scrolls the form to the wrong field that `focus_field` named. The field must have been
    /// drawn once (a closed section opens first), so it waits one frame, and then it uses the
    /// place that the field reported. Without a place it does nothing.
    fn scroll_to_reveal(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some((field, drawn)) = self.reveal.take() else {
            return;
        };
        if !drawn {
            self.reveal = Some((field, true));
            cx.notify();
            return;
        }
        let Some(top) = self.tops.get(field).and_then(|t| t.take()) else {
            return;
        };
        let (view, at) = (self.scroll.bounds(), self.scroll.offset());
        // The field is a little below the top: its label is above it.
        let y = view.origin.y - top + at.y + window.rem_size() * 2.;
        let least = -self.scroll.max_offset().y;
        self.scroll.set_offset(point(at.x, y.clamp(least, px(0.))));
    }

    fn section(title: &'static str, cx: &App) -> Div {
        div()
            .pt_2()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(title)
    }

    /// The saved connections. The one in the draft is the selected row. "New connection" is a
    /// row, too: it is selected while the draft is new.
    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // A ghost Button is a tab stop. Enter and Space press it. Up and Down move the focus.
        let row = |id: SharedString| Button::new(id).ghost().small().w_full().justify_start();
        v_flex()
            .gap_0p5()
            .on_key_down(|ev, window, cx| {
                if ev.keystroke.modifiers.modified() {
                    return;
                }
                match ev.keystroke.key.as_str() {
                    "down" => window.focus_next(cx),
                    "up" => window.focus_prev(cx),
                    _ => {}
                }
            })
            .children(self.profiles.iter().map(|p| {
                let id = p.id;
                let active = !self.is_new && self.origin.id == id;
                row(format!("profile:{id}").into())
                    .selected(active)
                    .accessibility_label(p.name.clone())
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_left()
                            .child(p.name.clone()),
                    )
                    .on_click(cx.listener(move |s, _, window, cx| {
                        if s.is_new || s.origin.id != id {
                            s.guard(Act::Select(id), window, cx);
                        }
                    }))
            }))
            .child(
                row("new-profile".into())
                    .selected(self.is_new)
                    .accessibility_label("New connection")
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(Icon::new(IconName::Plus).small())
                            .child("New connection"),
                    )
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.scroll_to_reveal(window, cx);
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
            .child(
                field().label("Protocol").child(
                    Select::new(&self.protocol_select)
                        .small()
                        .accessibility_label("Protocol"),
                ),
            )
            .child(self.input_field("Username", "username", &self.username, cx))
            .child(
                field().label("Password").child(
                    v_flex()
                        .child(self.marker("password"))
                        .gap_1()
                        .child(Input::new(&self.password).small().mask_toggle())
                        .children(self.error_text("password", cx))
                        .when(!self.is_new, |d| {
                            d.child(self.clear_secret_box(Secret::Password, cx))
                        }),
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
            .child(self.file_field("CA file", FileKind::Ca, &self.ca_file, cx));
        let client_cert = v_form()
            .child(self.file_field("Certificate", FileKind::Cert, &self.client_cert, cx))
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
                            d.child(self.clear_secret_box(Secret::Passphrase, cx))
                        }),
                ),
            );

        let advanced = v_form()
            .child(
                field().label("Client ID").child(
                    v_flex()
                        .child(self.marker("client_id"))
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().flex_1().child(Input::new(&self.client_id).small()))
                                .child(
                                    Button::new("new-client-id")
                                        .small()
                                        .label("New ID")
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
            )
            .child(self.input_field("WebSocket path", "ws_path", &self.ws_path, cx))
            .child(
                field().child(
                    Switch::new("clean-start")
                        .checked(self.clean_start)
                        .label(clean_label(self.protocol))
                        .on_click(cx.listener(|s, v: &bool, _, cx| {
                            s.clean_start = *v;
                            cx.notify();
                        })),
                ),
            )
            .when(has_expiry_and_will_delay(self.protocol), |f| {
                f.child(self.input_field(
                    "Session expiry (seconds)",
                    "session_expiry",
                    &self.session_expiry,
                    cx,
                ))
            })
            .child(self.input_field("Keep alive (seconds)", "keepalive", &self.keepalive, cx))
            .when(
                keep_alive_is_off(self.keepalive.read(cx).value().as_ref()),
                |f| f.child(field().child(warn(KEEP_ALIVE_OFF_NOTE))),
            )
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
                    // The same control as the QoS of the composer.
                    ToggleGroup::new("will-qos")
                        .segmented()
                        .single()
                        .small()
                        .child(
                            Toggle::new("will-qos-0")
                                .label("QoS 0")
                                .checked(self.will_qos == Qos::AtMostOnce),
                        )
                        .child(
                            Toggle::new("will-qos-1")
                                .label("QoS 1")
                                .checked(self.will_qos == Qos::AtLeastOnce),
                        )
                        .on_click(cx.listener(|s, checks: &Vec<bool>, _, cx| {
                            if let Some(ix) = checks.iter().position(|c| *c) {
                                s.will_qos = if ix == 0 {
                                    Qos::AtMostOnce
                                } else {
                                    Qos::AtLeastOnce
                                };
                                cx.notify();
                            }
                        })),
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
            .when(has_expiry_and_will_delay(self.protocol), |f| {
                f.child(self.input_field(
                    "Will delay (seconds)",
                    "will.delay_secs",
                    &self.will_delay,
                    cx,
                ))
            });

        let form = v_flex()
            .gap_3()
            .px_4()
            .pb_4()
            .child(self.render_list(cx))
            .child(basic)
            .when(!encrypted, |d| {
                d.child(warn(
                    "Not encrypted. Passwords and messages are sent in clear text.",
                ))
            })
            .when(encrypted, |d| {
                let cert_open = self.client_cert_open;
                let cert_set = self.client_cert.is_some() || self.client_key.is_some();
                d.child(Self::section("TLS", cx))
                    .child(tls)
                    .when(!self.verify_tls, |d| {
                        d.child(warn("Certificate verification is off."))
                    })
                    .child(
                        h_flex()
                            .child(self.marker("client_cert"))
                            .gap_2()
                            .items_center()
                            .child(
                                Button::new("client-cert")
                                    .small()
                                    .ghost()
                                    .icon(disclosure_icon(cert_open))
                                    .label("Client certificate")
                                    .on_click(cx.listener(|s, _, _, cx| {
                                        s.client_cert_open = !s.client_cert_open;
                                        cx.notify();
                                    })),
                            )
                            .when(cert_set && !cert_open, |d| {
                                d.child(div().text_xs().text_color(muted).child("In use"))
                            }),
                    )
                    // The message is above the fields, so it stays in view when the section opens.
                    .children(self.error_text("client_cert", cx))
                    .when(cert_open, |d| d.child(client_cert))
            })
            .child(
                h_flex().child(
                    Button::new("advanced")
                        .small()
                        .ghost()
                        .icon(disclosure_icon(self.advanced))
                        .label("Advanced")
                        .on_click(cx.listener(|s, _, _, cx| {
                            s.advanced = !s.advanced;
                            cx.notify();
                        })),
                ),
            )
            .when(self.advanced, |d| d.child(advanced))
            .children(self.error_text("subscriptions", cx))
            .when(!self.is_new, |d| {
                d.child(
                    div().pt_2().child(
                        Button::new("delete")
                            .small()
                            .danger()
                            .label("Delete connection…")
                            .on_click(cx.listener(|s, _, window, cx| s.confirm_delete(window, cx))),
                    ),
                )
            });
        // The sheet body grows with its content, so a scroll area inside it would grow too. An
        // absolute box takes the size of the body instead, and the form scrolls in it. The
        // sheet adds no padding of its own here: the form has its own, inside the scroll area.
        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .child(
                div()
                    .id("sheet-form")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(form),
            )
            .vertical_scrollbar(&self.scroll)
    }
}

#[cfg(test)]
mod tests {
    use navigator_core::net::Protocol;

    use super::{
        Section, clean_label, has_expiry_and_will_delay, keep_alive_is_off, number, seconds,
        section_of,
    };

    #[test]
    fn a_hidden_session_field_keeps_its_edit_and_raises_no_error() {
        // 0 changed to 3600 under MQTT 5, then MQTT 3.1.1, then save: the profile has 3600.
        assert_eq!(seconds("3600", Protocol::V311, 0), Some(3600));
        assert_eq!(seconds("3600", Protocol::V5, 0), Some(3600));
        // A wrong text is an error for MQTT 5 only. MQTT 3.1.1 keeps the saved value.
        assert_eq!(seconds("abc", Protocol::V5, 7), None);
        assert_eq!(seconds("abc", Protocol::V311, 7), Some(7));
        assert_eq!(seconds("", Protocol::V311, 7), Some(0));
    }

    #[test]
    fn mqtt_311_shows_clean_session_and_hides_the_mqtt_5_session_fields() {
        assert_eq!(clean_label(Protocol::V311), "Clean session");
        assert_eq!(clean_label(Protocol::V5), "Clean start");
        assert!(!has_expiry_and_will_delay(Protocol::V311));
        assert!(has_expiry_and_will_delay(Protocol::V5));
    }

    #[test]
    fn the_keep_alive_note_shows_for_0_and_for_an_empty_field_only() {
        assert!(keep_alive_is_off("0"));
        assert!(keep_alive_is_off(" "));
        assert!(!keep_alive_is_off("60"));
        assert!(!keep_alive_is_off("abc"));
    }

    #[test]
    fn a_wrong_field_opens_the_collapsed_section_that_holds_it() {
        // The client ID moved to Advanced. The client certificate has its own section.
        for f in [
            "client_id",
            "ws_path",
            "session_expiry",
            "keepalive",
            "will.topic",
        ] {
            assert_eq!(section_of(f), Some(Section::Advanced), "{f}");
        }
        assert_eq!(section_of("client_cert"), Some(Section::ClientCert));
        // The basic fields are always visible: nothing to open.
        for f in ["name", "host", "port", "username", "password"] {
            assert_eq!(section_of(f), None, "{f}");
        }
        // Every field in the focus order is either basic or in a section that can open.
        for f in super::FIELD_ORDER {
            let basic = ["name", "host", "port", "username", "password"].contains(&f);
            assert_eq!(section_of(f).is_none(), basic, "{f}");
        }
    }

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
