//! The action registry. One table lists every action of the app. The command palette shows it.
//! The key bindings run the same `Cmd` values. No gpui types here, so tests can run it.

use navigator_core::decode::{Endian, NumberKind};
use navigator_core::settings::ThemeMode;

use crate::composer::PayloadMode;
use crate::inspector::DecoderPick;

/// What an action does. The window runs it (`Navigator::run_cmd`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    Connect,
    Disconnect,
    CancelConnect,
    ManageConnections,
    NewConnection,
    FocusSubscribe,
    ViewMessages,
    ViewTree,
    DirectionAll,
    DirectionReceived,
    DirectionSent,
    ClearFilters,
    FocusFilter,
    Pause,
    Resume,
    FollowOn,
    FollowOff,
    RowHeight1,
    RowHeight2,
    ToggleDetails,
    ToggleExpand,
    Decoder(DecoderPick),
    /// The width of the Number decoder.
    NumberKind(NumberKind),
    NumberEndian(Endian),
    WrapOn,
    WrapOff,
    ShowAll,
    ShowMore,
    CopyValue,
    PublishToTopic,
    EditAndPublish,
    Reply,
    ClearRetained,
    FocusPublishTopic,
    Send,
    PayloadMode(PayloadMode),
    FormatJson,
    ToggleComposer,
    Theme(ThemeMode),
    ResetLayout,
    OpenLogFolder,
    CopyDiagnostics,
    Quit,
}

/// What the registry needs to know to decide which actions are available. The window fills it
/// from its state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Facts {
    pub has_profile: bool,
    pub connected: bool,
    /// A Connect is on its way, or the net thread tries again (Cancel is the action).
    pub connecting: bool,
    pub messages_view: bool,
    pub filters_active: bool,
    pub paused: bool,
    pub follow: bool,
    pub two_lines: bool,
    pub details_open: bool,
    /// The details pane shows a message or a topic value.
    pub has_value: bool,
    pub can_publish_to_topic: bool,
    pub can_edit: bool,
    pub has_reply: bool,
    pub has_retained: bool,
    /// The finished render is the one of the current selection.
    pub can_copy: bool,
    pub wrap: bool,
    /// The Number decoder is chosen, with this width and byte order.
    pub number: Option<(NumberKind, Endian)>,
    pub can_show_all: bool,
    pub can_show_more: bool,
    pub payload_mode: PayloadMode,
    pub theme: Option<ThemeMode>,
}

/// One row of the table.
pub struct Entry {
    pub id: &'static str,
    pub label: &'static str,
    /// Shown in the palette. `None`: the action has no key binding.
    pub shortcut: Option<&'static str>,
    pub cmd: Cmd,
    available: fn(&Facts) -> bool,
}

impl Entry {
    pub fn available(&self, f: &Facts) -> bool {
        (self.available)(f)
    }
}

const fn e(
    id: &'static str,
    label: &'static str,
    shortcut: Option<&'static str>,
    cmd: Cmd,
    available: fn(&Facts) -> bool,
) -> Entry {
    Entry {
        id,
        label,
        shortcut,
        cmd,
        available,
    }
}

/// The key that the app shows for Ctrl (Cmd on macOS).
pub const MOD: &str = if cfg!(target_os = "macos") {
    "Cmd"
} else {
    "Ctrl"
};

fn always(_: &Facts) -> bool {
    true
}

fn value(f: &Facts) -> bool {
    f.has_value
}

fn messages(f: &Facts) -> bool {
    f.messages_view
}

/// The Number decoder is chosen and `kind` is not the width now.
fn kind_not(f: &Facts, kind: NumberKind) -> bool {
    f.number.is_some_and(|(k, _)| k != kind)
}

fn theme_not(mode: ThemeMode) -> impl Fn(&Facts) -> bool {
    move |f| f.theme != Some(mode)
}

/// Every action, in the order of the palette.
pub static COMMANDS: &[Entry] = &[
    e("connect", "Connect", None, Cmd::Connect, |f| {
        f.has_profile && !f.connected && !f.connecting
    }),
    e("disconnect", "Disconnect", None, Cmd::Disconnect, |f| {
        f.connected
    }),
    e(
        "cancel-connect",
        "Cancel connecting",
        None,
        Cmd::CancelConnect,
        |f| f.connecting && !f.connected,
    ),
    e(
        "manage-connections",
        "Manage connections",
        None,
        Cmd::ManageConnections,
        always,
    ),
    e(
        "new-connection",
        "New connection",
        Some("Mod+N"),
        Cmd::NewConnection,
        always,
    ),
    e(
        "focus-subscribe",
        "Focus the subscribe input",
        None,
        Cmd::FocusSubscribe,
        always,
    ),
    e(
        "view-messages",
        "Messages view",
        Some("Mod+1"),
        Cmd::ViewMessages,
        |f| !f.messages_view,
    ),
    e(
        "view-tree",
        "Tree view",
        Some("Mod+2"),
        Cmd::ViewTree,
        |f| f.messages_view,
    ),
    e(
        "direction-all",
        "Show all messages",
        None,
        Cmd::DirectionAll,
        messages,
    ),
    e(
        "direction-received",
        "Show received messages only",
        None,
        Cmd::DirectionReceived,
        messages,
    ),
    e(
        "direction-sent",
        "Show sent messages only",
        None,
        Cmd::DirectionSent,
        messages,
    ),
    e(
        "clear-filters",
        "Clear filters",
        None,
        Cmd::ClearFilters,
        |f| f.filters_active,
    ),
    e(
        "focus-filter",
        "Focus the filter",
        Some("Mod+F"),
        Cmd::FocusFilter,
        always,
    ),
    e("pause", "Pause", None, Cmd::Pause, |f| !f.paused),
    e("resume", "Resume", None, Cmd::Resume, |f| f.paused),
    e("follow-on", "Follow on", None, Cmd::FollowOn, |f| {
        f.messages_view && !f.follow
    }),
    e("follow-off", "Follow off", None, Cmd::FollowOff, |f| {
        f.messages_view && f.follow
    }),
    e(
        "row-height-1",
        "Row height: 1 line",
        None,
        Cmd::RowHeight1,
        |f| f.messages_view && f.two_lines,
    ),
    e(
        "row-height-2",
        "Row height: 2 lines",
        None,
        Cmd::RowHeight2,
        |f| f.messages_view && !f.two_lines,
    ),
    e(
        "toggle-details",
        "Show or hide the details",
        None,
        Cmd::ToggleDetails,
        always,
    ),
    e(
        "toggle-expand",
        "Expand or restore the details",
        None,
        Cmd::ToggleExpand,
        |f| f.details_open,
    ),
    e(
        "decoder-auto",
        "Decoder: Auto",
        None,
        Cmd::Decoder(DecoderPick::Auto),
        value,
    ),
    e(
        "decoder-text",
        "Decoder: Text",
        None,
        Cmd::Decoder(DecoderPick::Text),
        value,
    ),
    e(
        "decoder-json",
        "Decoder: JSON",
        None,
        Cmd::Decoder(DecoderPick::Json),
        value,
    ),
    e(
        "decoder-hex",
        "Decoder: Hex",
        None,
        Cmd::Decoder(DecoderPick::Hex),
        value,
    ),
    e(
        "decoder-base64",
        "Decoder: Base64",
        None,
        Cmd::Decoder(DecoderPick::Base64),
        value,
    ),
    e(
        "decoder-number",
        "Decoder: Number",
        None,
        Cmd::Decoder(DecoderPick::Number),
        value,
    ),
    e(
        "number-int8",
        "Number type: int8",
        None,
        Cmd::NumberKind(NumberKind::I8),
        |f| kind_not(f, NumberKind::I8),
    ),
    e(
        "number-uint8",
        "Number type: uint8",
        None,
        Cmd::NumberKind(NumberKind::U8),
        |f| kind_not(f, NumberKind::U8),
    ),
    e(
        "number-int16",
        "Number type: int16",
        None,
        Cmd::NumberKind(NumberKind::I16),
        |f| kind_not(f, NumberKind::I16),
    ),
    e(
        "number-uint16",
        "Number type: uint16",
        None,
        Cmd::NumberKind(NumberKind::U16),
        |f| kind_not(f, NumberKind::U16),
    ),
    e(
        "number-int32",
        "Number type: int32",
        None,
        Cmd::NumberKind(NumberKind::I32),
        |f| kind_not(f, NumberKind::I32),
    ),
    e(
        "number-uint32",
        "Number type: uint32",
        None,
        Cmd::NumberKind(NumberKind::U32),
        |f| kind_not(f, NumberKind::U32),
    ),
    e(
        "number-int64",
        "Number type: int64",
        None,
        Cmd::NumberKind(NumberKind::I64),
        |f| kind_not(f, NumberKind::I64),
    ),
    e(
        "number-uint64",
        "Number type: uint64",
        None,
        Cmd::NumberKind(NumberKind::U64),
        |f| kind_not(f, NumberKind::U64),
    ),
    e(
        "number-float32",
        "Number type: float32",
        None,
        Cmd::NumberKind(NumberKind::F32),
        |f| kind_not(f, NumberKind::F32),
    ),
    e(
        "number-float64",
        "Number type: float64",
        None,
        Cmd::NumberKind(NumberKind::F64),
        |f| kind_not(f, NumberKind::F64),
    ),
    e(
        "number-little-endian",
        "Number byte order: little endian",
        None,
        Cmd::NumberEndian(Endian::Little),
        |f| f.number.is_some_and(|(_, e)| e != Endian::Little),
    ),
    e(
        "number-big-endian",
        "Number byte order: big endian",
        None,
        Cmd::NumberEndian(Endian::Big),
        |f| f.number.is_some_and(|(_, e)| e != Endian::Big),
    ),
    e("wrap-on", "Wrap lines: on", None, Cmd::WrapOn, |f| !f.wrap),
    e("wrap-off", "Wrap lines: off", None, Cmd::WrapOff, |f| {
        f.wrap
    }),
    e(
        "show-all",
        "Show all of the payload",
        None,
        Cmd::ShowAll,
        |f| f.can_show_all,
    ),
    e(
        "show-more",
        "Show more of the text",
        None,
        Cmd::ShowMore,
        |f| f.can_show_more,
    ),
    e("copy-value", "Copy the value", None, Cmd::CopyValue, |f| {
        f.can_copy
    }),
    e(
        "publish-to-topic",
        "Publish to this topic",
        None,
        Cmd::PublishToTopic,
        |f| f.can_publish_to_topic,
    ),
    e(
        "edit-and-publish",
        "Edit and publish",
        None,
        Cmd::EditAndPublish,
        |f| f.can_edit,
    ),
    e("reply", "Reply", None, Cmd::Reply, |f| f.has_reply),
    e(
        "clear-retained",
        "Clear the retained message",
        None,
        Cmd::ClearRetained,
        |f| f.has_retained && f.connected,
    ),
    e(
        "focus-publish-topic",
        "Focus the publish topic",
        None,
        Cmd::FocusPublishTopic,
        always,
    ),
    e("send", "Send", Some("Mod+Enter"), Cmd::Send, |f| {
        f.connected
    }),
    e(
        "payload-text",
        "Payload mode: Text",
        None,
        Cmd::PayloadMode(PayloadMode::Text),
        |f| f.payload_mode == PayloadMode::Hex,
    ),
    e(
        "payload-hex",
        "Payload mode: Hex",
        None,
        Cmd::PayloadMode(PayloadMode::Hex),
        |f| f.payload_mode == PayloadMode::Text,
    ),
    e(
        "format-json",
        "Format the payload as JSON",
        None,
        Cmd::FormatJson,
        |f| f.payload_mode == PayloadMode::Text,
    ),
    e(
        "toggle-composer",
        "Collapse or expand the composer",
        None,
        Cmd::ToggleComposer,
        always,
    ),
    e(
        "theme-system",
        "Appearance: System",
        None,
        Cmd::Theme(ThemeMode::System),
        |f| theme_not(ThemeMode::System)(f),
    ),
    e(
        "theme-light",
        "Appearance: Light",
        None,
        Cmd::Theme(ThemeMode::Light),
        |f| theme_not(ThemeMode::Light)(f),
    ),
    e(
        "theme-dark",
        "Appearance: Dark",
        None,
        Cmd::Theme(ThemeMode::Dark),
        |f| theme_not(ThemeMode::Dark)(f),
    ),
    e(
        "reset-layout",
        "Reset the layout",
        None,
        Cmd::ResetLayout,
        always,
    ),
    e(
        "open-log-folder",
        "Open the log folder",
        None,
        Cmd::OpenLogFolder,
        always,
    ),
    e(
        "copy-diagnostics",
        "Copy diagnostics",
        None,
        Cmd::CopyDiagnostics,
        always,
    ),
    e("quit", "Quit", Some("Mod+Q"), Cmd::Quit, always),
];

/// The actions that the palette lists now, in table order.
pub fn available(f: &Facts) -> Vec<&'static Entry> {
    COMMANDS.iter().filter(|c| c.available(f)).collect()
}

/// The shortcut text for the current platform: `Mod+K` shows as `Ctrl+K` (`Cmd+K` on macOS).
pub fn shortcut_text(e: &Entry) -> Option<String> {
    e.shortcut.map(|s| s.replace("Mod", MOD))
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the gpui glob import brings its own `test` macro in the parent.
    use super::{COMMANDS, Cmd, Facts, available, shortcut_text};
    use crate::composer::PayloadMode;
    use navigator_core::decode::{Endian, NumberKind};
    use std::collections::HashSet;

    fn ids(f: &Facts) -> Vec<&'static str> {
        available(f).into_iter().map(|e| e.id).collect()
    }

    #[test]
    fn ids_and_labels_are_unique() {
        let ids: HashSet<_> = COMMANDS.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), COMMANDS.len());
        let labels: HashSet<_> = COMMANDS.iter().map(|c| c.label).collect();
        assert_eq!(labels.len(), COMMANDS.len());
        // One command value for each row: the key bindings find their row by `Cmd`.
        for c in COMMANDS {
            assert_eq!(
                COMMANDS.iter().filter(|o| o.cmd == c.cmd).count(),
                1,
                "{}",
                c.id
            );
        }
    }

    #[test]
    fn connection_actions_follow_the_state() {
        let idle = Facts {
            has_profile: true,
            messages_view: true,
            ..Facts::default()
        };
        let v = ids(&idle);
        assert!(v.contains(&"connect") && !v.contains(&"disconnect") && !v.contains(&"send"));
        let connecting = Facts {
            connecting: true,
            ..idle
        };
        let v = ids(&connecting);
        assert!(v.contains(&"cancel-connect") && !v.contains(&"connect"));
        let up = Facts {
            connected: true,
            ..idle
        };
        let v = ids(&up);
        assert!(v.contains(&"disconnect") && v.contains(&"send") && !v.contains(&"connect"));
        // No profile: there is nothing to connect.
        assert!(!ids(&Facts::default()).contains(&"connect"));
    }

    #[test]
    fn message_actions_need_their_facts() {
        let none = ids(&Facts::default());
        for id in [
            "copy-value",
            "decoder-number",
            "wrap-off",
            "show-all",
            "show-more",
            "decoder-json",
            "publish-to-topic",
            "edit-and-publish",
            "reply",
            "clear-retained",
            "clear-filters",
        ] {
            assert!(!none.contains(&id), "{id}");
        }
        let f = Facts {
            has_value: true,
            can_copy: true,
            can_edit: true,
            has_reply: true,
            has_retained: true,
            connected: true,
            payload_mode: PayloadMode::Hex,
            ..Facts::default()
        };
        let v = ids(&f);
        for id in [
            "copy-value",
            "edit-and-publish",
            "reply",
            "clear-retained",
            "payload-text",
        ] {
            assert!(v.contains(&id), "{id}");
        }
        assert!(!v.contains(&"format-json"), "Format is off in Hex mode");
        // Direction filters belong to the Messages view.
        assert!(!v.contains(&"direction-sent"));
        assert!(
            ids(&Facts {
                messages_view: true,
                ..f
            })
            .contains(&"direction-sent")
        );
    }

    #[test]
    fn shortcuts_name_the_platform_key() {
        let send = COMMANDS.iter().find(|c| c.cmd == Cmd::Send).expect("send");
        let text = shortcut_text(send).expect("a shortcut");
        assert!(text.ends_with("+Enter") && !text.contains("Mod"));
    }

    #[test]
    fn decoder_view_actions_follow_the_state() {
        let base = Facts {
            has_value: true,
            wrap: true,
            ..Facts::default()
        };
        let v = ids(&base);
        // Number kinds and byte order need the Number decoder.
        assert!(
            v.contains(&"decoder-number") && v.contains(&"wrap-off") && !v.contains(&"wrap-on")
        );
        assert!(!v.iter().any(|id| id.starts_with("number-")));
        let n = Facts {
            number: Some((NumberKind::I32, Endian::Little)),
            ..base
        };
        let v = ids(&n);
        assert!(!v.contains(&"number-int32") && v.contains(&"number-uint32"));
        assert!(!v.contains(&"number-little-endian") && v.contains(&"number-big-endian"));
        assert_eq!(
            v.iter()
                .filter(|id| id.starts_with("number-") && id.ends_with("t8"))
                .count(),
            2
        );
        // Copy, Show all and Show more need their facts: a render that is not ready gives none.
        let v = ids(&base);
        assert!(
            !v.contains(&"copy-value") && !v.contains(&"show-all") && !v.contains(&"show-more")
        );
        let v = ids(&Facts {
            can_copy: true,
            can_show_all: true,
            can_show_more: true,
            ..base
        });
        assert!(v.contains(&"copy-value") && v.contains(&"show-all") && v.contains(&"show-more"));
        // A topic without a message (a branch) can still get "Publish to this topic".
        let branch = Facts {
            can_publish_to_topic: true,
            ..Facts::default()
        };
        assert!(ids(&branch).contains(&"publish-to-topic"));
    }
}
