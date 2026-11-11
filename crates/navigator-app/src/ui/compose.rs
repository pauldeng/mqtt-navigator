//! The publish composer at the bottom of the centre pane: fields, payload mode, properties,
//! History, validation and Send. The model (drafts, Text/Hex conversion) is in `crate::composer`.

use std::sync::atomic::AtomicBool;

use gpui_kit::component::button::{DropdownButton, Toggle, ToggleGroup};
use gpui_kit::component::collapsible::Collapsible;
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::switch::Switch;
use navigator_core::decode::{self, Decoder};
use navigator_core::publish::validate_outgoing;

use super::profiles::display_name;
use super::*;
use crate::composer::{ComposerModel, Draft, LineKeep, PayloadMode, line_breaks_note};
use crate::inspector::size_text;

/// Height of the payload editor (design pixels, see [`ui`]).
const PAYLOAD_HEIGHT: f32 = 84.;

/// Format refuses a payload above this size, and a result above [`FORMAT_MAX_OUT`]: setting a
/// large text into the editor runs on the UI thread (see docs/ui-design.md, 3.3).
const FORMAT_MAX_IN: usize = 1024 * 1024;
const FORMAT_MAX_OUT: usize = 2 * 1024 * 1024;

/// A single-line input and the exact string that it stands for (see [`LineKeep`]).
struct Field {
    input: Entity<InputState>,
    keep: LineKeep,
}

impl Field {
    fn new(input: Entity<InputState>) -> Self {
        Self {
            input,
            keep: LineKeep::default(),
        }
    }

    /// The exact value: the kept string with line breaks, or the text of the input.
    fn get(&self, cx: &App) -> String {
        self.keep.value(&self.input.read(cx).value())
    }

    fn set(&mut self, text: &str, window: &mut Window, cx: &mut App) {
        let shown = self.keep.set(text);
        self.input
            .update(cx, |i, cx| i.set_value(shown, window, cx));
    }

    /// The note under the field, if the exact string has line breaks.
    fn note(&self, what: &str, cx: &App) -> Option<String> {
        self.keep
            .active(&self.input.read(cx).value())
            .then(|| line_breaks_note(what))
    }
}

/// One row of user properties: two inputs and a remove button. A row with two empty inputs is
/// unused, unless `keep_empty` is on: then the empty pair is sent.
struct UserRow {
    id: u64,
    key: Field,
    value: Field,
    keep_empty: bool,
    _subs: Vec<Subscription>,
}

pub(super) struct Composer {
    topic: Field,
    payload: Entity<EditorState>,
    content_type: Field,
    /// Send the content type also when it is empty.
    content_type_empty: bool,
    response_topic: Field,
    correlation: Entity<InputState>,
    /// Send the correlation data also when it is empty.
    correlation_empty: bool,
    expiry: Entity<InputState>,
    rows: Vec<UserRow>,
    next_row: u64,
    pub(super) mode: PayloadMode,
    pub(super) qos: Qos,
    retain: bool,
    utf8: bool,
    props_open: bool,
    pub(super) collapsed: bool,
    pub(super) model: ComposerModel,
    /// Why the last Send or conversion did not work. It shows under the composer.
    error: Option<String>,
    format_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl Composer {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Navigator>) -> Self {
        let input = |window: &mut Window, cx: &mut Context<Navigator>, hint: &'static str| {
            cx.new(|cx| InputState::new(window, cx).placeholder(hint))
        };
        let topic = input(window, cx, "Topic");
        let content_type = input(window, cx, "Content type");
        let response_topic = input(window, cx, "Response topic");
        let correlation = input(window, cx, "Correlation data (hex)");
        let expiry = input(window, cx, "Message expiry (seconds)");
        let payload = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(false)
                .searchable(false)
                .soft_wrap(true)
                .placeholder("Payload")
        });
        let mut subs: Vec<Subscription> = [
            &topic,
            &content_type,
            &response_topic,
            &correlation,
            &expiry,
        ]
        .into_iter()
        .map(|i| {
            cx.subscribe(i, |v, _, ev: &InputEvent, _| {
                if matches!(ev, InputEvent::Change) {
                    v.comp.model.edited();
                }
            })
        })
        .collect();
        subs.push(cx.subscribe(&payload, |v, _, ev: &InputEvent, _| {
            if matches!(ev, InputEvent::Change) {
                v.comp.model.edited();
            }
        }));
        Self {
            topic: Field::new(topic),
            payload,
            content_type: Field::new(content_type),
            content_type_empty: false,
            response_topic: Field::new(response_topic),
            correlation,
            correlation_empty: false,
            expiry,
            rows: Vec::new(),
            next_row: 1,
            mode: PayloadMode::Text,
            qos: Qos::AtMostOnce,
            retain: false,
            utf8: false,
            props_open: false,
            collapsed: false,
            model: ComposerModel::default(),
            error: None,
            format_task: None,
            _subs: subs,
        }
    }

    /// The profile changed: the composer and History start empty.
    pub(super) fn reset(&mut self, window: &mut Window, cx: &mut App) {
        self.model.clear();
        self.error = None;
        self.format_task = None;
        self.mode = PayloadMode::Text;
        self.qos = Qos::AtMostOnce;
        self.retain = false;
        self.utf8 = false;
        self.rows.clear();
        self.content_type_empty = false;
        self.correlation_empty = false;
        for f in [
            &mut self.topic,
            &mut self.content_type,
            &mut self.response_topic,
        ] {
            f.set("", window, cx);
        }
        for i in [&self.correlation, &self.expiry] {
            i.update(cx, |i, cx| i.set_value("", window, cx));
        }
        self.payload.update(cx, |e, cx| e.set_value("", window, cx));
    }
}

impl Navigator {
    /// What the composer holds now.
    pub(super) fn read_draft(&self, cx: &App) -> Draft {
        let c = &self.comp;
        let text = |i: &Entity<InputState>| i.read(cx).value().to_string();
        // A field with text is present. An empty field is present only if the user said so.
        let present = |t: String, empty: bool| (!t.is_empty() || empty).then_some(t);
        Draft {
            topic: c.topic.get(cx),
            mode: c.mode,
            payload: c.payload.read(cx).value().to_string(),
            qos: c.qos,
            retain: c.retain,
            content_type: present(c.content_type.get(cx), c.content_type_empty),
            response_topic: c.response_topic.get(cx),
            correlation: present(text(&c.correlation), c.correlation_empty),
            expiry: text(&c.expiry),
            utf8: c.utf8,
            user: c
                .rows
                .iter()
                .map(|r| (r.key.get(cx), r.value.get(cx), r.keep_empty))
                .filter(|(k, v, keep)| !k.is_empty() || !v.is_empty() || *keep)
                .map(|(k, v, _)| (k, v))
                .collect(),
        }
    }

    fn new_row(
        &mut self,
        key: &str,
        value: &str,
        keep_empty: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> UserRow {
        let mk = |window: &mut Window, cx: &mut Context<Self>, hint: &'static str, text: &str| {
            let e = cx.new(|cx| InputState::new(window, cx).placeholder(hint));
            let mut f = Field::new(e);
            f.set(text, window, cx);
            f
        };
        let (k, v) = (mk(window, cx, "Name", key), mk(window, cx, "Value", value));
        let subs = [&k.input, &v.input]
            .into_iter()
            .map(|i| {
                cx.subscribe(i, |v, _, ev: &InputEvent, _| {
                    if matches!(ev, InputEvent::Change) {
                        v.comp.model.edited();
                    }
                })
            })
            .collect();
        let id = self.comp.next_row;
        self.comp.next_row += 1;
        UserRow {
            id,
            key: k,
            value: v,
            // A pair that comes from a draft or a message is sent also when it is empty.
            keep_empty: key.is_empty() && value.is_empty() && keep_empty,
            _subs: subs,
        }
    }

    /// Writes a draft into the composer fields. It does not touch History.
    fn apply_draft(&mut self, d: &Draft, window: &mut Window, cx: &mut Context<Self>) {
        self.comp.topic.set(&d.topic, window, cx);
        self.comp
            .content_type
            .set(d.content_type.as_deref().unwrap_or(""), window, cx);
        self.comp.content_type_empty = d.content_type.as_deref() == Some("");
        self.comp.response_topic.set(&d.response_topic, window, cx);
        let corr = d.correlation.clone().unwrap_or_default();
        self.comp.correlation_empty = d.correlation.as_deref() == Some("");
        self.comp
            .correlation
            .update(cx, |i, cx| i.set_value(corr, window, cx));
        self.comp
            .expiry
            .update(cx, |i, cx| i.set_value(d.expiry.clone(), window, cx));
        self.comp
            .payload
            .update(cx, |e, cx| e.set_value(d.payload.clone(), window, cx));
        self.comp.mode = d.mode;
        self.set_payload_hint(window, cx);
        self.comp.qos = d.qos;
        self.comp.retain = d.retain;
        self.comp.utf8 = d.utf8;
        let rows: Vec<UserRow> = d
            .user
            .iter()
            .map(|(k, v)| self.new_row(k, v, true, window, cx))
            .collect();
        self.comp.rows = rows;
        self.comp.props_open = d.content_type.is_some()
            || !d.response_topic.is_empty()
            || d.correlation.is_some()
            || !d.expiry.is_empty()
            || d.utf8
            || !d.user.is_empty();
    }

    fn set_payload_hint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let hint = match self.comp.mode {
            PayloadMode::Text => "Payload",
            PayloadMode::Hex => "Payload as hex bytes, for example 01 ff 7a",
        };
        self.comp
            .payload
            .update(cx, |e, cx| e.set_placeholder(hint, window, cx));
    }

    /// Saves the unsent draft that the composer has now, before anything replaces it. Recall,
    /// Edit, Reply, Publish to this topic, Format and the Text/Hex switch all call this.
    fn save_before_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.read_draft(cx);
        if !self.comp.model.before_replace(current) {
            window.push_notification(
                Notification::warning("The old draft is larger than 1 MiB. It is not kept."),
                cx,
            );
        }
    }

    /// EVERY replacement of the composer content goes through here: the current draft is saved
    /// first, if it is unsent and has content. Then the composer gets `d` and the focus moves to
    /// the payload.
    pub(super) fn replace_composer(
        &mut self,
        d: Draft,
        unsent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_before_replace(window, cx);
        self.apply_draft(&d, window, cx);
        self.comp.model.loaded(unsent);
        self.comp.error = None;
        self.comp.collapsed = false;
        self.comp.payload.update(cx, |e, cx| e.focus(window, cx));
        cx.notify();
    }

    /// Fills the composer from an entry of History. A draft leaves Drafts.
    fn recall(&mut self, seq: u64, draft: bool, window: &mut Window, cx: &mut Context<Self>) {
        let h = &mut self.comp.model.history;
        let d = if draft {
            h.take_draft(seq)
        } else {
            h.sent_draft(seq)
        };
        if let Some(d) = d {
            self.replace_composer(d, draft, window, cx);
        }
    }

    pub(super) fn set_payload_mode(
        &mut self,
        to: PayloadMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut d = self.read_draft(cx);
        let old = d.clone();
        match d.switch_mode(to) {
            Ok(()) => {
                if old.payload != d.payload {
                    // The unsent original stays in Drafts (Text and Hex are not the same text).
                    if !self.comp.model.replace_with(old, self.comp.model.unsent) {
                        window.push_notification(
                            Notification::warning(
                                "The old draft is larger than 1 MiB. It is not kept.",
                            ),
                            cx,
                        );
                    }
                }
                self.comp
                    .payload
                    .update(cx, |e, cx| e.set_value(d.payload, window, cx));
                self.comp.mode = to;
                self.comp.error = None;
                self.set_payload_hint(window, cx);
            }
            Err(e) => self.comp.error = Some(e),
        }
        cx.notify();
    }

    pub(super) fn toggle_composer(&mut self, cx: &mut Context<Self>) {
        self.comp.collapsed = !self.comp.collapsed;
        cx.notify();
    }

    pub(super) fn focus_publish_topic(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.comp.collapsed = false;
        self.comp
            .topic
            .input
            .update(cx, |i, cx| i.focus(window, cx));
        cx.notify();
    }

    /// Formats the payload as JSON. The work runs in the background. A result for text that the
    /// user has changed since is dropped.
    pub(super) fn format_json(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.comp.mode != PayloadMode::Text {
            return;
        }
        let size = self.comp.payload.read(cx).text().len();
        if size > FORMAT_MAX_IN {
            self.comp.error = Some(format!(
                "Format works up to {}. This payload has {}.",
                size_text(FORMAT_MAX_IN),
                size_text(size)
            ));
            cx.notify();
            return;
        }
        let snapshot = self.comp.payload.read(cx).value().to_string();
        let text = snapshot.clone();
        self.comp.format_task = Some(cx.spawn_in(window, async move |this, cx| {
            let out = cx
                .background_executor()
                .spawn(async move {
                    let bytes = text.into_bytes();
                    decode::render(Decoder::Json, &bytes, bytes.len(), &AtomicBool::new(false))
                })
                .await;
            this.update_in(cx, |v, window, cx| {
                if v.comp.payload.read(cx).value() != snapshot.as_str() {
                    return;
                }
                match out.error {
                    Some(e) => v.comp.error = Some(e),
                    None if out.text.len() > FORMAT_MAX_OUT => {
                        v.comp.error = Some(format!(
                            "The formatted text has {}. Format works up to {}.",
                            size_text(out.text.len()),
                            size_text(FORMAT_MAX_OUT)
                        ));
                    }
                    None => {
                        let old = v.read_draft(cx);
                        if !v.comp.model.replace_with(old, true) {
                            window.push_notification(
                                Notification::warning(
                                    "The old draft is larger than 1 MiB. It is not kept.",
                                ),
                                cx,
                            );
                        }
                        v.comp
                            .payload
                            .update(cx, |e, cx| e.set_value(out.text, window, cx));
                        v.comp.error = None;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Ctrl/Cmd+Enter and the Send button.
    pub(super) fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.connected() {
            self.comp.error = Some("Connect to a broker first.".into());
            cx.notify();
            return;
        }
        let d = self.read_draft(cx);
        self.publish_draft(&d, true, window, cx);
    }

    /// Checks `d` with the core rules, sends it and records it in the list. `from_composer`: the
    /// draft goes to Sent and errors show under the composer.
    pub(super) fn publish_draft(
        &mut self,
        d: &Draft,
        from_composer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let op_id = self.next_op;
        let checked = d
            .to_outgoing(op_id)
            .and_then(|o| validate_outgoing(&o, self.net.broker_max_packet()).map(|()| o));
        let out = match checked {
            Ok(o) => o,
            Err(e) => {
                if from_composer {
                    self.comp.error = Some(e);
                } else {
                    window.push_notification(Notification::error(e), cx);
                }
                cx.notify();
                return;
            }
        };
        self.next_op += 1;
        match self.net.send(Command::Publish(Box::new(out.clone()))) {
            Ok(()) => {
                // The list row shows the progress. The broker sends the message back as a
                // normal received message if a subscription matches.
                self.capture_anchor();
                self.store.record_sent(
                    op_id,
                    &out.topic,
                    out.payload,
                    out.qos,
                    out.retain,
                    out.props,
                    SystemTime::now(),
                );
                self.restore_anchor();
                self.last_result = Some((op_id, status_text(&SendStatus::Sending)));
                if from_composer {
                    self.comp.model.sent(d.clone());
                    self.comp.error = None;
                }
            }
            Err(e) => {
                let why = send_error_text(e).to_string();
                if from_composer {
                    self.comp.error = Some(why);
                } else {
                    window.push_notification(Notification::error(why), cx);
                }
            }
        }
        cx.notify();
    }

    pub(super) fn render_composer(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let connected = self.connected();
        let collapsed = self.comp.collapsed;
        let result = self
            .last_result
            .as_ref()
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        let target = self.profile.as_ref().map(display_name);

        let drafts: Vec<(u64, SharedString)> = self
            .comp
            .model
            .history
            .drafts()
            .map(|e| (e.seq, e.label.clone().into()))
            .collect();
        let sent: Vec<(u64, SharedString)> = self
            .comp
            .model
            .history
            .sent()
            .map(|e| (e.seq, e.label.clone().into()))
            .collect();
        let weak = cx.entity().downgrade();
        let rem = window.rem_size();
        let history = DropdownButton::new("composer-history")
            .button(
                Button::new("composer-history-button")
                    .small()
                    .ghost()
                    .label("History")
                    .dropdown_caret(true),
            )
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu
                    .min_w(ui(280.).to_pixels(rem))
                    .max_w(ui(520.).to_pixels(rem))
                    .max_h(ui(360.).to_pixels(rem))
                    .scrollable(true);
                if drafts.is_empty() && sent.is_empty() {
                    return menu.item(PopupMenuItem::new("No history yet.").disabled(true));
                }
                for (title, list, is_draft) in [("Drafts", &drafts, true), ("Sent", &sent, false)] {
                    if list.is_empty() {
                        continue;
                    }
                    menu = menu.label(title);
                    for (seq, label) in list {
                        let (weak, seq) = (weak.clone(), *seq);
                        menu = menu.item(PopupMenuItem::new(label.clone()).on_click(
                            move |_, window, cx| {
                                weak.update(cx, |v, cx| v.recall(seq, is_draft, window, cx))
                                    .ok();
                            },
                        ));
                    }
                }
                menu
            });

        let topic_note = self.comp.topic.note("topic", cx);
        let header = h_flex()
            .gap_2()
            .items_center()
            .child(div().text_xs().text_color(muted).child("Publish"))
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&self.comp.topic.input).small()),
            )
            .child(history)
            .child(
                Button::new("composer-collapse")
                    .small()
                    .ghost()
                    .icon(if collapsed {
                        IconName::ChevronUp
                    } else {
                        IconName::ChevronDown
                    })
                    .tooltip(if collapsed {
                        "Show the composer"
                    } else {
                        "Collapse the composer to one line"
                    })
                    .on_click(cx.listener(|v, _, _, cx| v.toggle_composer(cx))),
            );

        let send = h_flex()
            .gap_2()
            .items_center()
            .when_some(target, |d, name| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("to {name}")),
                )
            })
            .child(
                Button::new("send")
                    .small()
                    .primary()
                    .label(format!("Send ({}+Enter)", crate::palette::MOD))
                    .disabled(!connected)
                    .on_click(cx.listener(|v, _, window, cx| v.send(window, cx))),
            );

        let status = div()
            .flex_1()
            .min_w_0()
            .text_xs()
            .text_color(muted)
            .child(if connected {
                result
            } else {
                "Connect to a broker first.".into()
            });

        let body = if collapsed {
            // The head of the text only: a large payload must not be copied for each frame.
            let one_line = {
                let rope = self.comp.payload.read(cx).text();
                let head = rope.slice(..rope.len_chars().min(512)).to_string();
                decode::preview(head.as_bytes(), 200)
            };
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(muted)
                        .child(one_line),
                )
                .child(send)
                .into_any_element()
        } else {
            let mode_ix = usize::from(self.comp.mode == PayloadMode::Hex);
            // A toggle group with a selected state. A click on the selected one keeps it.
            let qos = ToggleGroup::new("qos")
                .segmented()
                .single()
                .small()
                .child(
                    Toggle::new("qos-0")
                        .label("QoS 0")
                        .checked(self.comp.qos == Qos::AtMostOnce),
                )
                .child(
                    Toggle::new("qos-1")
                        .label("QoS 1")
                        .checked(self.comp.qos == Qos::AtLeastOnce),
                )
                .on_click(cx.listener(|v, checks: &Vec<bool>, _, cx| {
                    if let Some(ix) = checks.iter().position(|c| *c) {
                        v.comp.qos = if ix == 0 {
                            Qos::AtMostOnce
                        } else {
                            Qos::AtLeastOnce
                        };
                        v.comp.model.edited();
                        cx.notify();
                    }
                }));
            v_flex()
                .gap_2()
                .child(
                    Editor::new(&self.comp.payload)
                        .h(ui(PAYLOAD_HEIGHT))
                        .bordered(true)
                        .small(),
                )
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_2()
                        .items_center()
                        .child(
                            div().w(ui(110.)).child(
                                TabBar::new("payload-mode")
                                    .segmented()
                                    .small()
                                    .selected_index(mode_ix)
                                    .on_click(cx.listener(|v, ix: &usize, window, cx| {
                                        let to = if *ix == 0 {
                                            PayloadMode::Text
                                        } else {
                                            PayloadMode::Hex
                                        };
                                        v.set_payload_mode(to, window, cx);
                                    }))
                                    .child("Text")
                                    .child("Hex"),
                            ),
                        )
                        .child(
                            Button::new("format-json")
                                .small()
                                .ghost()
                                .label("Format")
                                .disabled(self.comp.mode == PayloadMode::Hex)
                                .on_click(
                                    cx.listener(|v, _, window, cx| v.format_json(window, cx)),
                                ),
                        )
                        .child(qos)
                        .child(
                            Switch::new("retain")
                                .checked(self.comp.retain)
                                .label("Retain")
                                .on_click(cx.listener(|v, on: &bool, _, cx| {
                                    v.comp.retain = *on;
                                    v.comp.model.edited();
                                    cx.notify();
                                })),
                        ),
                )
                .child(self.render_props(cx))
                .when_some(topic_note, |d, n| {
                    d.child(div().text_xs().text_color(muted).child(n))
                })
                .when_some(self.comp.error.clone(), |d, e| {
                    d.child(div().text_xs().text_color(theme.danger).child(e))
                })
                .child(h_flex().gap_2().items_center().child(status).child(send))
                .into_any_element()
        };

        v_flex()
            .key_context("Composer")
            .border_t_1()
            .border_color(theme.border)
            .p_2()
            .gap_2()
            .child(header)
            .child(body)
    }

    /// The "Properties" section: content type, response topic, correlation data, message expiry,
    /// payload format and user properties. "Send empty" keeps a property that is present but
    /// empty. A line break in a text is kept (see [`LineKeep`]).
    fn render_props(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.comp.props_open;
        let muted = cx.theme().muted_foreground;
        let note = move |n: Option<String>| n.map(|n| div().text_xs().text_color(muted).child(n));
        let empty_switch =
            |id: &'static str, on: bool| Switch::new(id).checked(on).label("Send empty");
        let rows = self.comp.rows.iter().map(|r| {
            let id = r.id;
            let n = r.key.note("name", cx).or_else(|| r.value.note("value", cx));
            v_flex()
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&r.key.input).small()))
                        .child(div().flex_1().child(Input::new(&r.value.input).small()))
                        .child(
                            Switch::new(("user-prop-empty", id as usize))
                                .checked(r.keep_empty)
                                .label("Send empty")
                                .on_click(cx.listener(move |v, on: &bool, _, cx| {
                                    if let Some(r) = v.comp.rows.iter_mut().find(|r| r.id == id) {
                                        r.keep_empty = *on;
                                    }
                                    v.comp.model.edited();
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(("user-prop-remove", id as usize))
                                .small()
                                .ghost()
                                .icon(IconName::Close)
                                .tooltip("Remove this property")
                                .on_click(cx.listener(move |v, _, _, cx| {
                                    v.comp.rows.retain(|r| r.id != id);
                                    v.comp.model.edited();
                                    cx.notify();
                                })),
                        ),
                )
                .children(note(n))
        });
        Collapsible::new()
            .open(open)
            .child(
                Button::new("props-toggle")
                    .xsmall()
                    .ghost()
                    .icon(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .label("Properties")
                    .on_click(cx.listener(|v, _, _, cx| {
                        v.comp.props_open = !v.comp.props_open;
                        cx.notify();
                    })),
            )
            .content(
                v_flex()
                    .gap_1()
                    .pt_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&self.comp.content_type.input).small()),
                            )
                            .child(
                                empty_switch("content-type-empty", self.comp.content_type_empty)
                                    .on_click(cx.listener(|v, on: &bool, _, cx| {
                                        v.comp.content_type_empty = *on;
                                        v.comp.model.edited();
                                        cx.notify();
                                    })),
                            ),
                    )
                    .children(note(self.comp.content_type.note("value", cx)))
                    .child(Input::new(&self.comp.response_topic.input).small())
                    .children(note(self.comp.response_topic.note("value", cx)))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&self.comp.correlation).small()),
                            )
                            .child(
                                empty_switch("correlation-empty", self.comp.correlation_empty)
                                    .on_click(cx.listener(|v, on: &bool, _, cx| {
                                        v.comp.correlation_empty = *on;
                                        v.comp.model.edited();
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(Input::new(&self.comp.expiry).small())
                    .child(
                        Switch::new("payload-utf8")
                            .checked(self.comp.utf8)
                            .label("Payload format: UTF-8")
                            .on_click(cx.listener(|v, on: &bool, _, cx| {
                                v.comp.utf8 = *on;
                                v.comp.model.edited();
                                cx.notify();
                            })),
                    )
                    .children(rows)
                    .child(
                        div().child(
                            Button::new("add-user-prop")
                                .xsmall()
                                .icon(IconName::Plus)
                                .label("Add property")
                                .on_click(cx.listener(|v, _, window, cx| {
                                    let row = v.new_row("", "", false, window, cx);
                                    v.comp.rows.push(row);
                                    cx.notify();
                                })),
                        ),
                    ),
            )
    }
}
