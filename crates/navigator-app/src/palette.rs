//! The action registry. One table lists every action of the app. The command palette shows it.
//! The key bindings run the same `Cmd` values. No gpui types here, so tests can run it.

use navigator_core::decode::{Endian, NumberKind};
use navigator_core::settings::ThemeMode;

use crate::composer::PayloadMode;
use crate::inspector::DecoderPick;
use navigator_core::Qos;

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
    /// Scrolls to the newest message and turns Follow on.
    JumpToNewest,
    RowHeight1,
    RowHeight2,
    ToggleDetails,
    /// Opens the details pane (a wide window) or sheet (a narrow one) for the selection.
    DetailsOpen,
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
    CopyTopic,
    PublishToTopic,
    EditAndPublish,
    Reply,
    ClearRetained,
    FocusPublishTopic,
    Send,
    PayloadMode(PayloadMode),
    /// Beautify (`true`) or Minify (`false`) the JSON payload.
    FormatJson(bool),
    /// Convert the payload to Base64 (`false`) or Base64URL (`true`) text.
    ConvertBase64(bool),
    NewMessage,
    ComposerProperties,
    ComposerHistory,
    CloseComposer,
    /// The subscription in "Show only" (see `Facts::sub_qos`).
    SubQos(Qos),
    SubOptions,
    SubRetry,
    SubRemove,
    Theme(ThemeMode),
    /// A colour theme preset, by id (see `crate::themes`).
    ThemePreset(&'static str),
    /// The RAM limit of the store, in MiB (one of `MEMORY_OPTIONS_MIB`).
    MemoryLimit(u32),
    ResetLayout,
    OpenLogFolder,
    CopyDiagnostics,
    /// Check for a new version now.
    CheckUpdates,
    /// Turn the check at start on or off.
    UpdateCheckAtStart(bool),
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
    /// A Disconnect is on its way. Connect waits until it is done.
    pub disconnecting: bool,
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
    /// The composer payload has bytes to convert to Base64 (not empty, valid Hex).
    pub can_convert: bool,
    pub theme: Option<ThemeMode>,
    /// The id of the colour theme preset now.
    pub preset: &'static str,
    /// The memory limit of the store now, in MiB.
    pub memory_mib: u32,
    /// The check for a new version at start is on.
    pub check_updates: bool,
    /// The QoS of the subscription in "Show only". `None`: no subscription is in Show only.
    pub sub_qos: Option<Qos>,
    /// The user removed the subscription in "Show only": its QoS and options are locked.
    pub sub_removing: bool,
    /// The broker rejected that removal. Remove can run again.
    pub sub_remove_failed: bool,
    /// The subscription in Show only was rejected.
    pub sub_failed: bool,
    /// The connection uses MQTT 5 (subscription options).
    pub v5: bool,
    pub composer_open: bool,
}

/// One row of the table.
pub struct Entry {
    pub id: &'static str,
    pub label: &'static str,
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
    cmd: Cmd,
    available: fn(&Facts) -> bool,
) -> Entry {
    Entry {
        id,
        label,
        cmd,
        available,
    }
}

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

fn preset_not(f: &Facts, id: &str) -> bool {
    f.preset != id
}

fn theme_not(mode: ThemeMode) -> impl Fn(&Facts) -> bool {
    move |f| f.theme != Some(mode)
}

/// The memory limits that the palette and the status bar offer, in MiB. Core clamps any value
/// to its own range, so a value outside it can only come from the settings file.
pub const MEMORY_OPTIONS_MIB: [u32; 6] = [64, 128, 256, 512, 1024, 2048];

/// "64 MiB", "1 GiB".
pub fn memory_text(mib: u32) -> String {
    if mib >= 1024 && mib.is_multiple_of(1024) {
        format!("{} GiB", mib / 1024)
    } else {
        format!("{mib} MiB")
    }
}

/// The row of the palette that matches the current state is marked.
pub fn is_current(e: &Entry, f: &Facts) -> bool {
    matches!(e.cmd, Cmd::MemoryLimit(m) if m == f.memory_mib)
}

/// Every action, in the order of the palette.
pub static COMMANDS: &[Entry] = &[
    e("connect", "Connect", Cmd::Connect, |f| {
        f.has_profile && !f.connected && !f.connecting && !f.disconnecting
    }),
    e("disconnect", "Disconnect", Cmd::Disconnect, |f| f.connected),
    e(
        "cancel-connect",
        "Cancel connecting",
        Cmd::CancelConnect,
        |f| f.connecting && !f.connected,
    ),
    e(
        "manage-connections",
        "Manage connections…",
        Cmd::ManageConnections,
        always,
    ),
    e(
        "new-connection",
        "New connection…",
        Cmd::NewConnection,
        always,
    ),
    e(
        "focus-subscribe",
        "Focus the subscribe input",
        Cmd::FocusSubscribe,
        always,
    ),
    e("view-messages", "Messages view", Cmd::ViewMessages, |f| {
        !f.messages_view
    }),
    e("view-tree", "Tree view", Cmd::ViewTree, |f| f.messages_view),
    e(
        "direction-all",
        "Show all messages",
        Cmd::DirectionAll,
        messages,
    ),
    e(
        "direction-received",
        "Show received messages only",
        Cmd::DirectionReceived,
        messages,
    ),
    e(
        "direction-sent",
        "Show sent messages only",
        Cmd::DirectionSent,
        messages,
    ),
    e("clear-filters", "Clear filters", Cmd::ClearFilters, |f| {
        f.filters_active
    }),
    e("focus-filter", "Focus the filter", Cmd::FocusFilter, always),
    e("pause", "Pause", Cmd::Pause, |f| !f.paused),
    e("resume", "Resume", Cmd::Resume, |f| f.paused),
    e("jump-to-newest", "Jump to newest", Cmd::JumpToNewest, |f| {
        f.messages_view && !f.follow && !f.paused
    }),
    e("row-height-1", "Row height: 1 line", Cmd::RowHeight1, |f| {
        f.messages_view && f.two_lines
    }),
    e(
        "row-height-2",
        "Row height: 2 lines",
        Cmd::RowHeight2,
        |f| f.messages_view && !f.two_lines,
    ),
    e(
        "toggle-details",
        "Show or hide the details",
        Cmd::ToggleDetails,
        always,
    ),
    e("details-open", "Details: Open", Cmd::DetailsOpen, |f| {
        !f.details_open
    }),
    e(
        "toggle-expand",
        "Expand or restore the details",
        Cmd::ToggleExpand,
        |f| f.details_open,
    ),
    e(
        "decoder-auto",
        "Decoder: Auto",
        Cmd::Decoder(DecoderPick::Auto),
        value,
    ),
    e(
        "decoder-text",
        "Decoder: Text",
        Cmd::Decoder(DecoderPick::Text),
        value,
    ),
    e(
        "decoder-json",
        "Decoder: JSON",
        Cmd::Decoder(DecoderPick::Json),
        value,
    ),
    e(
        "decoder-hex",
        "Decoder: Hex",
        Cmd::Decoder(DecoderPick::Hex),
        value,
    ),
    e(
        "decoder-base64",
        "Decoder: Base64",
        Cmd::Decoder(DecoderPick::Base64),
        value,
    ),
    e(
        "decoder-base64-url",
        "Decoder: Base64URL",
        Cmd::Decoder(DecoderPick::Base64Url),
        value,
    ),
    e(
        "decoder-base64-to-hex",
        "Decoder: Base64 → Hex",
        Cmd::Decoder(DecoderPick::Base64ToHex),
        value,
    ),
    e(
        "decoder-base64-url-to-hex",
        "Decoder: Base64URL → Hex",
        Cmd::Decoder(DecoderPick::Base64UrlToHex),
        value,
    ),
    e(
        "decoder-number",
        "Decoder: Number",
        Cmd::Decoder(DecoderPick::Number),
        value,
    ),
    e(
        "number-int8",
        "Number type: int8",
        Cmd::NumberKind(NumberKind::I8),
        |f| kind_not(f, NumberKind::I8),
    ),
    e(
        "number-uint8",
        "Number type: uint8",
        Cmd::NumberKind(NumberKind::U8),
        |f| kind_not(f, NumberKind::U8),
    ),
    e(
        "number-int16",
        "Number type: int16",
        Cmd::NumberKind(NumberKind::I16),
        |f| kind_not(f, NumberKind::I16),
    ),
    e(
        "number-uint16",
        "Number type: uint16",
        Cmd::NumberKind(NumberKind::U16),
        |f| kind_not(f, NumberKind::U16),
    ),
    e(
        "number-int32",
        "Number type: int32",
        Cmd::NumberKind(NumberKind::I32),
        |f| kind_not(f, NumberKind::I32),
    ),
    e(
        "number-uint32",
        "Number type: uint32",
        Cmd::NumberKind(NumberKind::U32),
        |f| kind_not(f, NumberKind::U32),
    ),
    e(
        "number-int64",
        "Number type: int64",
        Cmd::NumberKind(NumberKind::I64),
        |f| kind_not(f, NumberKind::I64),
    ),
    e(
        "number-uint64",
        "Number type: uint64",
        Cmd::NumberKind(NumberKind::U64),
        |f| kind_not(f, NumberKind::U64),
    ),
    e(
        "number-float32",
        "Number type: float32",
        Cmd::NumberKind(NumberKind::F32),
        |f| kind_not(f, NumberKind::F32),
    ),
    e(
        "number-float64",
        "Number type: float64",
        Cmd::NumberKind(NumberKind::F64),
        |f| kind_not(f, NumberKind::F64),
    ),
    e(
        "number-little-endian",
        "Number byte order: little endian",
        Cmd::NumberEndian(Endian::Little),
        |f| f.number.is_some_and(|(_, e)| e != Endian::Little),
    ),
    e(
        "number-big-endian",
        "Number byte order: big endian",
        Cmd::NumberEndian(Endian::Big),
        |f| f.number.is_some_and(|(_, e)| e != Endian::Big),
    ),
    e("wrap-on", "Wrap lines: on", Cmd::WrapOn, |f| !f.wrap),
    e("wrap-off", "Wrap lines: off", Cmd::WrapOff, |f| f.wrap),
    e("show-all", "Show all of the payload", Cmd::ShowAll, |f| {
        f.can_show_all
    }),
    e("show-more", "Show more of the text", Cmd::ShowMore, |f| {
        f.can_show_more
    }),
    e("copy-value", "Copy the value", Cmd::CopyValue, |f| {
        f.can_copy
    }),
    e("copy-topic", "Copy the topic", Cmd::CopyTopic, |f| {
        f.can_publish_to_topic
    }),
    e(
        "publish-to-topic",
        "Publish to this topic",
        Cmd::PublishToTopic,
        |f| f.can_publish_to_topic,
    ),
    e(
        "edit-and-publish",
        "Edit and publish",
        Cmd::EditAndPublish,
        |f| f.can_edit,
    ),
    e("reply", "Reply", Cmd::Reply, |f| f.has_reply),
    e(
        "clear-retained",
        "Clear the retained message…",
        Cmd::ClearRetained,
        |f| f.has_retained && f.connected,
    ),
    e(
        "focus-publish-topic",
        "Focus the publish topic",
        Cmd::FocusPublishTopic,
        always,
    ),
    e("send", "Publish", Cmd::Send, |f| f.connected),
    e(
        "payload-text",
        "Payload mode: Text",
        Cmd::PayloadMode(PayloadMode::Text),
        |f| f.payload_mode != PayloadMode::Text,
    ),
    e(
        "payload-hex",
        "Payload mode: Hex",
        Cmd::PayloadMode(PayloadMode::Hex),
        |f| f.payload_mode != PayloadMode::Hex,
    ),
    e(
        "payload-json",
        "Payload mode: JSON",
        Cmd::PayloadMode(PayloadMode::Json),
        |f| f.payload_mode != PayloadMode::Json,
    ),
    e(
        "beautify-json",
        "Beautify the JSON payload",
        Cmd::FormatJson(true),
        |f| f.payload_mode.is_text(),
    ),
    e(
        "minify-json",
        "Minify the JSON payload",
        Cmd::FormatJson(false),
        |f| f.payload_mode.is_text(),
    ),
    e(
        "convert-base64",
        "Convert to Base64",
        Cmd::ConvertBase64(false),
        |f| f.composer_open && f.can_convert,
    ),
    e(
        "convert-base64url",
        "Convert to Base64URL",
        Cmd::ConvertBase64(true),
        |f| f.composer_open && f.can_convert,
    ),
    e("new-message", "New message", Cmd::NewMessage, always),
    e(
        "composer-properties",
        "Composer: Properties",
        Cmd::ComposerProperties,
        |f| f.v5,
    ),
    e(
        "composer-history",
        "Composer: History…",
        Cmd::ComposerHistory,
        always,
    ),
    e(
        "close-composer",
        "Close the composer",
        Cmd::CloseComposer,
        |f| f.composer_open,
    ),
    e(
        "sub-qos-0",
        "Subscription: QoS 0",
        Cmd::SubQos(Qos::AtMostOnce),
        always,
    ),
    e(
        "sub-qos-1",
        "Subscription: QoS 1",
        Cmd::SubQos(Qos::AtLeastOnce),
        always,
    ),
    e(
        "sub-options",
        "Subscription: Options…",
        Cmd::SubOptions,
        |f| f.v5,
    ),
    e("sub-retry", "Subscription: Retry", Cmd::SubRetry, always),
    e("sub-remove", "Subscription: Remove", Cmd::SubRemove, always),
    e(
        "theme-system",
        "Appearance: System",
        Cmd::Theme(ThemeMode::System),
        |f| theme_not(ThemeMode::System)(f),
    ),
    e(
        "theme-light",
        "Appearance: Light",
        Cmd::Theme(ThemeMode::Light),
        |f| theme_not(ThemeMode::Light)(f),
    ),
    e(
        "theme-dark",
        "Appearance: Dark",
        Cmd::Theme(ThemeMode::Dark),
        |f| theme_not(ThemeMode::Dark)(f),
    ),
    e(
        "theme-default",
        "Theme: Default",
        Cmd::ThemePreset("default"),
        |f| preset_not(f, "default"),
    ),
    e(
        "theme-catppuccin",
        "Theme: Catppuccin",
        Cmd::ThemePreset("catppuccin"),
        |f| preset_not(f, "catppuccin"),
    ),
    e(
        "theme-solarized",
        "Theme: Solarized",
        Cmd::ThemePreset("solarized"),
        |f| preset_not(f, "solarized"),
    ),
    e(
        "theme-everforest",
        "Theme: Everforest",
        Cmd::ThemePreset("everforest"),
        |f| preset_not(f, "everforest"),
    ),
    e(
        "theme-flexoki",
        "Theme: Flexoki",
        Cmd::ThemePreset("flexoki"),
        |f| preset_not(f, "flexoki"),
    ),
    e(
        "theme-mellifluous",
        "Theme: Mellifluous",
        Cmd::ThemePreset("mellifluous"),
        |f| preset_not(f, "mellifluous"),
    ),
    e(
        "theme-macos-classic",
        "Theme: macOS Classic",
        Cmd::ThemePreset("macos-classic"),
        |f| preset_not(f, "macos-classic"),
    ),
    e(
        "theme-tokyo-night",
        "Theme: Tokyo Night",
        Cmd::ThemePreset("tokyo-night"),
        |f| preset_not(f, "tokyo-night"),
    ),
    e(
        "memory-64",
        "Memory limit: 64 MiB",
        Cmd::MemoryLimit(64),
        always,
    ),
    e(
        "memory-128",
        "Memory limit: 128 MiB (default)",
        Cmd::MemoryLimit(128),
        always,
    ),
    e(
        "memory-256",
        "Memory limit: 256 MiB",
        Cmd::MemoryLimit(256),
        always,
    ),
    e(
        "memory-512",
        "Memory limit: 512 MiB",
        Cmd::MemoryLimit(512),
        always,
    ),
    e(
        "memory-1024",
        "Memory limit: 1 GiB",
        Cmd::MemoryLimit(1024),
        always,
    ),
    e(
        "memory-2048",
        "Memory limit: 2 GiB",
        Cmd::MemoryLimit(2048),
        always,
    ),
    e("reset-layout", "Reset the layout", Cmd::ResetLayout, always),
    e(
        "open-log-folder",
        "Open the log folder",
        Cmd::OpenLogFolder,
        always,
    ),
    e(
        "copy-diagnostics",
        "Copy diagnostics",
        Cmd::CopyDiagnostics,
        always,
    ),
    e(
        "check-updates",
        "Check for updates…",
        Cmd::CheckUpdates,
        always,
    ),
    e(
        "update-check-off",
        "Turn off the update check at start",
        Cmd::UpdateCheckAtStart(false),
        |f| f.check_updates,
    ),
    e(
        "update-check-on",
        "Turn on the update check at start",
        Cmd::UpdateCheckAtStart(true),
        |f| !f.check_updates,
    ),
    e("quit", "Quit", Cmd::Quit, always),
];

/// The actions that the palette lists now, in table order.
pub fn available(f: &Facts) -> Vec<&'static Entry> {
    COMMANDS.iter().filter(|c| c.available(f)).collect()
}

/// The reason why a listed action cannot run now. The palette shows the row with this text.
pub fn disabled_reason(e: &Entry, f: &Facts) -> Option<&'static str> {
    const SELECT: &str = "Select a subscription first.";
    match (e.cmd, f.sub_qos) {
        (Cmd::SubQos(_) | Cmd::SubOptions | Cmd::SubRetry | Cmd::SubRemove, None) => Some(SELECT),
        (Cmd::SubRemove, Some(_)) if f.sub_removing && !f.sub_remove_failed => {
            Some("Wait for the broker to confirm the removal.")
        }
        (Cmd::SubQos(_) | Cmd::SubOptions | Cmd::SubRetry, Some(_)) if f.sub_removing => {
            Some("You removed this subscription.")
        }
        (Cmd::SubQos(q), Some(now)) if q == now => Some("The subscription has this QoS already."),
        (Cmd::SubRetry, Some(_)) if !f.sub_failed => Some("The subscription did not fail."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the gpui glob import brings its own `test` macro in the parent.
    use super::{
        COMMANDS, Cmd, Facts, MEMORY_OPTIONS_MIB, available, disabled_reason, is_current,
        memory_text,
    };
    use crate::composer::PayloadMode;
    use navigator_core::Qos;
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
        // Connect waits until a Disconnect is done (the window would ignore it).
        let leaving = Facts {
            disconnecting: true,
            ..idle
        };
        assert!(!ids(&leaving).contains(&"connect"));
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
        assert!(!v.contains(&"beautify-json"), "Beautify is off in Hex mode");
        assert!(!v.contains(&"minify-json"), "Minify is off in Hex mode");
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
    fn jump_to_newest_shows_only_when_follow_is_off_and_the_view_runs() {
        let base = Facts {
            messages_view: true,
            ..Facts::default()
        };
        // Follow is on: nothing to jump to. There is no "Follow on/off" any more.
        let on = ids(&Facts {
            follow: true,
            ..base
        });
        assert!(!on.contains(&"jump-to-newest"));
        assert!(!on.iter().any(|id| id.starts_with("follow-")));
        assert!(ids(&base).contains(&"jump-to-newest"));
        // A pause freezes the view, and the Tree view has no Follow.
        assert!(
            !ids(&Facts {
                paused: true,
                ..base
            })
            .contains(&"jump-to-newest")
        );
        assert!(!ids(&Facts::default()).contains(&"jump-to-newest"));
    }

    #[test]
    fn details_open_is_listed_while_the_details_are_closed() {
        let closed = Facts::default();
        assert!(ids(&closed).contains(&"details-open"));
        let open = Facts {
            details_open: true,
            ..closed
        };
        assert!(!ids(&open).contains(&"details-open"));
        assert!(ids(&open).contains(&"toggle-expand"));
    }

    #[test]
    fn the_composer_entries_open_it_and_close_shows_only_when_it_is_open() {
        let closed = Facts::default();
        let v = ids(&closed);
        assert!(v.contains(&"new-message") && v.contains(&"composer-history"));
        assert!(!v.contains(&"close-composer"), "nothing to close");
        assert!(!v.contains(&"composer-properties"), "MQTT 5 only");
        assert!(!v.contains(&"toggle-composer"), "collapse is gone");
        let open = Facts {
            composer_open: true,
            v5: true,
            ..closed
        };
        let v = ids(&open);
        assert!(v.contains(&"close-composer") && v.contains(&"composer-properties"));
        // Send stays listed for a connection: the window opens the composer first.
        assert!(
            ids(&Facts {
                connected: true,
                ..closed
            })
            .contains(&"send")
        );
    }

    #[test]
    fn subscription_actions_need_a_subscription_in_show_only() {
        let reason = |id: &str, f: &Facts| {
            let e = COMMANDS.iter().find(|c| c.id == id).expect("entry");
            disabled_reason(e, f)
        };
        let none = Facts::default();
        for id in ["sub-qos-0", "sub-qos-1", "sub-retry", "sub-remove"] {
            assert!(ids(&none).contains(&id), "{id} is listed");
            assert_eq!(
                reason(id, &none),
                Some("Select a subscription first."),
                "{id}"
            );
        }
        // Options are an MQTT 5 feature: with 3.1.1 the entry is not listed at all.
        assert!(!ids(&none).contains(&"sub-options"));
        let v5 = Facts { v5: true, ..none };
        assert!(ids(&v5).contains(&"sub-options"));
        assert_eq!(
            reason("sub-options", &v5),
            Some("Select a subscription first.")
        );
        let one = Facts {
            sub_qos: Some(Qos::AtLeastOnce),
            v5: true,
            ..none
        };
        assert_eq!(reason("sub-qos-0", &one), None);
        let removing = Facts {
            sub_removing: true,
            ..one
        };
        assert_eq!(
            reason("sub-qos-0", &removing),
            Some("You removed this subscription.")
        );
        assert_eq!(
            reason("sub-remove", &removing),
            Some("Wait for the broker to confirm the removal.")
        );
        // A rejected removal can run again.
        let rejected = Facts {
            sub_remove_failed: true,
            ..removing
        };
        assert_eq!(reason("sub-remove", &rejected), None);
        assert_eq!(
            reason("sub-qos-0", &rejected),
            Some("You removed this subscription.")
        );
        assert_eq!(
            reason("sub-qos-1", &one),
            Some("The subscription has this QoS already.")
        );
        assert_eq!(reason("sub-remove", &one), None);
        assert_eq!(reason("sub-options", &one), None);
        // Retry works for a rejected subscription only.
        assert_eq!(
            reason("sub-retry", &one),
            Some("The subscription did not fail.")
        );
        let failed = Facts {
            sub_failed: true,
            ..one
        };
        assert_eq!(reason("sub-retry", &failed), None);
    }

    #[test]
    fn every_theme_preset_has_one_palette_entry() {
        for p in crate::themes::PRESETS {
            let rows: Vec<_> = COMMANDS
                .iter()
                .filter(|c| c.cmd == Cmd::ThemePreset(p.id))
                .collect();
            assert_eq!(rows.len(), 1, "{}", p.id);
            assert_eq!(rows[0].label, format!("Theme: {}", p.label));
        }
        let presets = COMMANDS
            .iter()
            .filter(|c| matches!(c.cmd, Cmd::ThemePreset(_)))
            .count();
        assert_eq!(presets, crate::themes::PRESETS.len());
        // The preset in use is not offered again.
        let f = Facts {
            preset: "flexoki",
            ..Facts::default()
        };
        let v = ids(&f);
        assert!(!v.contains(&"theme-flexoki") && v.contains(&"theme-solarized"));
    }

    #[test]
    fn the_update_check_can_be_turned_off_and_on() {
        let on = ids(&Facts {
            check_updates: true,
            ..Facts::default()
        });
        assert!(on.contains(&"check-updates") && on.contains(&"update-check-off"));
        assert!(!on.contains(&"update-check-on"));
        let off = ids(&Facts::default());
        assert!(off.contains(&"check-updates") && off.contains(&"update-check-on"));
        assert!(!off.contains(&"update-check-off"));
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
        assert!(ids(&branch).contains(&"copy-topic"));
        assert!(!ids(&base).contains(&"copy-topic"));
    }

    #[test]
    fn memory_options_have_labels_and_the_current_one_is_marked() {
        use navigator_core::store::{BUDGET_DEFAULT, BUDGET_MAX, BUDGET_MIN};
        const MIB: usize = 1024 * 1024;
        let default_mib = u32::try_from(BUDGET_DEFAULT / MIB).expect("small");
        assert_eq!(memory_text(64), "64 MiB");
        assert_eq!(memory_text(1024), "1 GiB");
        assert_eq!(memory_text(2048), "2 GiB");
        assert_eq!(memory_text(1536), "1536 MiB");
        let rows: Vec<_> = COMMANDS
            .iter()
            .filter(|c| matches!(c.cmd, Cmd::MemoryLimit(_)))
            .collect();
        assert_eq!(rows.len(), MEMORY_OPTIONS_MIB.len());
        for (row, mib) in rows.iter().zip(MEMORY_OPTIONS_MIB) {
            assert_eq!(row.cmd, Cmd::MemoryLimit(mib));
            // Every option is inside the range that core accepts.
            assert!((BUDGET_MIN / MIB..=BUDGET_MAX / MIB).contains(&(mib as usize)));
            let default = if mib == default_mib { " (default)" } else { "" };
            assert_eq!(
                row.label,
                format!("Memory limit: {}{default}", memory_text(mib))
            );
        }
        // Exactly one row is marked, and it is the current one.
        let f = Facts {
            memory_mib: 256,
            ..Facts::default()
        };
        let marked: Vec<_> = rows.iter().filter(|r| is_current(r, &f)).collect();
        assert_eq!(marked.len(), 1);
        assert_eq!(marked[0].cmd, Cmd::MemoryLimit(256));
    }
}
