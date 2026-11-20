//! The model of the publish composer. It has no gpui types, so that tests can run it.
//!
//! The UI reads the fields of the composer into a [`Draft`] and writes a `Draft` back. This
//! module decides what a draft is, how Text and Hex convert, which drafts are kept, and which
//! actions fill the composer from a message.

use std::borrow::Cow;
use std::collections::VecDeque;

use bytes::Bytes;
use navigator_core::net::{Outgoing, Protocol};
use navigator_core::publish::{parse_hex, to_hex};
use navigator_core::store::Msg;
use navigator_core::text::display_safe;
use navigator_core::{MsgProps, Qos, decode};

/// Most bytes that Drafts and Sent keep together.
pub const HISTORY_BYTES: usize = 1024 * 1024;
/// Most drafts that History keeps.
pub const MAX_DRAFTS: usize = 5;
/// Most sent messages that History keeps.
pub const MAX_SENT: usize = 20;
/// Shown when "Edit and publish" or "Reply" drops properties on an MQTT 3.1.1 connection.
pub const V3_NO_PROPERTIES: &str = "MQTT 3.1.1 does not send properties.";
/// Shown when the payload is not stored completely.
pub const NOT_STORED: &str = "The full payload is not stored.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PayloadMode {
    #[default]
    Text,
    Hex,
    /// Text that the composer checks as JSON and highlights. It is sent as is, also when it is
    /// not valid JSON.
    Json,
}

impl PayloadMode {
    /// Text and JSON both hold the payload text itself. Hex holds hex digits.
    pub fn is_text(self) -> bool {
        self != PayloadMode::Hex
    }
}

/// Everything the user can set in the composer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    pub topic: String,
    pub mode: PayloadMode,
    /// The text of the payload editor: the payload itself (Text, JSON) or its hex digits (Hex).
    pub payload: String,
    pub qos: Qos,
    pub retain: bool,
    /// `Some("")` is a present but empty content type. `None` is no content type.
    pub content_type: Option<String>,
    /// An empty response topic is not valid in MQTT, so empty means none.
    pub response_topic: String,
    /// Hex digits. `Some("")` is present but empty correlation data.
    pub correlation: Option<String>,
    /// Seconds, as typed.
    pub expiry: String,
    /// Payload format indicator "UTF-8".
    pub utf8: bool,
    /// Every pair is sent, also an empty pair. The UI leaves out the rows that the user did not
    /// use.
    pub user: Vec<(String, String)>,
}

impl Draft {
    /// True if the draft is worth keeping. The topic alone is not.
    pub fn has_content(&self) -> bool {
        !self.payload.is_empty()
            || self.content_type.is_some()
            || !self.response_topic.is_empty()
            || self.correlation.is_some()
            || !self.expiry.is_empty()
            || !self.user.is_empty()
    }

    /// Approximate memory, in bytes.
    pub fn size(&self) -> usize {
        self.topic.len()
            + self.payload.len()
            + self.content_type.as_deref().map_or(0, str::len)
            + self.response_topic.len()
            + self.correlation.as_deref().map_or(0, str::len)
            + self.expiry.len()
            + self
                .user
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>()
            + 64
    }

    /// The payload bytes. Text and JSON send the characters as UTF-8. Hex sends the bytes of the
    /// digits.
    pub fn payload_bytes(&self) -> Result<Vec<u8>, String> {
        match self.mode {
            PayloadMode::Text | PayloadMode::Json => Ok(self.payload.clone().into_bytes()),
            PayloadMode::Hex => parse_hex(&self.payload).map_err(|e| format!("Payload: {e}")),
        }
    }

    /// Changes the payload mode and converts the payload exactly. `Err` is the text for the
    /// user, and the draft does not change.
    pub fn switch_mode(&mut self, to: PayloadMode) -> Result<(), String> {
        // Text and JSON hold the same text: only the mode changes.
        if self.mode == to || (self.mode.is_text() && to.is_text()) {
            self.mode = to;
            return Ok(());
        }
        match to {
            PayloadMode::Hex => self.payload = to_hex(self.payload.as_bytes()),
            PayloadMode::Text | PayloadMode::Json => {
                let bytes = parse_hex(&self.payload).map_err(|e| format!("Payload: {e}"))?;
                self.payload = String::from_utf8(bytes)
                    .map_err(|_| "These bytes are not valid text. Keep Hex mode.".to_string())?;
            }
        }
        self.mode = to;
        Ok(())
    }

    /// The message to send. The core checks the result with `validate_outgoing`.
    pub fn to_outgoing(&self, op_id: u64) -> Result<Outgoing, String> {
        let payload = Bytes::from(self.payload_bytes()?);
        let correlation = self
            .correlation
            .as_deref()
            .map(|h| parse_hex(h).map_err(|e| format!("Correlation data: {e}")))
            .transpose()?;
        let expiry = self.expiry.trim();
        let message_expiry = if expiry.is_empty() {
            None
        } else {
            Some(expiry.parse::<u32>().map_err(|_| {
                "The message expiry must be a whole number of seconds, from 0 to 4294967295."
                    .to_string()
            })?)
        };
        Ok(Outgoing {
            op_id,
            topic: self.topic.clone(),
            payload,
            qos: self.qos,
            retain: self.retain,
            props: MsgProps {
                content_type: self.content_type.clone(),
                response_topic: (!self.response_topic.is_empty())
                    .then(|| self.response_topic.clone()),
                correlation_data: correlation.map(Bytes::from),
                message_expiry,
                user: self.user.clone(),
                payload_utf8: self.utf8,
            },
        })
    }

    /// Clears every MQTT 5 property (an MQTT 3.1.1 message has none). Topic, payload, QoS and
    /// retain stay. Returns true if the draft had a property.
    pub fn drop_properties(&mut self) -> bool {
        let had = self.content_type.is_some()
            || !self.response_topic.is_empty()
            || self.correlation.is_some()
            || !self.expiry.is_empty()
            || self.utf8
            || !self.user.is_empty();
        self.content_type = None;
        self.response_topic.clear();
        self.correlation = None;
        self.expiry.clear();
        self.utf8 = false;
        self.user.clear();
        had
    }

    /// "Edit and publish": the exact bytes, QoS, retain and all properties of `msg`. A payload
    /// that is not valid UTF-8 opens in Hex mode. The caller checks [`edit_unavailable`] first.
    pub fn from_message(topic: &str, msg: &Msg) -> Self {
        let (mode, payload) = match std::str::from_utf8(&msg.payload) {
            Ok(s) => (PayloadMode::Text, s.to_string()),
            Err(_) => (PayloadMode::Hex, to_hex(&msg.payload)),
        };
        let p = &msg.props;
        Self {
            topic: topic.to_string(),
            mode,
            payload,
            qos: msg.qos,
            retain: msg.retain,
            content_type: p.content_type.clone(),
            response_topic: p.response_topic.clone().unwrap_or_default(),
            correlation: p.correlation_data.as_deref().map(to_hex),
            expiry: p.message_expiry.map(|n| n.to_string()).unwrap_or_default(),
            utf8: p.payload_utf8,
            user: p.user.clone(),
        }
    }

    /// "Reply": the response topic is the topic and the correlation data is copied. The payload
    /// is empty, Retain is off and the other properties are empty. `None` without a response
    /// topic. `qos` is the QoS the composer has now.
    pub fn reply(msg: &Msg, qos: Qos) -> Option<Self> {
        let topic = msg
            .props
            .response_topic
            .as_deref()
            .filter(|t| !t.is_empty())?;
        Some(Self {
            topic: topic.to_string(),
            qos,
            correlation: msg.props.correlation_data.as_deref().map(to_hex),
            ..Self::default()
        })
    }

    /// One line for the History menu.
    fn label(&self) -> String {
        let payload = match self.mode {
            PayloadMode::Text | PayloadMode::Json => decode::preview(self.payload.as_bytes(), 40),
            PayloadMode::Hex => decode::preview(self.payload.as_bytes(), 24),
        };
        format!("{}  {}", display_safe(&self.topic), payload)
    }
}

/// True if "Edit and publish" or "Reply" must say "MQTT 3.1.1 does not send properties.": the
/// target connection is MQTT 3.1.1 and the source message has an MQTT 5 property. The decision
/// uses the source message, because a draft that Reply builds can be empty of properties even
/// when the message had a Response Topic.
pub fn v3_drops_properties(target: Protocol, source: &MsgProps) -> bool {
    target == Protocol::V311 && !crate::inspector::props_rows(source).is_empty()
}

/// `Some(reason)` if "Edit and publish" must be off: the stored payload is not the full payload
/// (cut, or the list row is gone).
pub fn edit_unavailable(msg: &Msg, row_evicted: bool) -> Option<&'static str> {
    (msg.truncated || row_evicted || msg.payload.len() != msg.original_size).then_some(NOT_STORED)
}

/// A single-line input removes CR and LF. A text view also shapes bidi controls and joiners, so
/// a topic or a property with them would look like another string. `LineKeep` shows such a
/// string escaped (`\n`, `\r`, `\\` for line breaks, `⟨U+202E⟩` for hidden characters) and keeps
/// the exact string until the user edits the field. Then the text of the field is the value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineKeep(Option<(String, String)>);

/// The note that the UI shows under a field with a kept line break. `what`: "topic", "value".
pub fn line_breaks_note(what: &str) -> String {
    format!("This {what} has line breaks; they are kept.")
}

/// The note for a field with kept hidden characters.
pub fn hidden_chars_note(what: &str) -> String {
    format!("This {what} has hidden characters; they are kept.")
}

impl LineKeep {
    /// Registers the exact string of a field. Returns the text for the input.
    pub fn set(&mut self, exact: &str) -> String {
        let breaks = exact.contains(['\n', '\r']);
        let safe = display_safe(exact);
        if matches!(safe, Cow::Borrowed(_)) {
            self.0 = None;
            return exact.to_string();
        }
        let shown = if breaks {
            display_safe(
                &exact
                    .replace('\\', "\\\\")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r"),
            )
            .into_owned()
        } else {
            safe.into_owned()
        };
        self.0 = Some((shown.clone(), exact.to_string()));
        shown
    }

    /// The value of the field. `current` is the text in the input.
    pub fn value(&self, current: &str) -> String {
        match &self.0 {
            Some((shown, exact)) if shown == current => exact.clone(),
            _ => current.to_string(),
        }
    }

    /// True if the exact string (not the text of the input) is still the value.
    #[cfg(test)]
    fn active(&self, current: &str) -> bool {
        self.0.as_ref().is_some_and(|(shown, _)| shown == current)
    }

    /// The note for the field, if the exact string (not the text of the input) is still the
    /// value. `what`: "topic", "value".
    pub fn note(&self, what: &str, current: &str) -> Option<String> {
        let (_, exact) = self.0.as_ref().filter(|(shown, _)| shown == current)?;
        let breaks = exact.contains(['\n', '\r']);
        let hidden = matches!(
            display_safe(&exact.replace(['\n', '\r'], "")),
            Cow::Owned(_)
        );
        Some(match (breaks, hidden) {
            (true, true) => {
                format!("This {what} has line breaks and hidden characters; they are kept.")
            }
            (true, false) => line_breaks_note(what),
            _ => hidden_chars_note(what),
        })
    }
}

/// One entry of Drafts or Sent.
#[derive(Clone, Debug)]
pub struct Entry {
    /// Grows with each entry, so the entry is found again after the lists change.
    pub seq: u64,
    pub label: String,
    pub draft: Draft,
}

/// Drafts (replaced, unsent) and Sent (this session). Memory only. Newest first.
#[derive(Debug)]
pub struct History {
    drafts: VecDeque<Entry>,
    sent: VecDeque<Entry>,
    next_seq: u64,
    cap: usize,
}

impl Default for History {
    fn default() -> Self {
        Self::with_cap(HISTORY_BYTES)
    }
}

impl History {
    pub fn with_cap(cap: usize) -> Self {
        Self {
            drafts: VecDeque::new(),
            sent: VecDeque::new(),
            next_seq: 1,
            cap,
        }
    }

    pub fn drafts(&self) -> impl Iterator<Item = &Entry> {
        self.drafts.iter()
    }

    pub fn sent(&self) -> impl Iterator<Item = &Entry> {
        self.sent.iter()
    }

    pub fn bytes(&self) -> usize {
        self.drafts
            .iter()
            .chain(&self.sent)
            .map(|e| e.draft.size())
            .sum()
    }

    pub fn clear(&mut self) {
        self.drafts.clear();
        self.sent.clear();
    }

    /// Keeps a draft. `false`: the draft has no content (nothing to keep), or it is larger than
    /// the whole budget (it is not kept, and the older entries stay).
    pub fn save_draft(&mut self, draft: Draft) -> bool {
        if !draft.has_content() {
            return true;
        }
        if self.drafts.front().is_some_and(|e| e.draft == draft) {
            return true;
        }
        let ok = self.push(draft, true);
        self.drafts.truncate(MAX_DRAFTS);
        ok
    }

    /// Keeps a message that the user sent.
    pub fn record_sent(&mut self, draft: Draft) -> bool {
        let ok = self.push(draft, false);
        self.sent.truncate(MAX_SENT);
        ok
    }

    /// Removes a draft from Drafts and gives it back (it goes into the composer).
    pub fn take_draft(&mut self, seq: u64) -> Option<Draft> {
        let i = self.drafts.iter().position(|e| e.seq == seq)?;
        self.drafts.remove(i).map(|e| e.draft)
    }

    pub fn sent_draft(&self, seq: u64) -> Option<Draft> {
        self.sent
            .iter()
            .find(|e| e.seq == seq)
            .map(|e| e.draft.clone())
    }

    fn push(&mut self, draft: Draft, is_draft: bool) -> bool {
        if draft.size() > self.cap {
            return false;
        }
        let entry = Entry {
            seq: self.next_seq,
            label: draft.label(),
            draft,
        };
        self.next_seq += 1;
        if is_draft {
            self.drafts.push_front(entry);
        } else {
            self.sent.push_front(entry);
        }
        // Drop the oldest entry of both lists until the budget holds.
        while self.bytes() > self.cap {
            let oldest = |l: &VecDeque<Entry>| l.back().map(|e| e.seq);
            match (oldest(&self.drafts), oldest(&self.sent)) {
                (Some(d), Some(s)) if d < s => self.drafts.pop_back(),
                (Some(_), None) => self.drafts.pop_back(),
                _ => self.sent.pop_back(),
            };
        }
        true
    }
}

/// Whether the composer shows, and its Properties section. The composer is closed by default.
/// Closing keeps the editors and the draft: this type holds the visibility only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Visibility {
    pub open: bool,
    /// The Properties section is open (MQTT 5).
    pub props: bool,
}

impl Visibility {
    /// Opens the composer. True if it was closed.
    pub fn open(&mut self) -> bool {
        !std::mem::replace(&mut self.open, true)
    }

    /// The one close operation (the Close item, `Esc` and the palette). True if the composer was
    /// open: then the caller must move the focus to the message list, so that the focus does
    /// not stay on a control that is gone. The Properties section keeps its state.
    pub fn close(&mut self) -> bool {
        std::mem::replace(&mut self.open, false)
    }
}

/// The MQTT 5 properties that the user can add to a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropKind {
    PayloadFormat,
    ContentType,
    ResponseTopic,
    Correlation,
    Expiry,
    User,
}

impl PropKind {
    /// The order of the "Add property" menu.
    pub const ALL: [PropKind; 6] = [
        PropKind::PayloadFormat,
        PropKind::ContentType,
        PropKind::ResponseTopic,
        PropKind::Correlation,
        PropKind::Expiry,
        PropKind::User,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PropKind::PayloadFormat => "Payload format (UTF-8)",
            PropKind::ContentType => "Content type",
            PropKind::ResponseTopic => "Response topic",
            PropKind::Correlation => "Correlation data",
            PropKind::Expiry => "Message expiry",
            PropKind::User => "User property",
        }
    }
}

/// Which single-field properties the Properties section shows. This is a state of its own,
/// separate from the text of the field: a field that the user adds stays visible while it is
/// empty. User properties are rows, so they need no flag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PropShown {
    pub payload_format: bool,
    pub content_type: bool,
    pub response_topic: bool,
    pub correlation: bool,
    pub expiry: bool,
}

impl PropShown {
    /// A draft shows the properties that it has (a History recall, Edit and publish, Reply).
    pub fn of_draft(d: &Draft) -> Self {
        Self {
            payload_format: d.utf8,
            content_type: d.content_type.is_some(),
            response_topic: !d.response_topic.is_empty(),
            correlation: d.correlation.is_some(),
            expiry: !d.expiry.is_empty(),
        }
    }

    fn flag(&mut self, kind: PropKind) -> Option<&mut bool> {
        match kind {
            PropKind::PayloadFormat => Some(&mut self.payload_format),
            PropKind::ContentType => Some(&mut self.content_type),
            PropKind::ResponseTopic => Some(&mut self.response_topic),
            PropKind::Correlation => Some(&mut self.correlation),
            PropKind::Expiry => Some(&mut self.expiry),
            PropKind::User => None,
        }
    }

    pub fn set(&mut self, kind: PropKind, shown: bool) {
        if let Some(f) = self.flag(kind) {
            *f = shown;
        }
    }

    pub fn is_shown(&self, kind: PropKind) -> bool {
        let mut me = *self;
        me.flag(kind).is_some_and(|f| *f)
    }

    /// The kinds that "Add property" offers: the single fields that are not shown, and the user
    /// property (any number of rows).
    pub fn addable(&self) -> Vec<PropKind> {
        PropKind::ALL
            .into_iter()
            .filter(|k| *k == PropKind::User || !self.is_shown(*k))
            .collect()
    }
}

/// The number in the "Properties (N)" tag: the properties that the message would carry. A
/// present but empty property ("Send empty") counts. A field that is shown but empty does not.
pub fn property_count(d: &Draft) -> usize {
    PropPresence {
        payload_format: d.utf8,
        content_type: d.content_type.is_some(),
        response_topic: !d.response_topic.is_empty(),
        correlation: d.correlation.is_some(),
        expiry: !d.expiry.is_empty(),
        user: d.user.len(),
    }
    .count()
}

/// Which properties are present, as flags. The window builds it from the small property fields
/// at each frame, without a `Draft`: a draft copies the whole payload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PropPresence {
    pub payload_format: bool,
    pub content_type: bool,
    pub response_topic: bool,
    pub correlation: bool,
    pub expiry: bool,
    pub user: usize,
}

impl PropPresence {
    pub fn count(&self) -> usize {
        usize::from(self.payload_format)
            + usize::from(self.content_type)
            + usize::from(self.response_topic)
            + usize::from(self.correlation)
            + usize::from(self.expiry)
            + self.user
    }
}

/// History and the "unsent" flag of the composer.
#[derive(Debug, Default)]
pub struct ComposerModel {
    pub history: History,
    /// The composer has content that the user typed or loaded and did not send.
    pub unsent: bool,
}

impl ComposerModel {
    /// Call this before EVERY replacement of the composer content, with what the composer has
    /// now. It keeps the current draft if it is not sent and has content. `false`: the draft
    /// is too large to keep.
    pub fn before_replace(&mut self, current: Draft) -> bool {
        if self.unsent {
            self.history.save_draft(current)
        } else {
            true
        }
    }

    /// Call this when `current` (the draft in the composer) is replaced in one step
    /// (Format, Text/Hex): the same save-before-replace as every other replacement. `unsent` is
    /// the flag after the replacement. `false`: the old draft is too large to keep.
    pub fn replace_with(&mut self, current: Draft, unsent: bool) -> bool {
        let ok = self.before_replace(current);
        self.loaded(unsent);
        ok
    }

    /// The user typed. `unsent` is true from now.
    pub fn edited(&mut self) {
        self.unsent = true;
    }

    /// The composer now holds `unsent` content (a recall of a draft, Edit, Reply, ...).
    pub fn loaded(&mut self, unsent: bool) {
        self.unsent = unsent;
    }

    /// The message was sent. It goes to Sent, and the composer content counts as sent.
    pub fn sent(&mut self, draft: Draft) {
        self.history.record_sent(draft);
        self.unsent = false;
    }

    /// The profile changed: forget everything.
    pub fn clear(&mut self) {
        self.history.clear();
        self.unsent = false;
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the gpui glob import brings its own `test` macro in the parent.
    use super::{
        ComposerModel, Draft, History, LineKeep, MAX_DRAFTS, MAX_SENT, NOT_STORED, PayloadMode,
        PropKind, PropPresence, PropShown, Visibility, edit_unavailable, hidden_chars_note,
        line_breaks_note, property_count, v3_drops_properties,
    };
    use bytes::Bytes;
    use navigator_core::net::Protocol;
    use navigator_core::store::Msg;
    use navigator_core::{MsgProps, Qos};
    use std::time::SystemTime;

    fn draft(topic: &str, payload: &str) -> Draft {
        Draft {
            topic: topic.into(),
            payload: payload.into(),
            ..Draft::default()
        }
    }

    fn msg(payload: &'static [u8], props: MsgProps) -> Msg {
        Msg {
            payload: Bytes::from_static(payload),
            truncated: false,
            original_size: payload.len(),
            qos: Qos::AtLeastOnce,
            retain: true,
            props,
            received_at: SystemTime::UNIX_EPOCH,
            protocol: Protocol::V5,
        }
    }

    #[test]
    fn an_mqtt_311_edit_keeps_topic_payload_qos_and_retain_and_drops_the_properties() {
        let props = MsgProps {
            content_type: Some("text/plain".into()),
            response_topic: Some("r/t".into()),
            correlation_data: Some(Bytes::from_static(b"\x01")),
            message_expiry: Some(30),
            user: vec![("k".into(), "v".into())],
            payload_utf8: true,
        };
        let mut d = Draft::from_message("a/b", &msg(b"hello", props));
        assert!(d.drop_properties());
        assert_eq!(
            d,
            Draft {
                topic: "a/b".into(),
                payload: "hello".into(),
                qos: Qos::AtLeastOnce,
                retain: true,
                ..Draft::default()
            }
        );
        // A message with no property has nothing to drop: no note.
        let mut plain = Draft::from_message("a/b", &msg(b"hello", MsgProps::default()));
        assert!(!plain.drop_properties());
        // A reply copies the correlation data. MQTT 3.1.1 drops it, and keeps the topic.
        let reply_props = MsgProps {
            response_topic: Some("reply/here".into()),
            correlation_data: Some(Bytes::from_static(b"\x02")),
            ..MsgProps::default()
        };
        let source = msg(b"q", reply_props);
        let mut r = Draft::reply(&source, Qos::AtMostOnce).expect("reply");
        assert!(r.drop_properties());
        assert_eq!(
            r,
            Draft {
                topic: "reply/here".into(),
                ..Draft::default()
            }
        );
    }

    #[test]
    fn the_notice_follows_the_source_message_and_the_target_protocol() {
        // A Response Topic alone is a property. The Reply draft has none left, but the notice is due.
        let only_response_topic = MsgProps {
            response_topic: Some("reply/here".into()),
            ..MsgProps::default()
        };
        let source = msg(b"q", only_response_topic.clone());
        let reply = Draft::reply(&source, Qos::AtMostOnce).expect("reply");
        assert!(!reply.clone().drop_properties());
        assert!(v3_drops_properties(Protocol::V311, &only_response_topic));
        // An MQTT 5 target sends properties: no notice. A source without properties: no notice.
        assert!(!v3_drops_properties(Protocol::V5, &only_response_topic));
        assert!(!v3_drops_properties(Protocol::V311, &MsgProps::default()));
    }

    #[test]
    fn a_history_entry_is_recalled_by_its_sequence_number() {
        // The History list is a column of buttons (Tab, then Enter or Space). A button gives
        // the sequence number of its entry to `take_draft` or `sent_draft`.
        let mut m = ComposerModel::default();
        m.edited();
        m.before_replace(draft("a", "one"));
        m.loaded(true);
        m.sent(draft("b", "two"));
        let d = m.history.drafts().next().expect("a draft").seq;
        let s = m.history.sent().next().expect("a sent entry").seq;
        assert_eq!(m.history.sent_draft(s).expect("sent").payload, "two");
        assert_eq!(m.history.sent().count(), 1, "Sent keeps its entry");
        assert_eq!(m.history.take_draft(d).expect("draft").payload, "one");
        assert_eq!(m.history.drafts().count(), 0, "a draft leaves Drafts");
        assert!(m.history.take_draft(d).is_none());
    }

    #[test]
    fn the_composer_is_closed_by_default_and_one_close_serves_all() {
        let mut v = Visibility::default();
        assert!(!v.open && !v.props);
        assert!(v.open(), "New message opens it");
        assert!(!v.open(), "opening again changes nothing");
        v.props = true;
        // Close (the item, Esc and the palette) reports that the focus must leave.
        assert!(v.close());
        assert!(!v.open && v.props, "the section keeps its state");
        assert!(!v.close(), "nothing to close: Esc has no work here");
    }

    #[test]
    fn a_property_shows_by_presence_not_by_text() {
        let mut d = draft("t", "");
        assert_eq!(PropShown::of_draft(&d), PropShown::default());
        assert_eq!(property_count(&d), 0);
        // An empty but present content type ("Send empty") shows and counts.
        d.content_type = Some(String::new());
        d.correlation = Some(String::new());
        d.user = vec![(String::new(), String::new())];
        let shown = PropShown::of_draft(&d);
        assert!(shown.content_type && shown.correlation && !shown.expiry);
        assert_eq!(property_count(&d), 3);
        // A response topic and an expiry are present when they have text. UTF-8 is a flag.
        d.response_topic = "r".into();
        d.expiry = "60".into();
        d.utf8 = true;
        assert_eq!(property_count(&d), 6);
        let shown = PropShown::of_draft(&d);
        assert!(shown.response_topic && shown.expiry && shown.payload_format);
        assert_eq!(
            shown.addable(),
            [PropKind::User],
            "only the user property is left"
        );
    }

    #[test]
    fn the_flag_count_equals_the_draft_count() {
        let mut d = draft("t", "payload");
        let flags = |d: &Draft| PropPresence {
            payload_format: d.utf8,
            content_type: d.content_type.is_some(),
            response_topic: !d.response_topic.is_empty(),
            correlation: d.correlation.is_some(),
            expiry: !d.expiry.is_empty(),
            user: d.user.len(),
        };
        assert_eq!(PropPresence::default().count(), 0);
        d.content_type = Some(String::new());
        d.utf8 = true;
        d.user = vec![("a".into(), "b".into()), (String::new(), String::new())];
        assert_eq!(flags(&d).count(), property_count(&d));
        assert_eq!(property_count(&d), 4);
    }

    #[test]
    fn a_field_that_the_user_adds_stays_visible_while_it_is_empty() {
        let mut shown = PropShown::default();
        assert_eq!(shown.addable().len(), 6);
        shown.set(PropKind::ContentType, true);
        // The draft has no text, so it has no property and the tag counts none.
        assert_eq!(property_count(&draft("t", "")), 0);
        assert!(shown.is_shown(PropKind::ContentType));
        assert!(!shown.addable().contains(&PropKind::ContentType));
        assert!(!shown.is_shown(PropKind::User), "user rows have no flag");
        shown.set(PropKind::ContentType, false);
        assert!(shown.addable().contains(&PropKind::ContentType));
    }

    #[test]
    fn every_replacement_saves_an_unsent_draft() {
        let mut m = ComposerModel::default();
        // Nothing typed: nothing to save.
        assert!(m.before_replace(draft("a", "")));
        m.edited();
        assert!(m.before_replace(draft("a", "")));
        assert_eq!(m.history.drafts().count(), 0, "no content");
        // Typed content: History recall, Edit, Reply and Publish to this topic all call the
        // same function, so each one saves.
        for i in 0..3 {
            m.edited();
            assert!(m.before_replace(draft("a", &format!("p{i}"))));
            m.loaded(true);
        }
        assert_eq!(m.history.drafts().count(), 3);
        // A sent draft is not saved again.
        m.sent(draft("a", "sent"));
        assert!(m.before_replace(draft("a", "sent")));
        assert_eq!(m.history.drafts().count(), 3);
        assert_eq!(m.history.sent().count(), 1);
        // The same draft twice in a row is one entry.
        m.edited();
        m.before_replace(draft("a", "p2"));
        assert_eq!(m.history.drafts().count(), 3);
    }

    #[test]
    fn counts_and_bytes_are_capped() {
        let mut h = History::default();
        for i in 0..10 {
            h.save_draft(draft("t", &format!("d{i}")));
        }
        for i in 0..30 {
            h.record_sent(draft("t", &format!("s{i}")));
        }
        assert_eq!(h.drafts().count(), MAX_DRAFTS);
        assert_eq!(h.sent().count(), MAX_SENT);
        assert_eq!(
            h.drafts().next().map(|e| e.draft.payload.as_str()),
            Some("d9")
        );
        // 600 bytes budget: older entries go first, from both lists.
        let mut h = History::with_cap(600);
        h.save_draft(draft("t", &"a".repeat(100)));
        h.record_sent(draft("t", &"b".repeat(100)));
        h.save_draft(draft("t", &"c".repeat(100)));
        h.record_sent(draft("t", &"d".repeat(100)));
        assert!(h.bytes() <= 600);
        assert_eq!(h.drafts().count() + h.sent().count(), 3);
        assert!(h.drafts().all(|e| e.draft.payload != "a".repeat(100)));
        // One entry over the whole budget is refused, and the others stay.
        assert!(!h.save_draft(draft("t", &"x".repeat(1000))));
        assert_eq!(h.drafts().count() + h.sent().count(), 3);
        // A recalled draft leaves Drafts.
        let seq = h.drafts().next().expect("a draft").seq;
        assert!(h.take_draft(seq).is_some());
        assert!(h.take_draft(seq).is_none());
        h.clear();
        assert_eq!(h.bytes(), 0);
    }

    #[test]
    fn text_and_hex_convert_exactly() {
        let mut d = draft("t", "héllo\r\n\0 \t\"");
        let before = d.payload.clone();
        d.switch_mode(PayloadMode::Hex).expect("hex");
        assert_eq!(d.payload_bytes().expect("bytes"), before.as_bytes());
        d.switch_mode(PayloadMode::Text).expect("text");
        assert_eq!(d.payload, before);
        // Hex that is not valid UTF-8 stays Hex, with the message.
        let mut d = draft("t", "ff fe 00");
        d.mode = PayloadMode::Hex;
        let e = d.switch_mode(PayloadMode::Text).expect_err("not text");
        assert_eq!(e, "These bytes are not valid text. Keep Hex mode.");
        assert_eq!((d.mode, d.payload.as_str()), (PayloadMode::Hex, "ff fe 00"));
        // Bad hex digits are an error, too, and nothing changes.
        d.payload = "zz".into();
        assert!(d.switch_mode(PayloadMode::Text).is_err());
        assert_eq!(d.payload, "zz");
        // Empty payload both ways.
        let mut d = draft("t", "");
        d.switch_mode(PayloadMode::Hex).expect("hex");
        assert_eq!(d.payload, "");
        d.switch_mode(PayloadMode::Text).expect("text");
    }

    #[test]
    fn edit_keeps_the_exact_bytes_and_properties() {
        let props = MsgProps {
            content_type: Some("a/b".into()),
            response_topic: Some("r".into()),
            correlation_data: Some(Bytes::from_static(&[1, 0xab])),
            message_expiry: Some(30),
            user: vec![("k".into(), "1".into()), ("k".into(), "2".into())],
            payload_utf8: false,
        };
        let m = msg(&[0xff, 0x00, 0x7f], props.clone());
        let d = Draft::from_message("t", &m);
        assert_eq!((d.mode, d.payload.as_str()), (PayloadMode::Hex, "ff 00 7f"));
        assert!(d.retain && d.qos == Qos::AtLeastOnce);
        let out = d.to_outgoing(7).expect("valid");
        assert_eq!(&out.payload[..], &[0xff, 0x00, 0x7f]);
        assert_eq!(
            out.props, props,
            "duplicate user properties keep their order"
        );
        let m = msg(b"text", MsgProps::default());
        let d = Draft::from_message("t", &m);
        assert_eq!((d.mode, d.payload.as_str()), (PayloadMode::Text, "text"));
    }

    #[test]
    fn reply_follows_the_field_rules() {
        let props = MsgProps {
            content_type: Some("a/b".into()),
            response_topic: Some("reply/to".into()),
            correlation_data: Some(Bytes::from_static(&[0xca, 0xfe])),
            message_expiry: Some(5),
            user: vec![("k".into(), "v".into())],
            payload_utf8: true,
        };
        let m = msg(b"ask", props);
        let d = Draft::reply(&m, Qos::AtMostOnce).expect("has a response topic");
        assert_eq!(d.topic, "reply/to");
        assert_eq!(d.correlation.as_deref(), Some("ca fe"));
        assert_eq!(d.payload, "");
        assert!(!d.retain && !d.utf8);
        assert!(d.content_type.is_none() && d.expiry.is_empty() && d.user.is_empty());
        assert_eq!(d.qos, Qos::AtMostOnce);
        // No response topic, no Reply. An empty one counts as none.
        assert!(Draft::reply(&msg(b"x", MsgProps::default()), Qos::AtMostOnce).is_none());
        let empty = MsgProps {
            response_topic: Some(String::new()),
            ..MsgProps::default()
        };
        assert!(Draft::reply(&msg(b"x", empty), Qos::AtMostOnce).is_none());
    }

    #[test]
    fn edit_needs_the_full_payload() {
        let mut m = msg(b"abc", MsgProps::default());
        assert_eq!(edit_unavailable(&m, false), None);
        assert_eq!(edit_unavailable(&m, true), Some(NOT_STORED));
        m.truncated = true;
        assert_eq!(edit_unavailable(&m, false), Some(NOT_STORED));
        m.truncated = false;
        m.original_size = 10;
        assert_eq!(edit_unavailable(&m, false), Some(NOT_STORED));
    }

    #[test]
    fn outgoing_reads_the_properties() {
        let mut d = draft("t", "x");
        d.content_type = Some("text/plain".into());
        d.correlation = Some("01 02".into());
        d.expiry = " 60 ".into();
        d.utf8 = true;
        d.user = vec![("a".into(), "b".into())];
        let o = d.to_outgoing(1).expect("valid");
        assert_eq!(o.props.message_expiry, Some(60));
        assert_eq!(o.props.correlation_data.as_deref(), Some(&[1u8, 2][..]));
        assert_eq!(o.props.user, vec![("a".to_string(), "b".to_string())]);
        assert!(o.props.payload_utf8);
        d.expiry = "-1".into();
        assert!(d.to_outgoing(1).is_err());
        d.expiry.clear();
        d.correlation = Some("0".into());
        assert!(d.to_outgoing(1).is_err());
    }

    #[test]
    fn empty_properties_keep_their_presence() {
        let props = MsgProps {
            content_type: Some(String::new()),
            correlation_data: Some(Bytes::new()),
            user: vec![
                (String::new(), String::new()),
                ("k".into(), String::new()),
                (String::new(), String::new()),
            ],
            ..MsgProps::default()
        };
        let m = msg(b"x", props.clone());
        // Edit: the draft has the presence, and the message that it makes is the same.
        let d = Draft::from_message("t", &m);
        assert_eq!(d.content_type.as_deref(), Some(""));
        assert_eq!(d.correlation.as_deref(), Some(""));
        assert!(d.has_content());
        let o = d.to_outgoing(1).expect("valid");
        assert_eq!(o.props.content_type.as_deref(), Some(""));
        assert_eq!(o.props.correlation_data.as_deref(), Some(&[][..]));
        assert_eq!(o.props, props);
        // No property, no presence.
        let none = Draft::from_message("t", &msg(b"x", MsgProps::default()));
        assert!(none.content_type.is_none() && none.correlation.is_none());
        let o = none.to_outgoing(1).expect("valid");
        assert!(o.props.content_type.is_none() && o.props.correlation_data.is_none());
        // Reply keeps empty correlation data.
        let reply_props = MsgProps {
            response_topic: Some("r".into()),
            correlation_data: Some(Bytes::new()),
            ..MsgProps::default()
        };
        let r = Draft::reply(&msg(b"x", reply_props), Qos::AtMostOnce).expect("reply");
        assert_eq!(r.correlation.as_deref(), Some(""));
    }

    #[test]
    fn a_string_with_line_breaks_is_kept_until_the_field_is_edited() {
        let mut k = LineKeep::default();
        // Plain text passes through.
        assert_eq!(k.set("a/b"), "a/b");
        assert!(!k.active("a/b"));
        assert_eq!(k.value("a/b"), "a/b");
        // Line breaks show escaped. The exact string stays the value.
        let shown = k.set("a\nb\r\\c");
        assert_eq!(shown, "a\\nb\\r\\\\c");
        assert!(k.active(&shown));
        assert_eq!(k.value(&shown), "a\nb\r\\c");
        assert_eq!(
            line_breaks_note("topic"),
            "This topic has line breaks; they are kept."
        );
        // An edit makes the field text the value.
        assert!(!k.active("a\\nbX"));
        assert_eq!(k.value("a\\nbX"), "a\\nbX");
        // A new plain value forgets the old one.
        k.set("x");
        assert_eq!(k.value(&shown), shown);
        // The exact topic is what Send uses.
        let mut d = draft("", "p");
        let mut k = LineKeep::default();
        let shown = k.set("a\nb");
        d.topic = k.value(&shown);
        assert_eq!(d.to_outgoing(1).expect("valid").topic, "a\nb");
    }

    #[test]
    fn hidden_characters_are_shown_escaped_and_sent_unchanged() {
        let exact = "ctl/\u{202e}gnp\u{200d}x";
        let mut k = LineKeep::default();
        let shown = k.set(exact);
        assert_eq!(shown, "ctl/⟨U+202E⟩gnp⟨U+200D⟩x");
        assert_eq!(
            k.note("topic", &shown).as_deref(),
            Some(hidden_chars_note("topic").as_str())
        );
        // Send uses the exact topic.
        let mut d = draft("", "p");
        d.topic = k.value(&shown);
        assert_eq!(d.to_outgoing(1).expect("valid").topic, exact);
        // An edit replaces the exact string and the note goes.
        let edited = format!("{shown}!");
        assert_eq!(k.value(&edited), edited);
        assert_eq!(k.note("topic", &edited), None);
        // Line breaks and hidden characters together.
        let both = k.set("a\nb\u{202e}");
        assert_eq!(both, "a\\nb⟨U+202E⟩");
        assert_eq!(k.value(&both), "a\nb\u{202e}");
        assert_eq!(
            k.note("value", &both).as_deref(),
            Some("This value has line breaks and hidden characters; they are kept.")
        );
        // Plain text has no note.
        let plain = k.set("a/b");
        assert_eq!(k.note("topic", &plain), None);
    }

    #[test]
    fn format_and_mode_switch_save_the_unsent_draft() {
        let mut m = ComposerModel::default();
        m.edited();
        let original = draft("t", "{\"a\":1}");
        // Format: the old draft is saved, then the formatted one is loaded and stays unsent.
        let mut formatted = original.clone();
        formatted.payload = "{\n  \"a\": 1\n}".into();
        assert!(m.replace_with(original.clone(), true));
        assert!(m.unsent);
        assert_eq!(m.history.drafts().next().map(|e| &e.draft), Some(&original));
        // Text to Hex: the text draft is saved too.
        let mut hex = formatted.clone();
        hex.switch_mode(PayloadMode::Hex).expect("hex");
        assert!(m.replace_with(formatted.clone(), true));
        assert_eq!(m.history.drafts().count(), 2);
        assert_eq!(
            m.history.drafts().next().map(|e| &e.draft),
            Some(&formatted)
        );
        // Sent content is not an unsent draft: nothing is saved.
        m.sent(hex.clone());
        assert!(m.replace_with(hex, false));
        assert_eq!(m.history.drafts().count(), 2);
    }
}
