//! Main window. P3: title bar with filter, subscriptions, message list and topic tree, details,
//! composer, status bar.

mod commands;
mod compose;
mod details;
mod profiles;
use profiles::{choose_text, display_name, host_port};
mod update;

use std::cell::OnceCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::component::WindowExt;
use gpui_kit::component::button::{Button, ButtonVariants, DropdownButton, Toggle, ToggleGroup};
use gpui_kit::component::command::{Command as PaletteList, CommandItem, CommandState};
use gpui_kit::component::empty::{Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyTitle};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::DropdownMenu;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tab::TabBar;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, Theme, TitleBar, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use navigator_core::Qos;
use navigator_core::decode::preview;
use navigator_core::net::{
    Command, ConnState, ConnectConfig, ConnectionDetails, Event, Net, NetLimits, Profile, Protocol,
    RetainHandling, SendError, Status, SubOptions,
};
use navigator_core::settings::{Load, RowHeight, Settings, ThemeMode, ViewMode};
use navigator_core::store::{
    Direction, DirectionFilter, Insert, Limits, ListFilter, Msg, NodeId, RowId, RowView,
    SendStatus, TopicStore, TreeFilter,
};
use navigator_core::text::display_safe;
use uuid::Uuid;

use crate::connections::{ConnectionSheet, SheetEvent, protocol_text};
use crate::messages::{
    colour_slot, details_lines, format_time, granted_text, group_digits, options_text, row_result,
    status_failed, status_from_result, status_text,
};
use crate::palette::Cmd;
use crate::persist::Writer;
use crate::secrets::{Cached, Secrets};

actions!(
    navigator_ui,
    [
        ShowMessages,
        ShowTree,
        FocusFilter,
        OpenPalette,
        NewConnection,
        NewMessage,
        CloseComposer,
        CloseDetails,
        OpenDetails,
        SendMessage,
        FocusNextField,
        FocusPrevField,
        SelectPrev,
        SelectNext,
        ConfirmRow,
        ExpandNode,
        CollapseNode
    ]
);

/// Design pixels as `Rems`. The design is drawn at the default 16 px rem. Layout uses this
/// instead of `px(..)`, so that it follows the zoom (the theme `font_size`).
pub(crate) const fn ui(px: f32) -> Rems {
    rems(px / 16.0)
}

const CONTEXT: &str = "Navigator";
const LIST_CONTEXT: &str = "NavigatorList";
const HIST_CONTEXT: &str = "NavigatorHistory";

/// A key binding. `secondary-` is the main key of the platform: Cmd on macOS, Ctrl on Windows
/// and Linux. (`KeyBinding::new` panics on `secondary-`.)
fn bind<A: Action>(keys: &str, action: A, context: Option<&str>, cx: &App) -> KeyBinding {
    let context = context
        .map(|c| KeyBindingContextPredicate::parse(c).expect("a key context"))
        .map(Rc::new);
    KeyBinding::load(keys, Box::new(action), context, false, None, cx).expect("a key binding")
}

/// The shortcut of an action as the key bindings of this platform give it: `Ctrl+K`, or `⌘K` on
/// macOS. `context` is the key context of the binding (`None`: a binding without a context).
/// It gives `None` when the action has no binding.
pub(crate) fn shortcut_text(
    action: &dyn Action,
    context: Option<&str>,
    window: &Window,
) -> Option<String> {
    let context = match context {
        Some(c) => KeyContext::parse(c).ok()?,
        None => KeyContext::default(),
    };
    let binding = window.highest_precedence_binding_for_action_in_context(action, context)?;
    Some(Kbd::format(binding.keystrokes().first()?.as_keystroke()))
}

/// Registers the key bindings of this window. Call once at start.
pub fn init(cx: &mut App) {
    let bindings = [
        bind("secondary-1", ShowMessages, Some(CONTEXT), cx),
        bind("secondary-2", ShowTree, Some(CONTEXT), cx),
        bind("secondary-f", FocusFilter, Some(CONTEXT), cx),
        bind("secondary-k", OpenPalette, Some(CONTEXT), cx),
        bind("secondary-n", NewConnection, Some(CONTEXT), cx),
        bind("secondary-shift-n", NewMessage, Some(CONTEXT), cx),
        // Publish works from any control of the composer. The payload editor takes this key for
        // a newline, so it has its own, deeper binding below.
        bind("secondary-enter", SendMessage, Some("Composer"), cx),
        bind("secondary-enter", SendMessage, Some("Composer > Input"), cx),
        bind("secondary-q", crate::Quit, None, cx),
    ];
    cx.bind_keys(bindings);
    cx.bind_keys([
        // `Esc` closes the area that has the focus. A control that uses `Esc` (a menu, the
        // filter) handles it first and does not pass it on (design 8).
        KeyBinding::new("escape", CloseComposer, Some("Composer")),
        KeyBinding::new("escape", CloseDetails, Some("DetailsPane")),
        // In the History list of the composer, Enter must reach the focused entry. Without this
        // the dialog takes it as "confirm" and closes before the entry recalls.
        KeyBinding::new("enter", NoAction, Some("HistoryList")),
        // The jump button is inside the list area, whose Enter opens the details. Let the button
        // take its own Enter.
        KeyBinding::new("enter", NoAction, Some("JumpStrip")),
        // In a narrow window, Enter opens the details sheet of the selected row.
        KeyBinding::new("enter", OpenDetails, Some(LIST_CONTEXT)),
        // The payload editor inserts a tab for Tab. In the composer, Tab moves the focus.
        KeyBinding::new("tab", FocusNextField, Some("Composer > Input")),
        KeyBinding::new("shift-tab", FocusPrevField, Some("Composer > Input")),
        KeyBinding::new("up", SelectPrev, Some(LIST_CONTEXT)),
        KeyBinding::new("down", SelectNext, Some(LIST_CONTEXT)),
        KeyBinding::new("right", ExpandNode, Some(LIST_CONTEXT)),
        KeyBinding::new("left", CollapseNode, Some(LIST_CONTEXT)),
        KeyBinding::new("up", SelectPrev, Some(HIST_CONTEXT)),
        KeyBinding::new("down", SelectNext, Some(HIST_CONTEXT)),
        KeyBinding::new("enter", ConfirmRow, Some(HIST_CONTEXT)),
    ]);
}

/// The two views of the centre pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Messages,
    Tree,
}

/// What the details pane shows. It does not change when the user switches views.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetailOf {
    Nothing,
    /// The latest value of `sel_topic` (the user selected a topic).
    Topic,
    /// `sel_msg` (the user selected a message row).
    Message,
}

/// The selected message. The view keeps its own copy, so the details stay when the store
/// removes the row.
struct SelMsg {
    id: RowId,
    node: Option<NodeId>,
    topic: Arc<str>,
    msg: Msg,
    direction: Direction,
    /// Identity of the message (`RowView::Message::seq`). 0: a sent message.
    seq: u64,
}

/// The top visible row of the message list while Follow is off.
struct Anchor {
    id: RowId,
    /// The row after it (older). It replaces the anchor when the store removes the anchor.
    next: Option<RowId>,
    /// Pixels of the row that are above the viewport.
    px: f32,
}

/// The row at the top of the viewport and the pixels of it that are above the viewport.
fn top_row(scroll_top: f32, row_h: f32) -> (usize, f32) {
    let i = (scroll_top / row_h).floor();
    (i as usize, scroll_top - i * row_h)
}

/// The scroll position that puts row `index` at `px` above the viewport.
fn scroll_top_of(index: usize, px: f32, row_h: f32) -> f32 {
    index as f32 * row_h + px
}

/// Identifies one formatted payload in the details pane.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DetailKey {
    /// Latest value of a node: node, identity of the value (`NodeView::latest_seq`).
    Node(NodeId, u64),
    Row(RowId),
    /// A History entry of a node: node, identity of the entry (`TopicStore::history_seq`).
    Hist(NodeId, u64),
}

/// One visible row of the message list, copied out of the store for rendering.
enum RowData {
    Msg {
        id: RowId,
        node: Option<NodeId>,
        topic: SharedString,
        time: SystemTime,
        retain: bool,
        preview: SharedString,
        sent: Option<SendStatus>,
        colour: Option<usize>,
    },
    Reconnected(RowId, SystemTime),
    Removed(RowId),
    RemovedMarker,
}

use crate::ingest;

/// Least time between two frames, so that many messages make one render.
#[cfg(not(windows))]
const COALESCE: Duration = Duration::from_millis(33);
/// Windows rounds a timer up to its tick of 15.625 ms. A wait of 33 ms ends at the third tick
/// (46.9 ms, 21 frames in a second). A wait of 25 ms ends at the second tick (31.25 ms, 32 frames
/// in a second), also when the loop wakes up to 6 ms late. A finer timer clock gives a few more
/// frames, and the merge stays within 4 ms of each 25 ms.
#[cfg(windows)]
const COALESCE: Duration = Duration::from_millis(25);

/// Who asks for an allocation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Caller {
    /// The frame loop in normal mode.
    Loop,
    /// The frame loop in backlog mode: the fallback for stalled frames.
    Fallback,
    /// The next-frame callback of `render` (backlog mode).
    Frame,
}

/// No frame callback for this long means that frames have stalled (a hidden window).
const FRAME_STALL: Duration = Duration::from_millis(100);

/// The one rule for the frame budget. A frame callback allocates only if no allocation started
/// since the previous callback. The loop in normal mode allocates `COALESCE` after the last
/// allocation. The fallback in backlog mode does the same, and only if the last callback is older
/// than `FRAME_STALL`. So while frames come, backlog mode has at most one allocation in each
/// display interval. `last_frame` is the time of the previous callback.
fn may_allocate(
    caller: Caller,
    now: Instant,
    last_alloc: Option<Instant>,
    last_frame: Option<Instant>,
) -> bool {
    let coalesced = last_alloc.is_none_or(|a| now - a >= COALESCE);
    match caller {
        Caller::Frame => last_frame.is_none_or(|f| last_alloc.is_none_or(|a| a <= f)),
        Caller::Loop => coalesced,
        Caller::Fallback => coalesced && last_frame.is_none_or(|f| now - f >= FRAME_STALL),
    }
}

/// A merge of this many messages (about 7.7k a second at `COALESCE`) is a flood.
const FLOOD_MSGS: u64 = 256;
/// Under a flood the list redraws after this gap at the least (about 15 Hz). A redraw costs about
/// 4 ms of UI time, whatever the message rate. A merge costs far less, so the merge keeps its
/// pace and only the redraw skips.
const REDRAW_GAP: Duration = Duration::from_millis(COALESCE.as_millis() as u64 * 3 / 2);

/// True if the redraw of this merge can wait. Only the loop skips (backlog mode and input draw
/// as before), only in a flood, and only if the last redraw is younger than `REDRAW_GAP`. So the
/// first merge after a quiet time draws, and so does every second merge of a flood. A skipped
/// redraw starts a timer (`Navigator::redraw_later`), so the last messages of a flood show too.
fn skip_redraw(caller: Caller, msgs: u64, since_redraw: Option<Duration>) -> bool {
    caller == Caller::Loop && msgs >= FLOOD_MSGS && since_redraw.is_some_and(|d| d < REDRAW_GAP)
}

/// How long the next frame waits, `since_last` after the start of the last one. `None`: no wait.
/// The first message after a quiet time gets a frame at once, and a flood gets one frame in each
/// `COALESCE`. Nothing waits while the loop is idle.
fn coalesce_wait(since_last: Duration) -> Option<Duration> {
    COALESCE.checked_sub(since_last).filter(|d| !d.is_zero())
}
/// After the first frame, a window resize in this time comes from the window system.
// ponytail: a window manager resize later than this is saved as the user's size. Add a check
// for user input (a mouse press on the window border) if that shows up.
const RESIZE_SETTLE: Duration = Duration::from_secs(1);
/// Quiet time after the last resize event before the size goes to the settings writer.
const RESIZE_SAVE: Duration = Duration::from_millis(250);
/// Wait before a filter text change applies.
const DEBOUNCE: Duration = Duration::from_millis(150);
const MIB: usize = 1024 * 1024;
/// Row heights in design pixels (at a 16 px rem, see [`ui`]): a message row of 1 line and of 2
/// lines, a tree row, and a subscription row (the filter line and the state line).
const ROW_H1: f32 = 30.;
const ROW_H2: f32 = 44.;
const TREE_ROW_H: f32 = 28.;
const SUB_ROW_H: f32 = 48.;
/// Widths of the side panes in design pixels: default, least and most. The centre pane keeps at
/// least `CENTRE_MIN`.
const LEFT_W: (f32, f32, f32) = (260., 180., 420.);
const RIGHT_W: (f32, f32, f32) = (360., 280., 720.);
const CENTRE_MIN: f32 = 360.;

/// Where one subscription row is in its life.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SubState {
    /// SUBSCRIBE is queued or sent. The SUBACK has not come.
    Pending,
    Active(Qos),
    Failed(String),
    /// The UNSUBSCRIBE could not be queued. The subscription is still wanted.
    NotRemoved(String),
    /// The net thread took the UNSUBSCRIBE: the filter is out of its desired list. The row
    /// waits for the broker answer.
    Removing,
    /// The broker rejected the UNSUBSCRIBE. The filter is out of the desired list, so it is not
    /// sent again after a reconnect, but the broker can still send its messages until then.
    RemoveFailed(String),
}

impl SubState {
    /// The user removed this filter, and the net thread took the request.
    fn removal_accepted(&self) -> bool {
        matches!(self, Self::Removing | Self::RemoveFailed(_))
    }
}

struct Sub {
    /// Cheap to clone: the row, the index and the element ids share it.
    filter: SharedString,
    qos: Qos,
    /// MQTT 5 options. MQTT 3.1.1 does not send them.
    options: SubOptions,
    state: SubState,
    /// The request ID of the latest Subscribe that the net thread accepted for this row. A
    /// `Subscribed` result changes the row only if it has this ID. 0: the filter came from the
    /// profile, or the connection is new (the net thread subscribes with ID 0 then).
    req: u64,
    /// Rises with each push, so `rows` stays sorted by it. The index stores it, not a position.
    seq: u64,
}

/// The details of the latest connection, as the UI got them. The log can still hold the
/// details of an older attempt (same generation) when a newer attempt is already `Connected`.
/// So the popover shows the details only while their attempt is the one of the status.
#[derive(Default)]
struct DetailsCache {
    latest: Option<(u64, Box<ConnectionDetails>)>,
}

impl DetailsCache {
    /// The newest event wins: the log keeps the order.
    fn offer(&mut self, attempt: u64, details: Box<ConnectionDetails>) {
        self.latest = Some((attempt, details));
    }

    /// The status left `Connected`, or the UI started or cancelled a connection.
    fn clear(&mut self) {
        self.latest = None;
    }

    /// The details to show: the status must be `Connected`, and the attempt must match.
    fn current(&self, status: &Status) -> Option<&ConnectionDetails> {
        let (attempt, details) = self.latest.as_ref()?;
        (status.state == ConnState::Connected && *attempt == status.attempt).then_some(&**details)
    }
}

/// The answer to a change of subscription options (see `Subs::check_options_change`).
#[derive(Debug, PartialEq, Eq)]
enum OptionsChange {
    /// Nothing to do: no such row, the row is being removed, or the same options.
    Ignore,
    /// The options are not allowed. The text is for the user.
    Refused(&'static str),
    /// The row has the new options. Send a SUBSCRIBE with this QoS.
    Go(Qos),
}

/// Follow keeps the newest row in view. It has no switch: it is on when the list opens, and it
/// goes off when the user scrolls away or selects a row. "Jump to newest" turns it on again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Follow {
    on: bool,
}

impl Default for Follow {
    fn default() -> Self {
        Self { on: true }
    }
}

impl Follow {
    /// The user scrolled away or selected a row. Returns true when Follow went off now. While
    /// the view is paused it does not move, so Follow keeps the state from before the pause.
    fn leave(&mut self, paused: bool) -> bool {
        let was = self.on;
        if !paused {
            self.on = false;
        }
        was && !self.on
    }

    fn jump(&mut self) {
        self.on = true;
    }

    /// "Jump to newest" shows when Follow is off and the view is not paused.
    fn jump_visible(&self, paused: bool) -> bool {
        !self.on && !paused
    }
}

/// The list has left its top: the scroll offset (zero at the top, negative below) is past a
/// small tolerance. A wheel event at the top changes nothing, so it does not count.
fn left_the_top(offset_y: f32) -> bool {
    offset_y < -0.5
}

/// WCAG relative luminance of a colour.
fn luminance(c: Hsla) -> f32 {
    let c = Rgba::from(c);
    let lin = |v: f32| {
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// WCAG contrast ratio of two colours (1 to 21).
fn contrast(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// The one of two theme colours that reads better on `fill`. A theme can pair a fill with a
/// foreground that is too close to it (a warning fill with an orange text).
fn readable_on(fill: Hsla, a: Hsla, b: Hsla) -> Hsla {
    if contrast(fill, a) >= contrast(fill, b) {
        a
    } else {
        b
    }
}

/// The details open as a sheet when the window is narrow, or when the three panes at their least
/// widths do not fit it. `rem` is the rem size in pixels: the zoom scales the pane limits.
fn needs_sheet(width: f32, rem: f32) -> bool {
    let least = (LEFT_W.1 + CENTRE_MIN + RIGHT_W.1) * rem / 16.;
    width < details::NARROW || width < least
}

/// "1 topic", "3 topics": the number of topics below a tree node.
fn topics_label(n: u32) -> String {
    format!("{n} topic{}", if n == 1 { "" } else { "s" })
}

/// The text of the "Jump to newest" button. `new` counts the messages since Follow went off.
fn jump_label(new: u64) -> String {
    if new == 0 {
        "Jump to newest".into()
    } else {
        format!("Jump to newest ({})", group_digits(new))
    }
}

/// The hint of the filter input. It says what the filter searches in.
fn filter_placeholder(view: View) -> &'static str {
    match view {
        View::Messages => "Filter by topic or payload",
        View::Tree => "Filter by topic",
    }
}

/// The filter chips that show: "Show only" and a direction other than All (Messages view
/// only). The text filter shows in its own input.
fn chip_labels(show_only: Option<&str>, direction: DirectionFilter, view: View) -> Vec<String> {
    let mut chips = Vec::new();
    if let Some(f) = show_only {
        chips.push(format!("Only: {}", display_safe(f)));
    }
    if view == View::Messages {
        match direction {
            DirectionFilter::All => {}
            DirectionFilter::Received => chips.push("Received".into()),
            DirectionFilter::Sent => chips.push("Sent".into()),
        }
    }
    chips
}

/// Why a connected window has nothing to show, when the subscriptions are the reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubsEmpty {
    /// Every filter is gone (or none was ever added).
    NoSubscriptions,
    /// The only filter is `#`, and the broker rejected it.
    HashRejected,
}

fn subs_empty(subs: &Subs) -> Option<SubsEmpty> {
    let mut live = subs.rows.iter().filter(|s| !s.state.removal_accepted());
    let Some(first) = live.next() else {
        return Some(SubsEmpty::NoSubscriptions);
    };
    let rejected = |s: &Sub| matches!(s.state, SubState::Failed(_));
    // Another filter may still bring messages: only an all-rejected list with `#` says it.
    let all_rejected = rejected(first) && live.clone().all(rejected);
    let has_hash = &*first.filter == "#" || live.any(|s| &*s.filter == "#");
    (all_rejected && has_hash).then_some(SubsEmpty::HashRejected)
}

/// The answer to a change of subscription QoS (see `Subs::check_qos_change`).
#[derive(Debug, PartialEq, Eq)]
enum QosChange {
    /// No such row, the row is being removed, or the same QoS.
    Ignore,
    /// Send a SUBSCRIBE with the new QoS and these options.
    Go(SubOptions),
}

/// The status line of a row, and whether it is a problem. `None`: the row needs no line.
fn sub_status(state: &SubState, asked: Qos, connected: bool) -> Option<(String, bool)> {
    match state {
        SubState::Pending if connected => Some(("Subscribing…".into(), false)),
        SubState::Pending => Some(("Waiting for connection".into(), false)),
        // Only a grant below the request is worth a line.
        SubState::Active(q) if *q != asked => Some((granted_text(*q), false)),
        SubState::Active(_) => None,
        SubState::Failed(e) | SubState::RemoveFailed(e) | SubState::NotRemoved(e) => {
            Some((e.clone(), true))
        }
        SubState::Removing => Some(("Removing…".into(), false)),
    }
}

/// The subscription rows. Free of gpui types, so that tests can run it.
///
/// `index` finds a row by filter in O(1). It holds the row `seq`, and the row is then found by
/// binary search, so a removal never has to renumber the other rows.
#[derive(Default)]
struct Subs {
    rows: Vec<Sub>,
    index: HashMap<SharedString, u64>,
    next_seq: u64,
    /// The last request ID that `new_req` gave out. IDs are never reused, so the result of an
    /// older row with the same filter cannot match a new row.
    last_req: u64,
    /// The rows changed since the store got the filters (see `Navigator::sync_subs`).
    dirty: bool,
    /// Rises when a row, or its state, may have changed. The subscription list redraws on it.
    rev: u64,
}

impl Subs {
    /// The rows of a profile. A filter that is in the list twice makes one row.
    fn from_profile(list: &[(String, Qos)]) -> Self {
        let mut subs = Self::default();
        for (filter, qos) in list {
            subs.push(filter, *qos);
        }
        subs
    }

    /// The rows of the active profile (none without a profile), with their saved options.
    fn of_profile(profile: Option<&Profile>) -> Self {
        let Some(p) = profile else {
            return Self::default();
        };
        let mut subs = Self::from_profile(&p.subscriptions);
        for s in &mut subs.rows {
            s.options = p.options_of(&s.filter);
        }
        subs
    }

    /// Writes the rows that the user sees into `p`. A default option is not stored.
    fn write_to(&self, p: &mut Profile) {
        p.subscriptions = self.wanted();
        p.subscription_options = self.wanted_options();
    }

    /// A new request ID for a Subscribe.
    fn new_req(&mut self) -> u64 {
        self.last_req += 1;
        self.last_req
    }

    /// Checks a change of options. It changes nothing: the row takes the options only when the
    /// net thread accepted the Subscribe (`commit_options`), so the row and the net thread
    /// agree. A row that is being removed does not change.
    fn check_options_change(&mut self, filter: &str, options: SubOptions) -> OptionsChange {
        let Some(sub) = self.row(filter) else {
            return OptionsChange::Ignore;
        };
        if sub.state.removal_accepted() || sub.options == options {
            return OptionsChange::Ignore;
        }
        match options.check(filter) {
            Err(text) => OptionsChange::Refused(text),
            Ok(()) => OptionsChange::Go(sub.qos),
        }
    }

    fn get(&self, filter: &str) -> Option<&Sub> {
        self.pos(filter).and_then(|i| self.rows.get(i))
    }

    /// Checks a change of QoS. Like `check_options_change`, it changes nothing.
    fn check_qos_change(&mut self, filter: &str, qos: Qos) -> QosChange {
        match self.row(filter) {
            Some(s) if !s.state.removal_accepted() && s.qos != qos => QosChange::Go(s.options),
            _ => QosChange::Ignore,
        }
    }

    /// The row takes `qos`. `req`: as in `commit_options`.
    fn commit_qos(&mut self, filter: &str, qos: Qos, req: Option<u64>) {
        if let Some(s) = self.row(filter) {
            s.qos = qos;
            if let Some(req) = req {
                s.req = req;
                s.state = SubState::Pending;
            }
        }
    }

    /// The row takes `options`. `req`: the ID of the Subscribe that the net thread accepted,
    /// and the row waits for its result. `None`: nothing was sent (no connection).
    fn commit_options(&mut self, filter: &str, options: SubOptions, req: Option<u64>) {
        if let Some(s) = self.row(filter) {
            s.options = options;
            if let Some(req) = req {
                s.req = req;
                s.state = SubState::Pending;
            }
        }
    }

    /// The options that differ from the default, for the rows that are wanted.
    fn wanted_options(&self) -> std::collections::BTreeMap<String, SubOptions> {
        self.rows
            .iter()
            .filter(|s| !s.state.removal_accepted() && s.options != SubOptions::default())
            .map(|s| (s.filter.to_string(), s.options))
            .collect()
    }

    /// Adds a Pending row. A known filter changes nothing.
    fn push(&mut self, filter: &str, qos: Qos) {
        if self.index.contains_key(filter) {
            return;
        }
        let filter = SharedString::from(filter.to_string());
        self.index.insert(filter.clone(), self.next_seq);
        self.rows.push(Sub {
            filter,
            qos,
            options: SubOptions::default(),
            state: SubState::Pending,
            req: 0,
            seq: self.next_seq,
        });
        self.next_seq += 1;
        self.dirty = true;
        self.rev += 1;
    }

    fn contains(&self, filter: &str) -> bool {
        self.index.contains_key(filter)
    }

    fn pos(&self, filter: &str) -> Option<usize> {
        let seq = *self.index.get(filter)?;
        self.rows.binary_search_by_key(&seq, |s| s.seq).ok()
    }

    fn row(&mut self, filter: &str) -> Option<&mut Sub> {
        self.rev += 1;
        let i = self.pos(filter)?;
        self.rows.get_mut(i)
    }

    fn remove(&mut self, filter: &str) {
        if let Some(i) = self.pos(filter) {
            self.rows.remove(i);
            self.index.remove(filter);
            self.dirty = true;
            self.rev += 1;
        }
    }

    /// True once after a change of the row list.
    fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Filters that the next Connect must subscribe. An accepted removal is not one of them.
    fn wanted(&self) -> Vec<(String, Qos)> {
        self.rows
            .iter()
            .filter(|s| !s.state.removal_accepted())
            .map(|s| (s.filter.to_string(), s.qos))
            .collect()
    }

    /// The connection changed. The broker forgot its subscriptions (clean start), so SUBACKs
    /// come again. A row whose removal the net thread accepted is gone: the filter is not in the
    /// desired list, and the new session does not have it.
    fn connection_reset(&mut self) {
        self.rev += 1;
        let before = self.rows.len();
        self.rows.retain(|s| !s.state.removal_accepted());
        if self.rows.len() != before {
            self.index
                .retain(|_, seq| self.rows.binary_search_by_key(seq, |s| s.seq).is_ok());
            self.dirty = true;
        }
        for s in &mut self.rows {
            s.state = SubState::Pending;
        }
    }

    /// A new connection: the net thread builds its wanted set from the profile, with ID 0.
    fn new_connection(&mut self) {
        self.connection_reset();
        for s in &mut self.rows {
            s.req = 0;
        }
    }

    /// A SUBSCRIBE result. `req` is the ID of the Subscribe that it answers. A result with
    /// another ID belongs to an older state of the filter (an older change, or an older row of
    /// the same filter) and changes nothing.
    fn on_subscribed(&mut self, filter: &str, req: u64, result: Result<Qos, String>) {
        if let Some(s) = self.row(filter)
            && s.req == req
            && !s.state.removal_accepted()
        {
            s.state = result.map_or_else(SubState::Failed, SubState::Active);
        }
    }

    /// The net thread took the UNSUBSCRIBE.
    fn begin_remove(&mut self, filter: &str) {
        if let Some(s) = self.row(filter) {
            s.state = SubState::Removing;
        }
    }

    /// The UNSUBSCRIBE could not be queued.
    fn remove_not_sent(&mut self, filter: &str, why: String) {
        if let Some(s) = self.row(filter) {
            s.state = SubState::NotRemoved(why);
        }
    }

    /// The UNSUBSCRIBE result for `filter`.
    fn on_unsubscribed(&mut self, filter: &str, result: Result<(), String>) {
        match result {
            Ok(()) => {
                if self.row(filter).is_some_and(|s| s.state.removal_accepted()) {
                    self.remove(filter);
                }
            }
            Err(why) => {
                if let Some(s) = self.row(filter)
                    && s.state.removal_accepted()
                {
                    s.state = SubState::RemoveFailed(why);
                }
            }
        }
    }
}

/// Decides which connection generation the UI shows. It is free of gpui types, so that tests can
/// run it.
///
/// The UI chooses the generation of a connection: `Net::connect` returns it when the Connect is
/// queued, and the net thread uses it for every status, event and message of that connection.
/// The UI accepts data of its current generation only. A switch, a delete or a cancel clears
/// the current generation. Until the next Connect, nothing of an older connection can pass,
/// even if its status has not reached the UI yet.
#[derive(Default)]
struct GenGate {
    current: Option<u64>,
}

impl GenGate {
    /// A Connect with generation `g` was queued. It replaces the older one.
    fn begin(&mut self, g: u64) {
        self.current = Some(g);
    }

    /// The profile changed or the connection was cancelled.
    fn clear(&mut self) {
        self.current = None;
    }

    /// Generation 0 is not a connection. The net thread uses it for a start error and for the
    /// answers to commands that came before any Connect.
    fn accept(&self, g: u64) -> bool {
        g == 0 || self.current == Some(g)
    }

    /// The generation of the shown data. 0: no connection.
    fn generation(&self) -> u64 {
        self.current.unwrap_or(0)
    }

    /// A Connect was queued, and no status of it has arrived (`status_generation` is another).
    fn outstanding(&self, status_generation: u64) -> bool {
        self.current.is_some_and(|c| c != status_generation)
    }
}

/// True while a connection is up or on its way: a live state, or a Connect that has no status
/// yet. A save of the active profile must not replace the runtime profile in this time.
fn connection_live(state: &ConnState, gate: &GenGate, status_generation: u64) -> bool {
    matches!(
        state,
        ConnState::Connected | ConnState::Connecting | ConnState::Reconnecting { .. }
    ) || gate.outstanding(status_generation)
}

/// A connection of the current generation is on its way or up. A status of another generation
/// (the old connection after a reconnect or a cancel) does not count: the gate drops its later
/// statuses, so it would never end.
fn connection_active(state: &ConnState, gate: &GenGate, status_generation: u64) -> bool {
    gate.outstanding(status_generation)
        || (gate.current == Some(status_generation)
            && !matches!(state, ConnState::Disconnected | ConnState::Failed(_)))
}

/// A topic or filter as drawn: invisible and direction characters are made visible. The
/// original string stays for publish, subscribe and copy.
fn safe(s: &SharedString) -> SharedString {
    match display_safe(s) {
        std::borrow::Cow::Borrowed(_) => s.clone(),
        std::borrow::Cow::Owned(o) => o.into(),
    }
}

/// Text for a command that the net queue did not accept.
fn send_error_text(e: SendError) -> &'static str {
    match e {
        SendError::Full => "Too many requests are waiting. Try again.",
        SendError::Closed => "The connection task stopped.",
    }
}

/// Checks a topic filter before it is sent. `Err` is the text for the user.
fn validate_filter(filter: &str) -> Result<(), &'static str> {
    use navigator_core::topic::{FilterError, StringError, check_filter};
    check_filter(filter).map_err(|e| match e {
        FilterError::Empty => "Type a topic filter.",
        FilterError::String(StringError::TooLong) => "The filter is longer than 65535 bytes.",
        FilterError::String(StringError::Nul) => "The filter must not contain a NUL character.",
        FilterError::String(_) => "The filter must not contain a control or unassigned character.",
        FilterError::Share(_) => "Use $share/group/filter with a group that has no + or #.",
        FilterError::Hash(_) => "Use # alone as the last level, for example a/#.",
        FilterError::Plus(_) => "Use + alone as a whole level, for example a/+/b.",
    })
}

/// See [`Navigator::composer_protocol`].
///
/// - A Connect waits for the keyring (`pending`): the saved version of the profile that it
///   connects. `conn_protocol` changes only when the Connect command goes out.
/// - Else a connection or an attempt is `active`: the connection's version.
/// - Else the saved version of the selected profile.
fn composer_protocol(
    pending: Option<Protocol>,
    active: bool,
    connection: Protocol,
    saved: Option<Protocol>,
) -> Protocol {
    match (pending, active) {
        (Some(p), _) => p,
        (None, true) => connection,
        (None, false) => saved.unwrap_or(connection),
    }
}

pub struct Navigator {
    /// The active profile. `None`: the user has not chosen one (or deleted it).
    profile: Option<Profile>,
    /// The unsaved "Command line" profile. It is never written to the settings file.
    session: Option<Profile>,
    /// The settings in memory. The writer thread saves them (see `persist`).
    settings: Settings,
    /// The settings file has arrived. Until then the sheet and saves are off.
    settings_loaded: bool,
    /// The file could not be read, so the app must never replace it.
    read_only: bool,
    writer: Option<Writer>,
    secrets: Secrets,
    /// Secrets that this session knows (loaded, entered or from the command line).
    secret_cache: HashMap<Uuid, Cached>,
    /// The keyring read that a Connect waits for: its token and the profile. A switch, a delete,
    /// an edit or a cancel clears it. A reply with another token never connects.
    secret_req: Option<(u64, Uuid)>,
    next_secret_token: u64,
    /// The active profile was edited while the connection was live. The connection still uses
    /// the old settings.
    reconnect_pending: bool,
    /// The profile of each recent Connect, by generation. A report of the session filters can
    /// come after a switch, and it belongs to the old profile.
    connect_owners: Vec<(u64, Uuid)>,
    /// The MQTT version of the latest Connect.
    conn_protocol: Protocol,
    /// The primary button of the title bar. A choice or a save moves the focus here.
    connect_focus: FocusHandle,
    /// The connection menu button. A cancelled switch question returns the focus here.
    menu_focus: FocusHandle,
    /// The settings (gear) button and the "Choose a connection" button. Escape in their menus
    /// returns the focus to the button.
    settings_focus: FocusHandle,
    choose_focus: FocusHandle,
    /// The user closed the failure banner. A new connect shows it again.
    banner_closed: bool,
    sheet: Option<WeakEntity<ConnectionSheet>>,
    /// The window system can resize a new window soon after the first frame. A resize before
    /// this time is not the user's. `None`: no frame yet.
    resize_settled_at: Option<Instant>,
    user_resized: bool,
    /// The timer that saves the window size after the last resize event.
    resize_save: Option<Task<()>>,
    last_size: (f32, f32),
    net: Net,
    store: TopicStore,
    status: Status,
    subs: Subs,
    view: View,
    /// Selected message (Messages view). It is cleared when the user selects a topic.
    sel_msg: Option<SelMsg>,
    /// Selected topic (Tree view).
    sel_topic: Option<NodeId>,
    /// What the details pane shows.
    detail_of: DetailOf,
    /// Keep the newest row at the top of the Messages view.
    follow: Follow,
    /// The renderer line of the settings menu. The first menu open reads it, not the start: on
    /// Windows `gpu_specs` loads driver libraries on the UI thread.
    renderer: Rc<OnceCell<SharedString>>,
    /// The update badge, and the check that runs.
    update: update::UpdateUi,
    direction: DirectionFilter,
    /// The subscription filter of "Show only".
    show_only: Option<String>,
    filter_input: Entity<InputState>,
    /// The running debounce timer. A new change replaces (and cancels) it.
    debounce: Option<Task<()>>,
    /// Wakes the frame loop when the store has new work (a filter change or a pause).
    kick: tokio::sync::mpsc::Sender<()>,
    /// The visible top row of the list and its pixel offset in the row. Kept while Follow is
    /// off, so that new rows do not move the content under the viewport.
    anchor: Option<Anchor>,
    /// Local time zone offset in seconds. It is read once in the background.
    utc_offset: i32,
    root_focus: FocusHandle,
    list_focus: FocusHandle,
    /// Connection generation of the shown data. Newer data from the net thread replaces it.
    gate: GenGate,
    /// The details pane (Value and History tabs, decoder, background render job).
    /// The details pane (its own view, see `details_sig`).
    pane: Entity<details::DetailsPane>,
    /// Counts the user's selections: a new one drops the History row of the pane.
    selection: u64,
    /// The last `details_sig` that the pane got.
    details_sig: u64,
    /// The window of this view. A merge has none, and tells the pane through it.
    window_handle: AnyWindowHandle,
    /// The details sheet of a narrow window is open.
    sheet_open: bool,
    /// Open the sheet at the next frame (a row was selected in a narrow window).
    want_sheet: bool,
    /// The details pane is widened over the message list.
    expanded: bool,
    /// The details pane is open. It stays closed until the user opens it again.
    details_open: bool,
    /// The window is narrower than `details::NARROW`: the details open as a sheet.
    narrow: bool,
    /// The publish composer (its own view, see `composer_sig`).
    comp: Entity<compose::ComposerPane>,
    /// The last `composer_sig` that the composer got.
    comp_sig: u64,
    /// The widths of the three panes (subscriptions, centre, details).
    panes: Entity<ResizableState>,
    /// The details width before "Expand". "Restore" returns to it.
    expand_restore: Option<Pixels>,
    /// "Reset layout" asked for the default pane widths. The next frame does it.
    pane_reset: bool,
    /// Pixels of one rem in the last frame. Row heights follow it (zoom).
    rem: f32,
    list_scroll: UniformListScrollHandle,
    tree_scroll: UniformListScrollHandle,
    subs_scroll: UniformListScrollHandle,
    /// The subscription rows, as a cached view: a message flood redraws the main view each frame,
    /// and this list stays as it was until `sub_list_sig` changes.
    sub_list: Entity<SubList>,
    sub_list_sig: u64,
    sub_input: Entity<InputState>,
    /// Error text under the subscribe input.
    sub_error: Option<String>,
    /// Set when the connection came back by itself. Cleared when the connection ends again.
    reconnected_at: Option<std::time::SystemTime>,
    /// The CONNACK facts of the live connection, for "Connection details".
    details: DetailsCache,
    sub_qos: Qos,
    next_op: u64,
    last_result: Option<(u64, String)>,
    rate: Rate,
    counts: IngestCounts,
    backlog: ingest::Backlog,
    /// The net queue. The frame loop and the next-frame callback of `render` share it.
    incoming: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<navigator_core::Incoming>>>,
    /// The message that woke the frame loop. Every allocation takes it before the queue, so the
    /// order of the messages does not change.
    held: Option<navigator_core::Incoming>,
    /// Start of the last allocation. `allocate` uses it to keep one frame budget for the loop and
    /// the frame callbacks.
    last_alloc: Option<Instant>,
    /// Time of the previous frame callback (see `may_allocate`).
    last_frame: Option<Instant>,
    /// A frame callback is registered. `arm_frame` keeps it so while backlog mode lasts.
    armed: bool,
    /// Time of the last redraw that `allocate` asked for, and the time of a skipped one that is
    /// still due.
    last_redraw: Option<Instant>,
    redraw_due: Option<Instant>,
    metrics: Metrics,
    /// Metrics only: `MQTT_NAVIGATOR_AUTOSELECT=<topic>` (ignored without
    /// `MQTT_NAVIGATOR_METRICS`). When this topic arrives, the app selects it and applies Show
    /// all, as a click would, and the app connects to the saved profile at start. It exists to
    /// measure the frame time of a large payload without UI automation.
    autoselect: Option<String>,
    /// Metrics only: the topic of `autoselect` was selected. The pane then starts with Show all.
    autoselected: bool,
    _subscriptions: Vec<Subscription>,
}

/// Totals of the ingest path since the start of the session. They tell the cause of a drop:
/// `dequeued = stale + admitted + store drops + topic drops` (the store counts the last two).
#[derive(Clone, Copy, Debug, Default)]
struct IngestCounts {
    /// Messages that the merge took from the net queue.
    dequeued: u64,
    /// Messages of an old connection generation. The merge rejects them.
    stale: u64,
    /// Messages that the store accepted (a row in the list and a node value).
    admitted: u64,
    /// Merge allocations that ran with the backlog plan (paced by a frame).
    paced: u64,
    /// Longest wait of the first message of a merge in the net queue, since the last log line.
    age_max: Duration,
}

#[derive(Default)]
struct Rate {
    window_start: Option<Instant>,
    count: u64,
    per_sec: u64,
    /// A zeroing timer is pending.
    zero_pending: bool,
}

impl Navigator {
    /// `cli` is the session profile from the command line. The window opens first. The settings
    /// arrive later, as an event.
    pub fn new(
        cli: Option<ConnectConfig>,
        load: Load,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut secret_cache = HashMap::new();
        let profile = cli.map(|c| {
            // The command line password stays in memory. It is never saved.
            secret_cache.insert(
                c.profile.id,
                Cached {
                    password: c.password,
                    passphrase: c.key_passphrase,
                    loaded: true,
                    ..Cached::default()
                },
            );
            c.profile
        });
        let mut net = Net::start(NetLimits::default());
        let incoming = std::mem::replace(&mut net.incoming, tokio_channel_placeholder());
        let events = std::mem::replace(&mut net.events, tokio_channel_placeholder());
        let status_rx = net.status.clone();
        // The UI reads the event log by itself. The net thread waits for room in a full log, and
        // this handle tells it when the UI took events out.
        let drained = net.events_drained();

        let sub_input = cx.new(|cx| InputState::new(window, cx).placeholder("Topic filter"));
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(filter_placeholder(View::Messages))
                .clean_on_escape()
        });
        // Each change restarts the debounce timer. Escape clears the text and sends a change too.
        let filter_events = cx.subscribe(&filter_input, |v, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                v.debounce_filter(cx);
            }
        });
        let sub_events = cx.subscribe_in(
            &sub_input,
            window,
            |v, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => v.add_sub(window, cx),
                InputEvent::Change if v.sub_error.take().is_some() => cx.notify(),
                _ => {}
            },
        );

        // `main` applied the saved theme. In System mode, a change of the OS appearance (an
        // event, no polling) applies again.
        let appearance = cx.observe_window_appearance(window, |v, window, cx| {
            if v.settings.ui.theme_mode == ThemeMode::System {
                v.apply_theme(window, cx);
                // The refresh draws the pane with the new theme: its signature must follow.
                v.sync_composer(cx);
            }
        });
        let bounds = cx.observe_window_bounds(window, |v, window, cx| v.on_resize(window, cx));
        let (kick, kick_rx) = tokio::sync::mpsc::channel(1);
        let incoming = Arc::new(tokio::sync::Mutex::new(incoming));
        Self::spawn_ingest(incoming.clone(), kick_rx, cx);
        Self::spawn_events(events, drained, cx);
        Self::spawn_status(status_rx, cx);
        // The time zone file is read off the UI thread.
        cx.spawn(async move |this, cx| {
            let offset = cx
                .background_executor()
                .spawn(async { jiff::ZonedDateTime::now().offset().seconds() })
                .await;
            this.update(cx, |v, cx| {
                v.utc_offset = offset;
                cx.notify();
            })
            .ok();
        })
        .detach();
        let root_focus = cx.focus_handle();
        root_focus.focus(window, cx);

        let subs = Subs::of_profile(profile.as_ref());
        let (writer_errors, writer_errors_rx) = tokio::sync::mpsc::channel(4);
        let writer = crate::persist::settings_path().map(|p| Writer::start(p, writer_errors));
        let list_focus = cx.focus_handle().tab_stop(true);
        let comp = {
            let nav = cx.weak_entity();
            let list = list_focus.clone();
            cx.new(|cx| compose::ComposerPane::new(nav, list, window, cx))
        };
        // The pane tells when it needs another room.
        let comp_layout = cx.subscribe(&comp, |_, _, _: &compose::Relayout, cx| cx.notify());
        // Each notify of this view can change an input of the pane: it hears of it before the frame.
        let comp_inputs = cx.observe(&cx.entity(), |v, _, cx| v.sync_composer(cx));
        let mut this = Self {
            session: profile.clone(),
            profile,
            settings: Settings::default(),
            settings_loaded: false,
            read_only: false,
            writer,
            secrets: Secrets::start(),
            secret_cache,
            secret_req: None,
            next_secret_token: 1,
            reconnect_pending: false,
            connect_owners: Vec::new(),
            conn_protocol: Protocol::V311,
            connect_focus: cx.focus_handle(),
            menu_focus: cx.focus_handle(),
            settings_focus: cx.focus_handle(),
            choose_focus: cx.focus_handle(),
            banner_closed: false,
            sheet: None,
            resize_settled_at: None,
            user_resized: false,
            resize_save: None,
            last_size: {
                let s = window.viewport_size();
                (f32::from(s.width), f32::from(s.height))
            },
            net,
            store: TopicStore::new(Limits::default()),
            status: Status::default(),
            subs,
            view: View::Messages,
            sel_msg: None,
            detail_of: DetailOf::Nothing,
            sel_topic: None,
            follow: Follow::default(),
            renderer: Rc::default(),
            update: update::UpdateUi::default(),
            direction: DirectionFilter::All,
            show_only: None,
            filter_input,
            debounce: None,
            kick,
            anchor: None,
            utc_offset: 0,
            root_focus,
            // A Tab stop: the keyboard reaches the rows with Tab.
            list_focus,
            gate: GenGate::default(),
            pane: {
                let nav = cx.weak_entity();
                cx.new(|cx| details::DetailsPane::new(nav, window, cx))
            },
            selection: 0,
            details_sig: 0,
            window_handle: window.window_handle(),
            sheet_open: false,
            want_sheet: false,
            expanded: false,
            details_open: false,
            narrow: false,
            comp,
            comp_sig: 0,
            panes: cx.new(|_| ResizableState::default()),
            expand_restore: None,
            pane_reset: false,
            rem: 16.,
            list_scroll: UniformListScrollHandle::new(),
            tree_scroll: UniformListScrollHandle::new(),
            subs_scroll: UniformListScrollHandle::new(),
            sub_list: {
                let app = cx.weak_entity();
                cx.new(|_| SubList { app })
            },
            sub_list_sig: 0,
            sub_input,
            sub_error: None,
            reconnected_at: None,
            details: DetailsCache::default(),
            sub_qos: Qos::AtMostOnce,
            next_op: 1,
            last_result: None,
            rate: Rate::default(),
            counts: IngestCounts::default(),
            backlog: ingest::Backlog::default(),
            incoming,
            held: None,
            last_alloc: None,
            last_frame: None,
            armed: false,
            last_redraw: None,
            redraw_due: None,
            metrics: Metrics::from_env(),
            autoselected: false,
            autoselect: std::env::var("MQTT_NAVIGATOR_AUTOSELECT")
                .ok()
                .filter(|_| std::env::var_os("MQTT_NAVIGATOR_METRICS").is_some()),
            // On quit, disconnect cleanly. gpui waits for quit handlers for at most 200 ms.
            _subscriptions: vec![
                appearance,
                bounds,
                comp_layout,
                comp_inputs,
                sub_events,
                filter_events,
                // On quit, the settings writer saves a pending change at once.
                cx.on_app_quit(|v: &mut Self, cx| {
                    // A resize that waits for its timer is saved too.
                    if v.resize_save.take().is_some() {
                        v.persist();
                    }
                    let net = v.net.shutdown();
                    let flush = v.writer.as_ref().map(Writer::flush);
                    let restart = v.update.restart.take();
                    let background = cx.background_executor().clone();
                    async move {
                        if let Some(flush) = flush {
                            let _ = flush.await;
                        }
                        // "Restart to update": the new process applies the update at its
                        // start and runs the new version. This comes after the save and before the slow net stop.
                        if let Some(install) = restart {
                            background
                                .spawn(async move {
                                    let args = std::env::args_os().skip(1);
                                    if let Err(e) = navigator_update::restart(&install, args) {
                                        tracing::warn!(error = %e, "Cannot start the app again");
                                    }
                                })
                                .await;
                        }
                        net.await
                    }
                }),
            ],
        };
        // The command line gives a session profile: connect at once, as before.
        if this.profile.is_some()
            && let Err(why) = this.send_connect()
        {
            tracing::warn!(%why, "The command line connection is not valid");
        }
        this.spawn_write_errors(writer_errors_rx, window, cx);
        this.apply_settings(load, window, cx);
        this
    }

    /// Accepts data of generation `g` (see [`GenGate`]). Messages, events and status can arrive
    /// in any order, so each of them calls this first.
    fn accept(&self, g: u64) -> bool {
        self.gate.accept(g)
    }

    /// A Connect is on its way and no status of it has arrived.
    fn connecting(&self) -> bool {
        self.secret_req.is_some() || self.gate.outstanding(self.status.generation)
    }

    /// Cancels the connection. From now on no data of it is accepted (see [`GenGate`]).
    fn cancel_connection(&mut self) {
        self.net.disconnect();
        self.gate.clear();
        // The gate drops the later statuses of the old connection. Its last status must not
        // stay on screen as "Connected".
        self.status = Status::default();
        self.details.clear();
        self.secret_req = None;
        self.reconnect_pending = false;
    }

    /// Sends the connect command for the active profile. The secrets must be in the cache (see
    /// `connect` in `profiles.rs`): this does no keyring call. `Err` is the text for the user:
    /// the profile or a secret is not valid, and nothing was sent.
    fn send_connect(&mut self) -> Result<(), String> {
        let Some(mut profile) = self.profile.clone() else {
            return Ok(());
        };
        // The subscription list is what the user sees now, not what the profile has saved.
        self.subs.write_to(&mut profile);
        let secrets = self
            .secret_cache
            .get(&profile.id)
            .cloned()
            .unwrap_or_default();
        let owner = profile.id;
        let config = ConnectConfig {
            session_filters: profile.session_filters.clone(),
            password: secrets.password,
            key_passphrase: secrets.passphrase,
            ..ConnectConfig::from(profile)
        };
        // A settings file or an old version can hold a value that panics the MQTT library.
        config.validate()?;
        self.reconnect_pending = false;
        self.details.clear();
        self.conn_protocol = config.protocol;
        tracing::info!(url = %self.url(), "Connect");
        match self.net.connect(config) {
            Ok(generation) => {
                self.gate.begin(generation);
                self.connect_owners.push((generation, owner));
                let extra = self.connect_owners.len().saturating_sub(16);
                self.connect_owners.drain(..extra);
                // The broker forgets the subscriptions of the old connection.
                self.subs.new_connection();
                Ok(())
            }
            // ponytail: the command queue (64) is full only under extreme UI spam.
            Err(e) => Err(send_error_text(e).into()),
        }
    }

    fn add_sub(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let filter = self.sub_input.read(cx).value().to_string();
        let error = match validate_filter(&filter) {
            Err(text) => Some(text.to_string()),
            Ok(()) if self.subs.contains(&filter) => Some(format!(
                "You are already subscribed to {}.",
                display_safe(&filter)
            )),
            Ok(()) => None,
        };
        if error.is_some() {
            self.sub_error = error;
            cx.notify();
            return;
        }
        let qos = self.sub_qos;
        // No row and no input reset if the queue refuses the command.
        let req = self.subs.new_req();
        if let Err(e) = self.send_subscribe(&filter, qos, SubOptions::default(), req) {
            self.sub_error = Some(send_error_text(e).into());
            cx.notify();
            return;
        }
        self.sub_error = None;
        self.sub_input
            .update(cx, |i, cx| i.set_value("", window, cx));
        self.subs.push(&filter, qos);
        if let Some(s) = self.subs.row(&filter) {
            s.req = req;
        }
        self.persist_subs();
        cx.notify();
    }

    /// Gives the store the subscription filters in row order (the colours follow the order). It
    /// runs at the start of a frame, not at each change: a burst of UNSUBACKs makes one copy.
    fn sync_subs(&mut self) {
        if self.subs.take_dirty() {
            let filters = self
                .subs
                .rows
                .iter()
                .map(|s| s.filter.to_string())
                .collect();
            self.store.set_subscriptions(filters);
        }
    }

    fn send_subscribe(
        &mut self,
        filter: &str,
        qos: Qos,
        options: SubOptions,
        req: u64,
    ) -> Result<(), SendError> {
        self.net.send(Command::Subscribe {
            filter: filter.to_string(),
            qos,
            options,
            req,
        })
    }

    /// The user changed the options of a row. MQTT 5 only. While a connection or an attempt is
    /// live, the change is sent as a new SUBSCRIBE with a new request ID: the broker replaces
    /// the old options. The row takes the options and the ID only when the net thread accepted
    /// the command. If it did not (queue full), the row keeps the options that the net thread
    /// holds, and shows the failure. With no connection, only the row and the profile change;
    /// the next Connect subscribes with them.
    fn set_sub_options(&mut self, filter: &str, options: SubOptions, cx: &mut Context<Self>) {
        let qos = match self.subs.check_options_change(filter, options) {
            OptionsChange::Ignore => return,
            OptionsChange::Refused(text) => {
                self.sub_error = Some(text.into());
                cx.notify();
                return;
            }
            OptionsChange::Go(qos) => qos,
        };
        self.sub_error = None;
        if self.is_live() {
            let req = self.subs.new_req();
            match self.send_subscribe(filter, qos, options, req) {
                Ok(()) => self.subs.commit_options(filter, options, Some(req)),
                Err(e) => {
                    if let Some(s) = self.subs.row(filter) {
                        s.state = SubState::Failed(send_error_text(e).into());
                    }
                }
            }
        } else {
            self.subs.commit_options(filter, options, None);
        }
        self.persist_subs();
        cx.notify();
    }

    /// The user changed the QoS of a row. It is sent as a new SUBSCRIBE with a new request ID,
    /// with the same rules as `set_sub_options`: the row takes the QoS and the ID only when the
    /// net thread accepted the command.
    fn set_sub_qos(&mut self, filter: &str, qos: Qos, cx: &mut Context<Self>) {
        let QosChange::Go(options) = self.subs.check_qos_change(filter, qos) else {
            return;
        };
        self.sub_error = None;
        if self.is_live() {
            let req = self.subs.new_req();
            match self.send_subscribe(filter, qos, options, req) {
                Ok(()) => self.subs.commit_qos(filter, qos, Some(req)),
                Err(e) => {
                    if let Some(s) = self.subs.row(filter) {
                        s.state = SubState::Failed(send_error_text(e).into());
                    }
                }
            }
        } else {
            self.subs.commit_qos(filter, qos, None);
        }
        self.persist_subs();
        cx.notify();
    }

    fn retry_sub(&mut self, filter: &str, cx: &mut Context<Self>) {
        let Some((qos, options)) = self.subs.row(filter).map(|s| (s.qos, s.options)) else {
            return;
        };
        let req = self.subs.new_req();
        let state = match self.send_subscribe(filter, qos, options, req) {
            Ok(()) => SubState::Pending,
            Err(e) => SubState::Failed(send_error_text(e).into()),
        };
        if let Some(s) = self.subs.row(filter) {
            if state == SubState::Pending {
                s.req = req;
            }
            s.state = state;
        }
        cx.notify();
    }

    /// Starts the removal. While connected, the row stays until the UNSUBACK arrives. With no
    /// connection there is nothing to wait for: the row goes at once, and the net thread only
    /// drops the filter from its desired list (best effort).
    fn remove_sub(&mut self, filter: &str, cx: &mut Context<Self>) {
        if self
            .subs
            .row(filter)
            .is_none_or(|s| s.state == SubState::Removing)
        {
            return;
        }
        let cmd = Command::Unsubscribe {
            filter: filter.to_string(),
        };
        if !self.is_live() {
            let _ = self.net.send(cmd);
            self.subs.remove(filter);
            self.persist_subs();
            cx.notify();
            return;
        }
        match self.net.send(cmd) {
            Ok(()) => {
                self.subs.begin_remove(filter);
                self.persist_subs();
            }
            Err(e) => self.subs.remove_not_sent(filter, send_error_text(e).into()),
        }
        cx.notify();
    }

    /// The frame loop. `allocate` is the only function that merges and runs store work. This loop
    /// calls it after `COALESCE` while work remains. In backlog mode (a deep net queue) the frame
    /// callback of `render` also calls it, once in each display frame, before the draw. The
    /// loop is then only a fallback for a hidden window or a slow display. With no work, the loop
    /// waits for a message or a kick and has no timer.
    fn spawn_ingest(
        incoming: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<navigator_core::Incoming>>>,
        mut kick: tokio::sync::mpsc::Receiver<()>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok((held, work, backlog)) = this.update(cx, |v, _| {
                    (v.held.is_some(), v.store.has_work(), v.backlog.on())
                }) else {
                    break;
                };
                // A message that wakes the loop goes into `held` at once, so it is never outside
                // the shared state. In backlog mode the queue is deep and needs no wake-up.
                if !held && !backlog {
                    let mut rx = incoming.lock().await;
                    let Some(first) = ingest::next_first(&mut rx, &mut kick, work).await else {
                        break;
                    };
                    if this.update(cx, |v, _| v.held = first).is_err() {
                        break;
                    }
                }
                // The time is read after the wait for a message, so the first message after a
                // quiet time does not wait.
                let Ok(since) = this.update(cx, |v, _| v.last_alloc.map(|t| t.elapsed())) else {
                    break;
                };
                let wait = if backlog {
                    Some(COALESCE)
                } else {
                    since.and_then(coalesce_wait)
                };
                if let Some(wait) = wait {
                    cx.background_executor().timer(wait).await;
                }
                if this
                    .update(cx, |v, cx| {
                        v.allocate(
                            if backlog {
                                Caller::Fallback
                            } else {
                                Caller::Loop
                            },
                            cx,
                        )
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// The one place that runs a merge allocation: the merge, the redraw and the metrics line.
    /// `may_allocate` keeps one frame budget for the loop and the frame callbacks, so a late
    /// callback or timer does nothing.
    fn allocate(&mut self, caller: Caller, cx: &mut Context<Self>) {
        let now = Instant::now();
        let ok = may_allocate(caller, now, self.last_alloc, self.last_frame);
        if caller == Caller::Frame {
            self.last_frame = Some(now);
        }
        if !ok {
            return;
        }
        // The loop holds the queue only while it waits for a message, and then nothing is due.
        let incoming = self.incoming.clone();
        let Ok(mut rx) = incoming.try_lock() else {
            return;
        };
        self.last_alloc = Some(now);
        let first = self.held.take();
        let dequeued = self.counts.dequeued;
        if let Some(line) = self.merge(first, &mut rx, caller == Caller::Frame) {
            cx.background_executor().spawn(write_metrics(line)).detach();
        }
        let msgs = self.counts.dequeued - dequeued;
        // Backlog mode needs the draw: `render` registers its frame callback.
        if !self.backlog.on() && skip_redraw(caller, msgs, self.last_redraw.map(|t| now - t)) {
            self.redraw_later(cx);
        } else {
            self.redraw(cx);
        }
        self.schedule_rate_zero(cx);
    }

    /// The redraw of a merge: the pane sync and the notify.
    fn redraw(&mut self, cx: &mut Context<Self>) {
        self.last_redraw = Some(Instant::now());
        self.redraw_due = None;
        self.sync_details(cx);
        cx.notify();
    }

    /// Skips the redraw of this merge. The next merge of a flood draws. If none comes (the flood
    /// ended), one timer draws after `REDRAW_GAP`. The timer starts from a skip and ends with it,
    /// so an idle app has none.
    fn redraw_later(&mut self, cx: &mut Context<Self>) {
        if self.redraw_due.is_some() {
            return;
        }
        self.redraw_due = Some(Instant::now());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REDRAW_GAP).await;
            let _ = this.update(cx, |v, cx| {
                // An older timer must not draw for a newer skip.
                if v.redraw_due.is_some_and(|t| t.elapsed() >= REDRAW_GAP / 2) {
                    v.redraw(cx);
                }
            });
        })
        .detach();
    }

    /// Tells the pane that an input of it changed (see `details_sig`). A merge for another topic
    /// changes nothing, and the pane keeps its last frame. The merge calls this too, so that the
    /// pane draws in the frame of the merge and not in the next one.
    fn sync_details(&mut self, cx: &mut Context<Self>) {
        let sig = self.details_sig(cx.theme());
        if sig == self.details_sig {
            return;
        }
        self.details_sig = sig;
        let (pane, selection, window) = (self.pane.clone(), self.selection, self.window_handle);
        // After this update: the pane reads this view then.
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, cx| {
                    pane.update(cx, |p, cx| p.sync(selection, window, cx))
                })
                .ok();
        });
    }

    /// Registers the frame callback while backlog mode lasts. The callback allocates, and then it
    /// registers the next one, also when it skipped. So the pacing follows the display.
    fn arm_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.armed || !self.backlog.on() {
            return;
        }
        self.armed = true;
        let this = cx.weak_entity();
        // GPUI runs the callbacks of a frame before it decides to draw. The merge in the
        // callback marks the view dirty, so the same frame draws its result.
        window.on_next_frame(move |window, cx| {
            let _ = this.update(cx, |v, cx| {
                v.armed = false;
                v.allocate(Caller::Frame, cx);
                v.arm_frame(window, cx);
            });
        });
    }

    /// A design height in pixels now. The rows are `ui(..)` high, so this equals their height.
    fn scaled(&self, design: f32) -> f32 {
        design * self.rem / 16.
    }

    fn two_lines(&self) -> bool {
        self.settings.ui.row_height == RowHeight::TwoLines
    }

    /// Height of one message row in pixels (the scroll anchor needs it).
    fn row_height(&self) -> f32 {
        self.scaled(if self.two_lines() { ROW_H2 } else { ROW_H1 })
    }

    /// Remembers the top visible row, so that `restore_anchor` can undo the shift of new rows.
    /// Follow and a pause need no anchor. A scroll that is not yet drawn has no valid offset.
    fn capture_anchor(&mut self) {
        self.anchor = None;
        self.list_scrolled();
        if self.follow.on || self.store.is_paused() {
            return;
        }
        let top = {
            let state = self.list_scroll.0.borrow();
            if state.deferred_scroll_to_item.is_some() {
                return;
            }
            -f32::from(state.base_handle.offset().y)
        };
        let (i, px) = top_row(top.max(0.), self.row_height());
        let id_at = |i: usize| match self.store.list_row(i) {
            Some(
                RowView::Message { id, .. }
                | RowView::Reconnected { id, .. }
                | RowView::Removed { id },
            ) => Some(id),
            _ => None,
        };
        if let Some(id) = id_at(i) {
            self.anchor = Some(Anchor {
                id,
                next: id_at(i + 1),
                px,
            });
        }
    }

    /// Scrolls so that the anchor row keeps its place in the viewport. If the store removed the
    /// anchor, the next older row takes its place, or the list scrolls to its end.
    fn restore_anchor(&mut self) {
        let Some(a) = self.anchor.take() else {
            return;
        };
        let h = self.row_height();
        let top = if let Some(i) = self.store.list_index_of(a.id) {
            scroll_top_of(i, a.px, h)
        } else if let Some(i) = a.next.and_then(|n| self.store.list_index_of(n)) {
            scroll_top_of(i, 0., h)
        } else if self.store.row_evicted(a.id) {
            // The list clamps this to its end.
            scroll_top_of(self.store.list_len(), 0., h)
        } else {
            // A filter hides the row. A filter change decides the position.
            return;
        };
        let state = self.list_scroll.0.borrow();
        let mut offset = state.base_handle.offset();
        offset.y = px(-top);
        state.base_handle.set_offset(offset);
    }

    /// Asks the frame loop for a frame. The channel holds one kick, so more kicks merge.
    fn kick_work(&self) {
        // A full channel means a frame is already due.
        let _ = self.kick.try_send(());
    }

    /// One frame: merges the messages and then runs store work, inside the plan of `ingest::plan`.
    fn merge(
        &mut self,
        first: Option<navigator_core::Incoming>,
        rx: &mut tokio::sync::mpsc::Receiver<navigator_core::Incoming>,
        paced: bool,
    ) -> Option<String> {
        let start = Instant::now();
        let backlog = self
            .backlog
            .update(self.net.queued_msgs(), self.net.queued_bytes());
        // A display that draws fewer frames than 1/COALESCE gets the allocation of normal mode
        // from the fallback timer: the backlog split would give it less than before.
        let plan = ingest::plan(
            self.store.has_work(),
            backlog && paced,
            self.store.over_budget(),
        );
        self.counts.paced += u64::from(backlog && paced);
        self.capture_anchor();
        let anchor_in = start.elapsed();
        let mut n = 0u64;
        let mut metrics_line = None;
        let mut next = first;
        let mut aged = false;
        // Filter rebuilds and cap trims keep a part of the allocation. `work` always does one
        // round, also when the merge used its time, so a flood cannot starve it.
        let timing = ingest::run(&mut self.store, start, plan, |store| {
            let Some(msg) = ingest::next_message(&mut next, rx) else {
                return false;
            };
            if !aged {
                // The first message of an allocation is the oldest one in it.
                aged = true;
                let age = SystemTime::now()
                    .duration_since(msg.received_at)
                    .unwrap_or_default();
                self.counts.age_max = self.counts.age_max.max(age);
            }
            self.net.release(&msg);
            self.counts.dequeued += 1;
            if self.gate.accept(msg.generation) {
                let pick = self.autoselect.as_deref() == Some(&*msg.topic);
                if let Insert::Stored(id) = store.insert(msg) {
                    self.counts.admitted += 1;
                    if pick {
                        self.sel_topic = Some(id);
                        self.sel_msg = None;
                        self.detail_of = DetailOf::Topic;
                        self.details_open = true;
                        self.selection += 1;
                        self.autoselected = true;
                    }
                }
                n += 1;
            } else {
                self.counts.stale += 1;
            }
            true
        });
        let restore_start = Instant::now();
        self.restore_anchor();
        let anchor_out = restore_start.elapsed();
        self.backlog
            .update(self.net.queued_msgs(), self.net.queued_bytes());
        // `work` can remove the selected topic (an idle leaf, near the caps).
        if self
            .sel_topic
            .is_some_and(|id| self.store.get(id).is_none())
        {
            self.sel_topic = None;
            if self.detail_of == DetailOf::Topic {
                self.detail_of = DetailOf::Nothing;
            }
        }
        let now = Instant::now();
        self.metrics
            .merge(now - start, || (plan, timing, anchor_in, anchor_out));
        if self.metrics.due(now) {
            let s = self.store.stats();
            let net_drops = self
                .net
                .stats
                .drops
                .load(std::sync::atomic::Ordering::Relaxed);
            let drops = s.drops + s.topic_drops + net_drops;
            let queue = QueueView {
                msgs: self.net.queued_msgs(),
                bytes: self.net.queued_bytes(),
                net_drops,
            };
            let line = self
                .metrics
                .line(self.rate.per_sec, drops, &s, &self.counts, queue, now);
            self.counts.age_max = Duration::ZERO;
            metrics_line = Some(line);
        }
        let r = &mut self.rate;
        let ws = *r.window_start.get_or_insert(now);
        r.count += n;
        let elapsed = now.duration_since(ws);
        if elapsed >= Duration::from_secs(1) {
            r.per_sec = (r.count as f64 / elapsed.as_secs_f64()) as u64;
            r.count = 0;
            r.window_start = Some(now);
        }
        metrics_line
    }

    /// Starts (or restarts) the debounce timer of the filter text.
    fn debounce_filter(&mut self, cx: &mut Context<Self>) {
        self.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            this.update(cx, |v, cx| {
                v.debounce = None;
                v.apply_filters(cx);
            })
            .ok();
        }));
    }

    /// Gives the store the filters of the UI. A filter that did not change is not set again,
    /// because a set restarts the rebuild (and a pause).
    fn apply_filters(&mut self, cx: &mut Context<Self>) {
        let text = self.filter_input.read(cx).value().to_string();
        let list = ListFilter {
            text: text.clone(),
            direction: self.direction,
            show_only: self.show_only.clone(),
        };
        let tree = TreeFilter {
            text,
            show_only: self.show_only.clone(),
        };
        if *self.store.list_filter() != list {
            self.store.set_list_filter(list);
        }
        if *self.store.tree_filter() != tree {
            self.store.set_tree_filter(tree);
        }
        self.kick_work();
        cx.notify();
    }

    /// Clears the text, "Show only" and the direction.
    fn clear_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.debounce = None;
        self.filter_input
            .update(cx, |i, cx| i.set_value("", window, cx));
        self.show_only = None;
        self.direction = DirectionFilter::All;
        self.apply_filters(cx);
    }

    /// The subscription in "Show only": the target of the subscription actions of the palette.
    fn show_only_sub(&self) -> Option<&Sub> {
        self.subs.get(self.show_only.as_deref()?)
    }

    fn toggle_show_only(&mut self, filter: &str, cx: &mut Context<Self>) {
        self.show_only = if self.show_only.as_deref() == Some(filter) {
            None
        } else {
            Some(filter.to_string())
        };
        self.apply_filters(cx);
    }

    /// The hint of the filter input says what the filter searches in the current view.
    fn sync_filter_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let hint = filter_placeholder(self.view);
        self.filter_input
            .update(cx, |i, cx| i.set_placeholder(hint, window, cx));
    }

    /// The user scrolled away from the top or selected a row. Follow goes off, and the store
    /// starts to count the new rows for "Jump to newest (N)". A pause keeps the state.
    fn leave_follow(&mut self) {
        if self.follow.leave(self.store.is_paused()) {
            self.store.watch_new(true);
        }
    }

    /// The list moved (a wheel event, or a scrollbar drag that sends none). A scroll that leaves
    /// the top turns Follow off. Only "Jump to newest" turns it on. Returns true when Follow
    /// went off. A scroll that is not yet drawn has no valid offset.
    fn list_scrolled(&mut self) -> bool {
        if !self.follow.on || self.view != View::Messages || self.store.is_paused() {
            return false;
        }
        let y = {
            let state = self.list_scroll.0.borrow();
            if state.deferred_scroll_to_item.is_some() {
                return false;
            }
            f32::from(state.base_handle.offset().y)
        };
        let left = left_the_top(y);
        if left {
            self.leave_follow();
        }
        left
    }

    /// "Jump to newest": scroll to the top, keep the selection, turn Follow on.
    fn jump_to_newest(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.follow.jump();
        // The button goes away: the focus moves to the list, so that the keys keep working.
        self.list_focus.focus(window, cx);
        self.store.watch_new(false);
        self.list_scroll
            .scroll_to_item_strict(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        let pause = !self.store.is_paused();
        // Follow was on before the pause: the list returns to its top, as the old frame did.
        if !pause && self.follow.on {
            self.list_scroll
                .scroll_to_item_strict(0, ScrollStrategy::Top);
        }
        self.store.set_paused(pause);
        self.kick_work();
        cx.notify();
    }

    /// When messages stop, set the rate to 0 once, 1.5 s later. The timer stops after that.
    fn schedule_rate_zero(&mut self, cx: &mut Context<Self>) {
        if self.rate.zero_pending {
            return;
        }
        self.rate.zero_pending = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(1500))
                    .await;
                let done = this.update(cx, |v, cx| {
                    let idle = v
                        .rate
                        .window_start
                        .is_none_or(|t| t.elapsed() >= Duration::from_millis(1500));
                    if idle {
                        v.rate = Rate::default();
                        cx.notify();
                    }
                    idle
                });
                if done.unwrap_or(true) {
                    break;
                }
            }
        })
        .detach();
    }

    fn spawn_events(
        mut rx: tokio::sync::mpsc::Receiver<Event>,
        drained: navigator_core::net::EventsDrained,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                // The event left the log: the net thread may have room to send more.
                drained.notify();
                if this.update(cx, |v, cx| v.on_event(ev, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn on_event(&mut self, ev: Event, cx: &mut Context<Self>) {
        let g = match &ev {
            Event::Reconnected { generation, .. }
            | Event::Subscribed { generation, .. }
            | Event::Unsubscribed { generation, .. }
            | Event::Published { generation, .. }
            | Event::SessionFilters { generation, .. }
            | Event::ConnectionDetails { generation, .. }
            | Event::Error { generation, .. } => *generation,
        };
        // The filters belong to the profile of the connection, also after a switch.
        if let Event::SessionFilters { filters, .. } = &ev {
            self.save_session_filters(g, filters);
            return;
        }
        // The op id is unique for the whole run. A result of an older generation still ends the
        // "Sending" state of its row, so it is applied before the generation check.
        if let Event::Published { op_id, result, .. } = &ev {
            let status = status_from_result(result.clone());
            let text = status_text(&status);
            self.store.set_send_status(*op_id, status);
            // The line shows the newest send only. A late result of an older send is ignored.
            if self.last_result.as_ref().is_some_and(|(id, _)| id == op_id) {
                self.last_result = Some((*op_id, text));
            }
            // The details of a sent row show its result.
            self.sync_details(cx);
        }
        if !self.accept(g) {
            // A late send result changed its row and maybe the result line: draw again.
            if matches!(ev, Event::Published { .. }) {
                cx.notify();
            }
            return;
        }
        match ev {
            Event::Reconnected { at, .. } => {
                tracing::info!("Reconnected");
                self.reconnected_at = Some(at);
                // A new row at the top: keep the reading position (Follow off).
                self.capture_anchor();
                self.store.mark_reconnected(at);
                self.restore_anchor();
            }
            Event::Subscribed {
                filter,
                req,
                result,
                ..
            } => {
                if let Err(why) = &result {
                    tracing::warn!(%why, "Subscribe failed");
                }
                self.subs.on_subscribed(&filter, req, result);
            }
            Event::ConnectionDetails {
                attempt, details, ..
            } => self.details.offer(attempt, details),
            Event::Published { .. } | Event::SessionFilters { .. } => {}
            Event::Error { message, .. } => self.last_result = Some((0, message)),
            // An UNSUBSCRIBE result shows on its row, not in the publish line.
            Event::Unsubscribed { filter, result, .. } => {
                self.subs.on_unsubscribed(&filter, result);
            }
        }
        cx.notify();
    }

    fn spawn_status(mut rx: tokio::sync::watch::Receiver<Status>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while rx.changed().await.is_ok() {
                let status = rx.borrow_and_update().clone();
                let alive = this.update(cx, |v, cx| {
                    if v.accept(status.generation) {
                        if status.state != v.status.state {
                            tracing::info!(state = ?status.state, "Connection state");
                        }
                        if status.state != ConnState::Connected {
                            v.reconnected_at = None;
                            // The broker forgets the subscriptions. Results come again.
                            v.subs.connection_reset();
                            v.details.clear();
                        }
                        v.status = status;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn toggle_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // "Cancel" while the keyring read runs: the reply must not connect.
        if self.secret_req.take().is_some() {
            cx.notify();
            return;
        }
        // "Cancel" while a Connect has no status yet: the net thread may not have read it.
        if self.gate.outstanding(self.status.generation) {
            self.net.disconnect();
            self.gate.clear();
            self.status = Status::default();
            cx.notify();
            return;
        }
        match self.status.state {
            ConnState::Connected | ConnState::Connecting | ConnState::Reconnecting { .. } => {
                self.net.disconnect()
            }
            ConnState::Disconnecting => {}
            ConnState::Disconnected | ConnState::Failed(_) => self.connect(window, cx),
        }
        cx.notify();
    }

    /// Selects a topic of the tree, and with `toggle` (a double click) expands or collapses it.
    /// `generation` is the data generation when the row was rendered. A click on a row of an
    /// older generation does nothing, because its `NodeId` may now name another topic.
    fn select(&mut self, id: NodeId, generation: u64, toggle: bool, cx: &mut Context<Self>) {
        if generation != self.gate.generation() {
            return;
        }
        let Some(node) = self.store.get(id) else {
            return;
        };
        let (has_children, expanded) = (node.has_children, node.expanded);
        self.sel_topic = Some(id);
        self.sel_msg = None;
        self.detail_of = DetailOf::Topic;
        self.selection += 1;
        // A topic with subtopics takes a double click. Details that open on the first click
        // change the layout, and the second press lands on another element.
        if !has_children {
            self.reveal_details();
        }
        if toggle && has_children {
            self.store.set_expanded(id, !expanded);
            // A paused tree takes a new snapshot: wake the frame loop for the work.
            self.kick_work();
        }
        self.sync_details(cx);
        cx.notify();
    }

    /// Selects a message row of the list. The row can be gone, then nothing changes.
    fn select_row(&mut self, id: RowId, cx: &mut Context<Self>) {
        let Some(i) = self.store.list_index_of(id) else {
            return;
        };
        if let Some(RowView::Message {
            id,
            node,
            topic,
            msg,
            direction,
            seq,
            ..
        }) = self.store.list_row(i)
        {
            self.sel_msg = Some(SelMsg {
                id,
                node,
                topic: Arc::from(topic),
                msg: msg.clone(),
                direction,
                seq,
            });
            self.detail_of = DetailOf::Message;
            self.selection += 1;
            self.reveal_details();
            self.leave_follow();
            self.list_scroll.scroll_to_item(i, ScrollStrategy::Nearest);
            self.sync_details(cx);
            cx.notify();
        }
    }

    /// A selection opens the details pane in a wide window, with the mouse or the keyboard. It
    /// stays closed until the next selection. A narrow window keeps the focus in the list: a
    /// click opens the sheet, and Enter does (`enter_on_row`).
    fn reveal_details(&mut self) {
        if !self.narrow {
            self.details_open = true;
        }
    }

    /// Enter in the list: shows the details of the selected row. A narrow window opens the
    /// sheet, a wide window the pane (a click on a topic with subtopics does not open it).
    fn enter_on_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.detail_of == DetailOf::Nothing {
            return;
        }
        if self.narrow {
            self.open_details_sheet(window, cx);
        } else {
            self.details_open = true;
            self.sync_details(cx);
            cx.notify();
        }
    }

    /// Up and Down. `step` is -1 (up) or +1 (down).
    fn step_selection(&mut self, step: isize, cx: &mut Context<Self>) {
        match self.view {
            View::Messages => {
                let len = self.store.list_len() as isize;
                let cur = self
                    .sel_msg
                    .as_ref()
                    .and_then(|s| self.store.list_index_of(s.id));
                let (mut i, step) = match cur {
                    Some(c) => (c as isize + step, step),
                    None => (0, 1),
                };
                // Markers are not selectable: skip them.
                while (0..len).contains(&i) {
                    if let Some(RowView::Message { id, .. }) = self.store.list_row(i as usize) {
                        self.select_row(id, cx);
                        return;
                    }
                    i += step;
                }
            }
            View::Tree => {
                let rows = self.store.rows();
                let cur = self
                    .sel_topic
                    .and_then(|id| rows.iter().position(|r| *r == id));
                let next = match cur {
                    Some(c) => c.checked_add_signed(step),
                    None => Some(0),
                };
                if let Some(id) = next.and_then(|n| rows.get(n).copied()) {
                    self.sel_topic = Some(id);
                    self.sel_msg = None;
                    self.detail_of = DetailOf::Topic;
                    self.selection += 1;
                    self.reveal_details();
                    if let Some(n) = next {
                        self.tree_scroll.scroll_to_item(n, ScrollStrategy::Nearest);
                    }
                    self.sync_details(cx);
                    cx.notify();
                }
            }
        }
    }

    /// Right and Left in the tree.
    fn expand_selected(&mut self, expanded: bool, cx: &mut Context<Self>) {
        if self.view != View::Tree {
            return;
        }
        if let Some(id) = self.sel_topic
            && self.store.get(id).is_some_and(|n| n.has_children)
        {
            self.store.set_expanded(id, expanded);
            self.kick_work();
            cx.notify();
        }
    }

    /// Changes the view. The selected message goes with the user: the tree selects its topic,
    /// and the list highlights its row if the row still exists. The details keep the message.
    fn set_view(&mut self, view: View, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == view {
            return;
        }
        self.view = view;
        self.sync_filter_placeholder(window, cx);
        self.settings.ui.view = match view {
            View::Messages => ViewMode::Messages,
            View::Tree => ViewMode::Tree,
        };
        self.persist();
        match view {
            View::Tree => {
                let node = self
                    .sel_msg
                    .as_ref()
                    .and_then(|s| self.live_node(s.node, &s.topic));
                if let Some(node) = node {
                    // The tree shows the node: open its ancestors.
                    let mut up = self.store.parent(node);
                    while let Some(p) = up {
                        self.store.set_expanded(p, true);
                        up = self.store.parent(p);
                    }
                    self.kick_work();
                    self.sel_topic = Some(node);
                    if let Some(i) = self.store.rows().iter().position(|r| *r == node) {
                        self.tree_scroll.scroll_to_item(i, ScrollStrategy::Center);
                    }
                }
            }
            View::Messages => {
                // The details keep the message, also when the list does not have the row now.
                let index = self
                    .sel_msg
                    .as_ref()
                    .and_then(|s| self.store.list_index_of(s.id));
                if let Some(i) = index {
                    self.list_scroll.scroll_to_item(i, ScrollStrategy::Center);
                }
            }
        }
        cx.notify();
    }

    /// The node of a topic. The store can remove a node at any `work` (an idle leaf, near the
    /// caps), so a saved id is checked, and the topic is looked up again if it is gone.
    pub(super) fn live_node(&self, node: Option<NodeId>, topic: &str) -> Option<NodeId> {
        node.filter(|n| self.store.get(*n).is_some())
            .or_else(|| self.store.find(topic))
    }

    fn connected(&self) -> bool {
        self.status.state == ConnState::Connected
    }

    /// The MQTT version that the composer follows: the connection's while a connection or an
    /// attempt is live, else the saved version of the selected profile. `self.profile` is the
    /// profile of the last Connect: an edit that was saved during the connection is not in it.
    fn composer_protocol(&self) -> Protocol {
        let saved_of = |id| self.find_profile(id).map(|p| p.protocol);
        let pending = self.secret_req.and_then(|(_, id)| saved_of(id));
        let active = connection_active(&self.status.state, &self.gate, self.status.generation);
        let saved = self
            .profile
            .as_ref()
            .map(|p| saved_of(p.id).unwrap_or(p.protocol));
        composer_protocol(pending, active, self.conn_protocol, saved)
    }

    fn state_text(&self) -> String {
        match &self.status.state {
            _ if self.connecting() => "Connecting…".into(),
            ConnState::Disconnected => "Disconnected".into(),
            ConnState::Connecting => "Connecting…".into(),
            ConnState::Reconnecting { attempt, .. } => format!("Reconnecting ({attempt})…"),
            ConnState::Connected => "Connected".into(),
            ConnState::Disconnecting => "Disconnecting…".into(),
            ConnState::Failed(why) => format!("Disconnected: {why}"),
        }
    }

    fn url(&self) -> String {
        let Some(p) = &self.profile else {
            return "No connection".into();
        };
        let scheme = match p.transport {
            navigator_core::net::Transport::Tls => "mqtts",
            navigator_core::net::Transport::Tcp => "mqtt",
            navigator_core::net::Transport::Wss => "wss",
            navigator_core::net::Transport::Ws => "ws",
        };
        format!("{scheme}://{}", host_port(p))
    }

    fn render_title(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let dot = match self.status.state {
            _ if self.connecting() => theme.warning,
            ConnState::Connected => theme.success,
            ConnState::Connecting | ConnState::Reconnecting { .. } | ConnState::Disconnecting => {
                theme.warning
            }
            ConnState::Failed(_) => theme.danger,
            ConnState::Disconnected => theme.muted_foreground,
        };
        let menu_button = self.render_menu_button(dot, cx);
        let theme = cx.theme();
        let reason = match &self.status.state {
            ConnState::Reconnecting { reason, .. } => Some(reason.clone()),
            _ => None,
        };
        let label = match self.status.state {
            _ if self.connecting() => "Cancel",
            ConnState::Connected => "Disconnect",
            ConnState::Connecting | ConnState::Reconnecting { .. } => "Cancel",
            _ => "Connect",
        };
        TitleBar::new().child(
            h_flex()
                .w_full()
                .pr_2()
                .gap_3()
                .child(caption_control(menu_button))
                .child(
                    h_flex()
                        .gap_1()
                        .min_w_0()
                        .child(div().size_2().flex_none().rounded_full().bg(dot))
                        .child(
                            div()
                                .text_sm()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(self.state_text()),
                        )
                        .when_some(reason, |d, why| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .max_w(ui(360.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(why),
                            )
                        }),
                )
                .child(div().flex_1())
                .children(self.render_update_badge(cx).map(caption_control))
                .child(caption_control(self.render_settings_menu(cx)))
                .when(self.profile.is_some(), |d| {
                    d.child(caption_control(
                        Button::new("connect")
                            .debug_selector(|| "connect".into())
                            .small()
                            .primary()
                            .label(if self.status.state == ConnState::Disconnecting {
                                "Disconnecting…"
                            } else {
                                label
                            })
                            .tooltip(self.primary_tooltip(label))
                            .track_focus(&self.connect_focus)
                            .disabled(self.status.state == ConnState::Disconnecting)
                            .on_click(
                                cx.listener(|v, _, window, cx| v.toggle_connection(window, cx)),
                            ),
                    ))
                }),
        )
    }

    /// "Connect to lab (mqtt://localhost:18830)".
    fn primary_tooltip(&self, label: &str) -> String {
        let name = self.profile.as_ref().map(display_name).unwrap_or_default();
        match label {
            "Connect" => format!("Connect to {name} ({})", self.url()),
            "Cancel" => format!("Cancel the connection to {name}"),
            _ => format!("Disconnect from {name}"),
        }
    }

    /// The connection menu button of the title bar. With no connection at all, it opens the
    /// sheet. Below 900 px, it shows the name only.
    fn render_menu_button(&self, dot: Hsla, cx: &mut Context<Self>) -> AnyElement {
        let count = self.all_profiles().count();
        let Some(p) = &self.profile else {
            let button = Button::new("connection-menu")
                .small()
                .track_focus(&self.menu_focus)
                .label(choose_text(count));
            return if count == 0 {
                button
                    .on_click(cx.listener(|v, _, window, cx| v.open_sheet(None, true, window, cx)))
                    .into_any_element()
            } else {
                button
                    .dropdown_caret(true)
                    .dropdown_menu(self.connection_menu(&self.menu_focus, cx))
                    .into_any_element()
            };
        };
        let (name, addr) = (display_name(p), host_port(p));
        let narrow = self.last_size.0 < 900.;
        Button::new("connection-menu")
            .debug_selector(|| "connection-menu".into())
            .small()
            .max_w(ui(320.))
            .track_focus(&self.menu_focus)
            .tooltip(format!("{name} ({})", self.url()))
            .dropdown_caret(true)
            .child(div().size_2().flex_none().rounded_full().bg(dot))
            .child(
                div()
                    .max_w(ui(240.))
                    .flex_shrink_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(name),
            )
            .when(!narrow, |b| {
                b.child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("· {addr}")),
                )
            })
            .dropdown_menu(self.connection_menu(&self.menu_focus, cx))
            .into_any_element()
    }

    /// The settings menu of the title bar (the gear): Commands…, the submenus Appearance, Theme,
    /// Row height and Memory budget, Reset the layout, the renderer and the version. The checked items of a
    /// submenu show the current choice. The top level has no checked item and is short, so it
    /// fits a small window and does not scroll (a scrollable menu jumps to its checked item).
    fn render_settings_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.settings.ui.theme_mode;
        let current = crate::themes::preset_by_id(&self.settings.ui.theme_preset);
        let (two_lines, memory) = (self.two_lines(), self.memory_mib());
        let renderer = self.renderer.clone();
        let weak = cx.entity().downgrade();
        let focus = self.settings_focus.clone();
        Button::new("settings")
            .debug_selector(|| "settings".into())
            .small()
            .ghost()
            .icon(IconName::Settings)
            .track_focus(&self.settings_focus)
            .tooltip("Settings")
            .dropdown_menu(move |menu, window, cx| {
                // The action gives the menu its shortcut text (the key binding).
                let menu = menu
                    .action_context(focus.clone())
                    .item(PopupMenuItem::new("Commands…").action(Box::new(OpenPalette)));
                let w = weak.clone();
                let menu =
                    menu.separator()
                        .submenu("Appearance", window, cx, move |mut menu, _, _| {
                            for (label, m) in [
                                ("System", ThemeMode::System),
                                ("Light", ThemeMode::Light),
                                ("Dark", ThemeMode::Dark),
                            ] {
                                let w = w.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(label).checked(mode == m).on_click(
                                        move |_, window, cx| {
                                            w.update(cx, |v, cx| v.set_appearance(m, window, cx))
                                                .ok();
                                        },
                                    ),
                                );
                            }
                            menu
                        });
                let w = weak.clone();
                let menu = menu.submenu("Theme", window, cx, move |mut menu, _, _| {
                    for p in crate::themes::PRESETS {
                        let w = w.clone();
                        menu = menu.item(
                            PopupMenuItem::new(p.label)
                                .checked(p.id == current.id)
                                .on_click(move |_, window, cx| {
                                    w.update(cx, |v, cx| v.set_preset(p.id, window, cx)).ok();
                                }),
                        );
                    }
                    menu
                });
                let w = weak.clone();
                let menu = menu.submenu("Row height", window, cx, move |mut menu, _, _| {
                    for (label, two) in [("1 line", false), ("2 lines", true)] {
                        let w = w.clone();
                        menu = menu.item(
                            PopupMenuItem::new(label)
                                .checked(two_lines == two)
                                .on_click(move |_, _, cx| {
                                    w.update(cx, |v, cx| v.set_row_height(two, cx)).ok();
                                }),
                        );
                    }
                    menu
                });
                let w = weak.clone();
                let menu = menu.submenu("Memory budget", window, cx, move |mut menu, _, _| {
                    for mib in crate::palette::MEMORY_OPTIONS_MIB {
                        let w = w.clone();
                        menu = menu.item(
                            PopupMenuItem::new(crate::palette::memory_text(mib))
                                .checked(mib == memory)
                                .on_click(move |_, _, cx| {
                                    w.update(cx, |v, cx| v.set_memory_limit(mib, cx)).ok();
                                }),
                        );
                    }
                    menu
                });
                let w = weak.clone();
                menu.separator()
                    .item(
                        PopupMenuItem::new("Reset the layout").on_click(move |_, window, cx| {
                            w.update(cx, |v, cx| v.run_cmd(Cmd::ResetLayout, window, cx))
                                .ok();
                        }),
                    )
                    .separator()
                    .label(format!(
                        "Renderer: {}",
                        renderer.get_or_init(|| {
                            crate::diagnostics::renderer_text(
                                window.gpu_specs().map(|g| g.is_software_emulated),
                            )
                            .into()
                        })
                    ))
                    .label(concat!("Version: ", env!("CARGO_PKG_VERSION")))
            })
    }

    /// One subscription row. Its height is `SUB_ROW_H`, so that the list can be virtual.
    fn sub_row(&self, i: usize, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(s) = self.subs.rows.get(i) else {
            return div().into_any_element();
        };
        let status = sub_status(&s.state, s.qos, self.connected());
        let rejected = matches!(s.state, SubState::Failed(_));
        // The ids come from the filter, not from the index: they stay when rows move.
        let row_id = ElementId::from(s.filter.clone());
        let pick = s.filter.clone();
        let active = self.show_only.as_deref() == Some(&*s.filter);
        let line = h_flex()
            .w_full()
            .gap_2()
            .child(if rejected {
                Icon::new(IconName::Close)
                    .xsmall()
                    .text_color(theme.danger)
                    .into_any_element()
            } else {
                div()
                    .flex_none()
                    .size_2()
                    .rounded_full()
                    .bg(chip_colour(i, theme))
                    .into_any_element()
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family(theme.mono_font_family.clone())
                    .text_sm()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(safe(&s.filter)),
            )
            .when(s.qos == Qos::AtLeastOnce, |d| {
                d.child(Tag::secondary().xsmall().child("Q1"))
            })
            .child(self.sub_menu(&row_id, s, active, cx));
        let status_line = status.map(|(text, problem)| {
            div()
                .w_full()
                .text_xs()
                .text_color(if problem {
                    theme.danger
                } else if active {
                    theme.accent_foreground
                } else {
                    theme.muted_foreground
                })
                .child(text)
        });
        // Click the row to show only its messages. Click again to show all. The state of the
        // row (selected) is the fill of the row.
        ListItem::new(row_id)
            .debug_selector(|| format!("sub-{}", s.filter))
            .w_full()
            .h(ui(SUB_ROW_H))
            .flex_none()
            .overflow_hidden()
            .rounded_sm()
            .px_2()
            .py_0p5()
            .selected(active)
            .on_click(cx.listener(move |v, _, _, cx| v.toggle_show_only(&pick, cx)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .w_full()
                    .child(line)
                    .children(status_line),
            )
            .into_any_element()
    }

    /// The `⋯` menu of a row: Show only, QoS, Options…, Retry and Remove.
    fn sub_menu(
        &self,
        row_id: &ElementId,
        s: &Sub,
        active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let (filter, qos, state) = (s.filter.clone(), s.qos, s.state.clone());
        let v5 = self.composer_protocol() == Protocol::V5;
        // A click on the button must not toggle "Show only" on the row.
        div()
            .id((row_id.clone(), "menu-wrap"))
            .flex_none()
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                Button::new((row_id.clone(), "menu"))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Ellipsis)
                    .tab_stop(true)
                    .tooltip("Subscription actions")
                    .dropdown_menu(move |menu, _, _| {
                        sub_menu_items(menu, &weak, &filter, qos, &state, active, v5)
                    }),
            )
    }

    /// The form of the subscription options (MQTT 5): No Local, Retain As Published and Retain
    /// Handling. Each change sends a new SUBSCRIBE (`set_sub_options`).
    fn options_form(
        weak: WeakEntity<Self>,
        filter: SharedString,
        options: SubOptions,
        locked: bool,
    ) -> AnyElement {
        let id = ElementId::from(filter.clone());
        let change = {
            let (weak, filter) = (weak.clone(), filter.clone());
            move |next: SubOptions, cx: &mut App| {
                weak.update(cx, |v, cx| v.set_sub_options(&filter, next, cx))
                    .ok();
            }
        };
        let (on_nl, on_rap, on_rh) = (change.clone(), change.clone(), change.clone());
        v_flex()
            .gap_2()
            .p_1()
            .w(ui(280.))
            .child(
                Switch::new((id.clone(), "no-local"))
                    .checked(options.no_local)
                    .disabled(locked)
                    .label("No Local")
                    .on_change(move |on: &bool, _, cx| {
                        on_nl(
                            SubOptions {
                                no_local: *on,
                                ..options
                            },
                            cx,
                        );
                    }),
            )
            .child(
                Switch::new((id.clone(), "rap"))
                    .checked(options.retain_as_published)
                    .disabled(locked)
                    .label("Retain As Published")
                    .on_change(move |on: &bool, _, cx| {
                        on_rap(
                            SubOptions {
                                retain_as_published: *on,
                                ..options
                            },
                            cx,
                        );
                    }),
            )
            .child(div().text_xs().child("Retained messages at subscribe"))
            .child(
                ToggleGroup::new((id.clone(), "rh"))
                    .segmented()
                    .single()
                    .small()
                    .disabled(locked)
                    .child(
                        Toggle::new((id.clone(), "rh-0"))
                            .label("Always")
                            .checked(options.retain_handling == RetainHandling::OnSubscribe),
                    )
                    .child(
                        Toggle::new((id.clone(), "rh-1"))
                            .label("If new")
                            .checked(options.retain_handling == RetainHandling::OnNewSubscription),
                    )
                    .child(
                        Toggle::new((id, "rh-2"))
                            .label("Never")
                            .checked(options.retain_handling == RetainHandling::Never),
                    )
                    .on_click(move |checks: &Vec<bool>, _, cx| {
                        let Some(ix) = checks.iter().position(|c| *c) else {
                            return;
                        };
                        let retain_handling = [
                            RetainHandling::OnSubscribe,
                            RetainHandling::OnNewSubscription,
                            RetainHandling::Never,
                        ][ix.min(2)];
                        on_rh(
                            SubOptions {
                                retain_handling,
                                ..options
                            },
                            cx,
                        );
                    }),
            )
            .into_any_element()
    }

    /// Opens the options of a subscription in a dialog (row menu and command palette).
    fn open_sub_options(&mut self, filter: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.composer_protocol() != Protocol::V5 || window.has_active_dialog(cx) {
            return;
        }
        let Some(filter) = self.subs.get(filter).map(|s| s.filter.clone()) else {
            return;
        };
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            // Read the row at each draw: a change sends a SUBSCRIBE and the options move.
            let (options, locked) = weak
                .upgrade()
                .and_then(|v| {
                    v.read(cx)
                        .subs
                        .get(&filter)
                        .map(|s| (s.options, s.state.removal_accepted()))
                })
                .unwrap_or_default();
            let muted = cx.theme().muted_foreground;
            dialog
                .title(format!("Options of {}", display_safe(&filter)))
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(options_text(&options)),
                )
                .child(Self::options_form(
                    weak.clone(),
                    filter.clone(),
                    options,
                    locked,
                ))
                .footer(
                    h_flex().justify_end().child(
                        Button::new("sub-options-close")
                            .label("Close")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
        });
    }

    /// The Details button of the status bar. Its popover shows the CONNACK facts (while
    /// connected), the topic count, the memory use with the budget control, and the drops.
    fn details_button(&self, drops: u64, cx: &mut Context<Self>) -> AnyElement {
        let lines = self
            .details
            .current(&self.status)
            .map(details_lines)
            .unwrap_or_default();
        let stats = self.store.stats();
        let (topics, used, current) = (stats.topics, stats.used_bytes, self.memory_mib());
        let weak = cx.entity().downgrade();
        let theme = cx.theme();
        let (muted, mono, danger, ui_font) = (
            theme.muted_foreground,
            theme.mono_font_family.clone(),
            theme.danger,
            theme.font_family.clone(),
        );
        Popover::new("status-details")
            .anchor(gpui_kit::Anchor::BottomRight)
            .trigger(
                Button::new("status-details-button")
                    .xsmall()
                    .ghost()
                    .label("Details"),
            )
            .content(move |_, _, _| {
                let fact = |label: &'static str, value: String| {
                    h_flex()
                        .gap_3()
                        .text_xs()
                        .child(div().w(ui(170.)).text_color(muted).child(label))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .font_family(mono.clone())
                                .child(value),
                        )
                };
                let weak = weak.clone();
                let budget = DropdownButton::new("memory-limit")
                    .button(
                        Button::new("memory-limit-button")
                            .xsmall()
                            .ghost()
                            .label(crate::palette::memory_text(current)),
                    )
                    .dropdown_menu(move |menu, _, _| {
                        crate::palette::MEMORY_OPTIONS_MIB.into_iter().fold(
                            menu.label("Memory budget"),
                            |menu, mib| {
                                let weak = weak.clone();
                                menu.item(
                                    PopupMenuItem::new(crate::palette::memory_text(mib))
                                        .checked(mib == current)
                                        .on_click(move |_, _, cx| {
                                            weak.update(cx, |v, cx| v.set_memory_limit(mib, cx))
                                                .ok();
                                        }),
                                )
                            },
                        )
                    });
                v_flex()
                    .gap_1()
                    .p_1()
                    .w(ui(380.))
                    .font_family(ui_font.clone())
                    .when(!lines.is_empty(), |d| {
                        d.child(div().text_sm().child("Connection details"))
                            .children(lines.iter().map(|(l, v)| fact(l, v.clone())))
                    })
                    .child(div().text_sm().child("Store"))
                    .child(fact("Topics", group_digits(topics as u64)))
                    .child(
                        h_flex()
                            .gap_3()
                            .text_xs()
                            .child(div().w(ui(170.)).text_color(muted).child("Memory"))
                            .child(
                                div()
                                    .font_family(mono.clone())
                                    .child(format!("{} MiB of", used / MIB)),
                            )
                            .child(budget),
                    )
                    .child(
                        div()
                            .when(drops > 0, |d| d.text_color(danger))
                            .child(fact("Dropped", format!("{drops} messages"))),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }

    fn render_subs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // The QoS of the next subscription: a small menu at the end of the input.
        let (current, weak) = (self.sub_qos, cx.entity().downgrade());
        let qos = Button::new("sub-qos")
            .ghost()
            .xsmall()
            .label(format!("Q{}", current.as_u8()))
            .dropdown_caret(true)
            .tooltip("QoS of the new subscription")
            .dropdown_menu(move |menu, _, _| {
                [("QoS 0", Qos::AtMostOnce), ("QoS 1", Qos::AtLeastOnce)]
                    .into_iter()
                    .fold(menu, |menu, (label, q)| {
                        let weak = weak.clone();
                        menu.item(PopupMenuItem::new(label).checked(current == q).on_click(
                            move |_, _, cx| {
                                weak.update(cx, |v, cx| {
                                    v.sub_qos = q;
                                    cx.notify();
                                })
                                .ok();
                            },
                        ))
                    })
            });
        let has_hash = self.subs.contains("#");
        // The side padding is on the parts, not on the pane: the list scrolls against the pane
        // edge, and its scrollbar sits there.
        v_flex()
            .size_full()
            .py_2()
            .gap_1()
            .child(
                div()
                    .px_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Subscriptions"),
            )
            // Only the visible rows are built. The add input stays pinned below them.
            .child(
                div()
                    .relative()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .child(
                        self.sub_list
                            .clone()
                            .cached(StyleRefinement::default().size_full()),
                    )
                    .vertical_scrollbar(&self.subs_scroll),
            )
            .when(has_hash && self.subs.rows.len() > 1, |d| {
                d.child(
                    h_flex()
                        .px_2()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Remove # to receive only your filters?"),
                        )
                        .child(
                            Button::new("remove-hash")
                                .xsmall()
                                .label("Remove #")
                                .on_click(cx.listener(|v, _, _, cx| v.remove_sub("#", cx))),
                        ),
                )
            })
            .child(
                div()
                    .px_2()
                    .child(Input::new(&self.sub_input).small().suffix(qos)),
            )
            .when_some(self.sub_error.clone(), |d, e| {
                d.child(div().px_2().text_xs().text_color(theme.danger).child(e))
            })
    }

    /// The rows of the subscription list. `SubList` draws it.
    fn render_sub_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        uniform_list(
            "subs",
            self.subs.rows.len(),
            cx.processor(|v, range: std::ops::Range<usize>, _window, cx| {
                let theme = cx.theme().clone();
                range.map(|i| v.sub_row(i, &theme, cx)).collect()
            }),
        )
        .track_scroll(&self.subs_scroll)
        .size_full()
        // The inset is inside the list: the wheel region reaches the pane edge, as the bar does.
        .px_2()
    }

    /// Changes when the subscription list must draw again: rows or their states, the shown
    /// filter, the connection (the "Waiting" text) or the theme colours that the rows use.
    fn sub_list_sig(&self, theme: &Theme) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            self.subs.rev,
            &self.show_only,
            self.connected(),
            self.composer_protocol() == Protocol::V5,
            self.rem.to_bits(),
            f32::from(self.subs_scroll.0.borrow().base_handle.offset().y).to_bits(),
            theme.theme_name(),
        )
            .hash(&mut h);
        for c in [
            theme.accent,
            theme.accent_foreground,
            theme.muted_foreground,
            theme.danger,
            theme.chart_1,
        ] {
            [c.h, c.s, c.l, c.a].map(f32::to_bits).hash(&mut h);
        }
        h.finish()
    }

    fn filters_active(&self) -> bool {
        match self.view {
            View::Messages => *self.store.list_filter() != ListFilter::default(),
            View::Tree => *self.store.tree_filter() != TreeFilter::default(),
        }
    }

    /// The centre header: view switch, filter, direction menu and pause.
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let paused = self.store.is_paused();
        let messages = self.view == View::Messages;
        let direction = self.direction;
        let weak = cx.entity().downgrade();
        h_flex()
            .flex_wrap()
            .items_center()
            .gap_x_2()
            .gap_y_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div().flex_none().w(ui(150.)).child(
                    TabBar::new("view")
                        .segmented()
                        .small()
                        .selected_index(usize::from(!messages))
                        .on_click(cx.listener(|v, ix: &usize, window, cx| {
                            let view = if *ix == 0 { View::Messages } else { View::Tree };
                            v.set_view(view, window, cx)
                        }))
                        .child("Messages")
                        .child("Tree"),
                ),
            )
            .child(
                div().flex_1().min_w(ui(160.)).child(
                    Input::new(&self.filter_input)
                        .small()
                        .prefix(IconName::Search),
                ),
            )
            .when(messages, |d| {
                d.child(
                    Button::new("direction")
                        .small()
                        .ghost()
                        .label(match direction {
                            DirectionFilter::All => "All",
                            DirectionFilter::Received => "Received",
                            DirectionFilter::Sent => "Sent",
                        })
                        .dropdown_caret(true)
                        .tooltip("Show received or sent messages")
                        .dropdown_menu(move |menu, _, _| {
                            [
                                ("All", DirectionFilter::All),
                                ("Received", DirectionFilter::Received),
                                ("Sent", DirectionFilter::Sent),
                            ]
                            .into_iter()
                            .fold(menu, |menu, (label, d)| {
                                let weak = weak.clone();
                                menu.item(
                                    PopupMenuItem::new(label).checked(direction == d).on_click(
                                        move |_, _, cx| {
                                            weak.update(cx, |v, cx| v.set_direction(d, cx)).ok();
                                        },
                                    ),
                                )
                            })
                        }),
                )
            })
            .when(paused, |d| {
                d.child(div().text_xs().text_color(theme.warning).child(format!(
                    "Paused · {} new",
                    group_digits(self.store.paused_new())
                )))
            })
            .child(
                Button::new("pause")
                    .small()
                    .ghost()
                    .icon(if paused {
                        IconName::Play
                    } else {
                        IconName::Pause
                    })
                    .label(if paused { "Resume" } else { "Pause" })
                    .on_click(cx.listener(|v, _, _, cx| v.toggle_pause(cx))),
            )
    }

    /// Active filters as small chips, each with a clear button. The text filter has its own
    /// input, so it has no chip.
    fn render_chips(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let chips = chip_labels(self.show_only.as_deref(), self.direction, self.view);
        if chips.is_empty() {
            return None;
        }
        let theme = cx.theme();
        Some(
            h_flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .children(chips.into_iter().map(|c| {
                    // "Show only" is the first chip when it is set. The direction is the other.
                    let is_only = c.starts_with("Only: ");
                    h_flex()
                        .gap_0p5()
                        .child(
                            Tag::secondary()
                                .small()
                                .max_w(ui(260.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(c),
                        )
                        .child(
                            Button::new(("clear-chip", usize::from(is_only)))
                                .xsmall()
                                .ghost()
                                .icon(IconName::Close)
                                .tooltip("Clear this filter")
                                .on_click(cx.listener(move |v, _, _, cx| {
                                    if is_only {
                                        v.show_only = None;
                                        v.apply_filters(cx);
                                    } else {
                                        v.set_direction(DirectionFilter::All, cx);
                                    }
                                })),
                        )
                }))
                .into_any_element(),
        )
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.profile.is_none() {
            return self.render_no_profile(cx);
        }
        if self.filters_active() {
            let (title, what) = match self.view {
                View::Messages => (
                    "No messages match the filters",
                    "Change or clear the filters.",
                ),
                View::Tree => (
                    "No topics match the filters",
                    "Change or clear the filters.",
                ),
            };
            return empty_state(
                title,
                what,
                [Button::new("clear-filters-empty")
                    .small()
                    .label("Clear filters")
                    .on_click(cx.listener(|v, _, window, cx| v.clear_filters(window, cx)))
                    .into_any_element()],
            );
        }
        if self.connected() {
            let focus_input = |label: &'static str, cx: &mut Context<Self>| {
                Button::new("focus-subscribe-empty")
                    .small()
                    .label(label)
                    .on_click(cx.listener(|v, _, window, cx| {
                        v.sub_input.update(cx, |i, cx| i.focus(window, cx))
                    }))
                    .into_any_element()
            };
            match subs_empty(&self.subs) {
                Some(SubsEmpty::NoSubscriptions) => {
                    return empty_state(
                        "No subscriptions",
                        "Subscribe to a topic filter to see messages.",
                        [focus_input("Add a topic filter", cx)],
                    );
                }
                Some(SubsEmpty::HashRejected) => {
                    return empty_state(
                        "The broker does not allow #.",
                        "Subscribe to a narrower filter.",
                        [focus_input("Add a topic filter", cx)],
                    );
                }
                None => {}
            }
            return empty_state(
                "No messages yet",
                "Messages show here when the broker sends them.",
                [],
            );
        }
        let Some(p) = &self.profile else {
            return div().into_any_element();
        };
        let name = display_name(p);
        let connecting = self.connecting()
            || matches!(
                self.status.state,
                ConnState::Connecting | ConnState::Reconnecting { .. }
            );
        if connecting {
            return empty_state(
                &format!("Connecting to {}…", self.url()),
                "Messages show here when the connection is up.",
                [Button::new("connect-empty")
                    .small()
                    .label("Cancel")
                    .on_click(cx.listener(|v, _, window, cx| v.toggle_connection(window, cx)))
                    .into_any_element()],
            );
        }
        if self.status.state == ConnState::Disconnecting {
            return empty_state(
                &format!("Disconnecting from {name}…"),
                "Messages show here when the connection is up.",
                [],
            );
        }
        // C and D: the connection card. A failure adds its reason.
        let facts = format!(
            "{} · {} · {}",
            self.url(),
            protocol_text(p.protocol),
            if p.transport.is_encrypted() {
                "Encrypted"
            } else {
                "Not encrypted"
            }
        );
        let (what, action) = match &self.status.state {
            ConnState::Failed(why) => (format!("{facts}\n{why}"), "Try again"),
            _ => (facts, "Connect"),
        };
        empty_state(
            &name,
            &what,
            [
                Button::new("connect-empty")
                    .small()
                    .primary()
                    .label(action)
                    .on_click(cx.listener(|v, _, window, cx| v.toggle_connection(window, cx)))
                    .into_any_element(),
                self.edit_action("edit-empty", cx),
            ],
        )
    }

    /// "Edit connection…", or a note for the "Command line" connection (it is not saved, so the
    /// sheet cannot edit it).
    fn edit_action(&self, id: &'static str, cx: &mut Context<Self>) -> AnyElement {
        if self.session.as_ref().map(|s| s.id) == self.profile.as_ref().map(|p| p.id) {
            return div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Command line: not saved")
                .into_any_element();
        }
        Button::new(id)
            .small()
            .label("Edit connection…")
            .on_click(cx.listener(|v, _, window, cx| v.open_sheet(None, false, window, cx)))
            .into_any_element()
    }

    /// The centre pane: header, filter chips and the active view. The composer is added by the
    /// caller.
    fn render_list_area(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.view {
            View::Messages => self.render_messages(cx),
            View::Tree => self.render_tree(cx),
        };
        let has_rows = match self.view {
            View::Messages => self.store.list_len() > 0,
            View::Tree => !self.store.rows().is_empty(),
        };
        let ring = cx.theme().ring;
        let jump = self.view == View::Messages
            && self.store.list_len() > 0
            && self.follow.jump_visible(self.store.is_paused());
        v_flex()
            .flex_1()
            .min_h_0()
            .track_focus(&self.list_focus)
            .key_context(LIST_CONTEXT)
            // Keyboard focus shows also before a row is selected. The border is always there,
            // so that the focus does not move the rows.
            .border_2()
            .border_color(cx.theme().transparent)
            .focus_visible(|s| s.border_color(ring))
            .on_action(cx.listener(|v, _: &SelectPrev, _, cx| v.step_selection(-1, cx)))
            .on_action(cx.listener(|v, _: &SelectNext, _, cx| v.step_selection(1, cx)))
            .on_action(cx.listener(|v, _: &ExpandNode, _, cx| v.expand_selected(true, cx)))
            .on_action(cx.listener(|v, _: &CollapseNode, _, cx| v.expand_selected(false, cx)))
            .on_action(cx.listener(|v, _: &OpenDetails, window, cx| v.enter_on_row(window, cx)))
            // A scroll that leaves the top turns Follow off. The list handles the wheel event
            // first, so the offset is the new one here.
            .on_scroll(cx.listener(|v, _, _, cx| {
                if v.list_scrolled() {
                    cx.notify();
                }
            }))
            // The button has its own strip: it never covers a row.
            .when(jump, |d| {
                d.child(
                    h_flex()
                        .key_context("JumpStrip")
                        .flex_none()
                        .justify_center()
                        .py_1()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(
                            Button::new("jump-to-newest")
                                .small()
                                .icon(IconName::ArrowUp)
                                .label(jump_label(self.store.watched_new()))
                                .on_click(
                                    cx.listener(|v, _, window, cx| v.jump_to_newest(window, cx)),
                                ),
                        ),
                )
            })
            .children(self.failure_banner(has_rows, cx))
            .child(body)
    }

    /// A failed connection with messages on screen: the empty state does not show, so a banner
    /// above the list gives the reason and the next steps.
    fn failure_banner(&self, has_rows: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ConnState::Failed(why) = &self.status.state else {
            return None;
        };
        if self.banner_closed || !has_rows || self.connecting() {
            return None;
        }
        let name = self.profile.as_ref().map(display_name).unwrap_or_default();
        let theme = cx.theme();
        Some(
            h_flex()
                .flex_none()
                .gap_2()
                .px_3()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .small()
                        .text_color(theme.danger),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(format!("{name}: {why}")),
                )
                .child(
                    Button::new("retry-banner")
                        .small()
                        .primary()
                        .label("Try again")
                        .on_click(cx.listener(|v, _, window, cx| v.toggle_connection(window, cx))),
                )
                .child(self.edit_action("edit-banner", cx))
                .child(
                    Button::new("close-banner")
                        .small()
                        .ghost()
                        .icon(IconName::Close)
                        .tooltip("Close")
                        .on_click(cx.listener(|v, _, _, cx| {
                            v.banner_closed = true;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    fn render_messages(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.store.list_len();
        if count == 0 {
            return self.render_empty(cx);
        }
        self.list_scrolled();
        if self.follow.on && !self.store.is_paused() {
            self.list_scroll
                .scroll_to_item_strict(0, ScrollStrategy::Top);
        }
        let list = uniform_list(
            "messages",
            count,
            cx.processor(|v, range: std::ops::Range<usize>, window, cx| {
                let theme = cx.theme().clone();
                let focused = v.list_focus.contains_focused(window, cx);
                let mut rows = Vec::with_capacity(range.len());
                for i in range {
                    rows.push(match v.store.list_row(i) {
                        Some(RowView::Message {
                            id,
                            node,
                            topic,
                            msg,
                            status,
                            ..
                        }) => RowData::Msg {
                            id,
                            node,
                            topic: topic.to_string().into(),
                            time: msg.received_at,
                            retain: msg.retain,
                            // Built for the visible rows only.
                            preview: preview(&msg.payload, 240).into(),
                            sent: status.cloned(),
                            colour: None,
                        },
                        Some(RowView::Reconnected { id, at }) => RowData::Reconnected(id, at),
                        Some(RowView::Removed { id }) => RowData::Removed(id),
                        Some(RowView::RemovedMarker) => RowData::RemovedMarker,
                        None => continue,
                    });
                }
                for row in &mut rows {
                    if let RowData::Msg {
                        node,
                        topic,
                        colour,
                        ..
                    } = row
                    {
                        *colour = match node {
                            Some(n) => v.store.colour_of(*n),
                            None => v.store.colour_of_topic(topic),
                        };
                    }
                }
                rows.into_iter()
                    .map(|row| v.message_row(row, &theme, focused, cx))
                    .collect()
            }),
        )
        .track_scroll(&self.list_scroll)
        .size_full();
        // The bar sits at the trailing edge of the list region.
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(list)
            .vertical_scrollbar(&self.list_scroll)
            .into_any_element()
    }

    /// One row of the message list. Its height is fixed, so that the list can be virtual.
    fn message_row(
        &self,
        row: RowData,
        theme: &Theme,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let height = ui(if self.two_lines() { ROW_H2 } else { ROW_H1 });
        let mono = theme.mono_font_family.clone();
        let muted = theme.muted_foreground;
        match row {
            RowData::Msg {
                id,
                node: _,
                topic,
                time,
                retain,
                preview,
                sent,
                colour,
            } => {
                let selected = self.sel_msg.as_ref().is_some_and(|s| s.id == id);
                let look = row_look(theme, selected, focused);
                // Muted text stays readable on the strong selected colour.
                let muted = look.muted.unwrap_or(muted);
                // The subscription colour is a thin edge, and only with more than one
                // subscription.
                let edge = (self.subs.rows.len() > 1)
                    .then(|| colour.map(|c| chip_colour(c, theme)))
                    .flatten();
                let topic = div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_family(mono.clone())
                    .text_sm()
                    .map(|d| {
                        if self.two_lines() {
                            d.flex_1()
                        } else {
                            d.max_w(ui(320.))
                        }
                    })
                    .child(safe(&topic));
                let result = sent.as_ref().and_then(row_result).map(|text| {
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(if status_failed(sent.as_ref().expect("sent")) {
                            theme.danger
                        } else {
                            muted
                        })
                        .child(text)
                });
                let preview_el = || {
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .font_family(mono.clone())
                        .text_xs()
                        .text_color(muted)
                        .child(preview.clone())
                };
                let line = h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .when(sent.is_some(), |d| {
                        d.child(Icon::new(IconName::ArrowUp).xsmall().text_color(muted))
                    })
                    .child(topic)
                    .when(!self.two_lines(), |d| d.child(preview_el().flex_1()))
                    .when(retain, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .child("R"),
                        )
                    })
                    .children(result)
                    .child(
                        div()
                            .flex_none()
                            .font_family(mono.clone())
                            .text_xs()
                            .text_color(muted)
                            .child(format_time(time, self.utc_offset, true)),
                    );
                let content = v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .pl_2()
                    .pr_3()
                    .justify_center()
                    .child(line)
                    .when(self.two_lines(), |d| d.child(preview_el().w_full()));
                h_flex()
                    .id(("msg", id.0 as usize))
                    .h(height)
                    .w_full()
                    .child(
                        div()
                            .flex_none()
                            .w(px(2.))
                            .h_full()
                            .when_some(edge, |d, c| d.bg(c)),
                    )
                    .child(content)
                    .when_some(look.bg, |d, c| d.bg(c))
                    .when_some(look.fg, |d, c| d.text_color(c))
                    .hover(move |d| look.hover_bg.map_or(d.clone(), |c| d.bg(c)))
                    .on_click(cx.listener(move |v, _, window, cx| {
                        v.list_focus.focus(window, cx);
                        v.select_row(id, cx);
                        // A click opens the sheet. The keys (Up, Down) must keep the list focus.
                        v.want_sheet = v.narrow;
                    }))
                    .into_any_element()
            }
            RowData::Reconnected(id, at) => h_flex()
                .id(("msg", id.0 as usize))
                .h(height)
                .px_2()
                .items_center()
                .border_y_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(muted)
                .child(format!(
                    "Reconnected {}",
                    format_time(at, self.utc_offset, false)
                ))
                .into_any_element(),
            RowData::Removed(id) => h_flex()
                .id(("msg", id.0 as usize))
                .h(height)
                .px_2()
                .items_center()
                .text_xs()
                .text_color(muted)
                .opacity(0.6)
                .child("Removed (memory limit)")
                .into_any_element(),
            RowData::RemovedMarker => h_flex()
                .id("removed-marker")
                .h(height)
                .px_2()
                .items_center()
                .text_xs()
                .text_color(theme.warning)
                .child("Older messages were removed (memory limit)")
                .into_any_element(),
        }
    }

    fn render_tree(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.store.rows().len();
        if count == 0 {
            return self.render_empty(cx);
        }
        let list = uniform_list(
            "topics",
            count,
            cx.processor(|v, range: std::ops::Range<usize>, window, cx| {
                let theme = cx.theme().clone();
                let focused = v.list_focus.contains_focused(window, cx);
                let generation = v.gate.generation();
                let ids: Vec<NodeId> = v.store.rows()[range].to_vec();
                ids.into_iter()
                    .map(|id| {
                        // A paused snapshot can hold a node that `work` removed since.
                        let Some(n) = v.store.get(id) else {
                            return div().h(ui(TREE_ROW_H)).into_any_element();
                        };
                        let chevron = n.has_children.then_some(if n.expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        });
                        // Paused: the store has the preview of the snapshot.
                        let preview: SharedString = match (n.preview, n.latest) {
                            (Some(p), _) => p.to_string().into(),
                            (None, Some(m)) => preview(&m.payload, 80).into(),
                            (None, None) => SharedString::default(),
                        };
                        let selected = v.sel_topic == Some(id);
                        let parent = n.has_children;
                        let look = row_look(&theme, selected, focused);
                        let muted = look.muted.unwrap_or(theme.muted_foreground);
                        h_flex()
                            .id(("row", node_key(id)))
                            .debug_selector(move || format!("tree-{}", node_key(id)))
                            .h(ui(TREE_ROW_H))
                            .w_full()
                            .pl_2()
                            .pr_3()
                            .gap_2()
                            .text_sm()
                            .when_some(look.bg, |d, c| d.bg(c))
                            .when_some(look.fg, |d, c| d.text_color(c))
                            .hover(move |d| look.hover_bg.map_or(d.clone(), |c| d.bg(c)))
                            .child(div().w(ui(n.depth as f32 * 14.)))
                            .child(
                                div().w(ui(14.)).flex_none().when_some(chevron, |d, icon| {
                                    d.child(Icon::new(icon).xsmall())
                                }),
                            )
                            .child(
                                div()
                                    .font_family(theme.mono_font_family.clone())
                                    .child(display_safe(n.name).into_owned()),
                            )
                            .when(n.subtree_topics > 0, |d| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(topics_label(n.subtree_topics)),
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                div()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_color(muted)
                                    .max_w(ui(320.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(preview),
                            )
                            .child(
                                div()
                                    .w(ui(72.))
                                    .flex()
                                    .justify_end()
                                    .font_family(theme.mono_font_family.clone())
                                    .child(n.subtree_msgs.to_string()),
                            )
                            .on_click(cx.listener(move |v, ev: &ClickEvent, window, cx| {
                                v.list_focus.focus(window, cx);
                                v.select(id, generation, ev.click_count() == 2, cx);
                                // As in `select`: a topic with subtopics opens no sheet.
                                v.want_sheet = v.narrow && !parent;
                            }))
                            .into_any_element()
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.tree_scroll)
        .size_full();
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(list)
            .vertical_scrollbar(&self.tree_scroll)
            .into_any_element()
    }

    /// "used / limit". A click opens the list of limits (the same ones as in the palette).
    /// Width limits of the panes in pixels: (initial, least, most) for the two side panes, and
    /// the least of the centre. The saved widths are clamped to them, and the sides never take
    /// the room that the centre needs. Rems give the limits, so that they follow the zoom.
    fn pane_limits(&self, window: &Window) -> PaneLimits {
        let rem = window.rem_size();
        let to_px = |v: f32| ui(v).to_pixels(rem);
        let total = window.viewport_size().width;
        let centre = to_px(CENTRE_MIN);
        let saved = |i: usize| self.settings.ui.pane_widths.get(i).copied();
        let left_max =
            to_px(LEFT_W.2).min((total - centre - to_px(RIGHT_W.1)).max(to_px(LEFT_W.1)));
        let left_min = to_px(LEFT_W.1);
        let left = saved(0)
            .filter(|w| w.is_finite())
            .map_or(to_px(LEFT_W.0), px)
            .clamp(left_min, left_max);
        let right_min = to_px(RIGHT_W.1);
        // The same rule as the left pane: the design maximum, or less when the centre needs
        // the room. Expand uses its own, wider target (`resize_panel` does not save it).
        let right_max = to_px(RIGHT_W.2)
            .min((total - left - centre).max(right_min))
            .max(right_min);
        let right = saved(1)
            .filter(|w| w.is_finite())
            .map_or(to_px(RIGHT_W.0), px)
            .clamp(
                right_min,
                (total - left - centre).clamp(right_min, right_max),
            );
        PaneLimits {
            left: (left, left_min, left_max),
            right: (right, right_min, right_max),
            centre,
        }
    }

    /// The user dragged a handle. Save the side widths. A drag ends "Expand".
    fn panes_resized(&mut self, sizes: &[Pixels]) {
        let w = |i: usize| sizes.get(i).map(|s| f32::from(*s));
        let saved = &self.settings.ui.pane_widths;
        let left = w(0).or_else(|| saved.first().copied());
        // The details panel keeps its last size while it is hidden.
        let right = w(2).or_else(|| saved.get(1).copied());
        if let (Some(l), Some(r)) = (left, right) {
            self.settings.ui.pane_widths = vec![l.round(), r.round()];
            self.expanded = false;
            self.expand_restore = None;
            self.persist();
        }
    }

    /// Puts the side panes at their default widths. The widths are no longer saved.
    fn reset_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.ui.pane_widths.clear();
        self.persist();
        let limits = self.pane_limits(window);
        self.panes.update(cx, |s, cx| {
            s.resize_panel(0, limits.left.0, window, cx);
            s.resize_panel(2, limits.right.0, window, cx);
        });
        cx.notify();
    }

    /// The memory limit of the store now, in MiB.
    pub(super) fn memory_mib(&self) -> u32 {
        u32::try_from(self.store.budget() / MIB).unwrap_or(u32::MAX)
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = self.store.stats();
        let drops = s.drops
            + s.topic_drops
            + self
                .net
                .stats
                .drops
                .load(std::sync::atomic::Ordering::Relaxed);
        let details_button = self.details_button(drops, cx);
        let show = status_items(&StatusFacts {
            encrypted: self.profile.as_ref().map(|p| p.transport.is_encrypted()),
            verify_tls: self.profile.as_ref().is_none_or(|p| p.verify_tls),
            drops,
            list_drops: s.list_drops,
            used: s.used_bytes,
            budget: self.store.budget(),
            reconnect: self.reconnect_pending && self.is_live(),
        });
        let theme = cx.theme();
        let mut left = h_flex().gap_3().text_xs().child(self.url());
        if let Some(text) = show.security {
            // The warning fill, with the theme text colour that reads best on it.
            let fill = theme.warning;
            let text_colour = readable_on(fill, theme.foreground, theme.background);
            left = left.child(Tag::custom(fill, text_colour, fill).xsmall().child(text));
        }
        if show.reconnect {
            let id = self.profile.as_ref().map(|p| p.id);
            left = left.child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme.warning)
                            .child("Reconnect to apply your changes."),
                    )
                    .child(
                        Button::new("reconnect")
                            .xsmall()
                            .label("Reconnect")
                            .on_click(cx.listener(move |v, _, window, cx| {
                                if let Some(id) = id {
                                    v.request_switch(id, true, window, cx);
                                }
                            })),
                    ),
            );
        }
        // Mono is for the numbers only. The words stay in the UI font.
        let mono = theme.mono_font_family.clone();
        let num = move |t: String| div().font_family(mono.clone()).child(t);
        StatusBar::new().left(left).right(
            h_flex()
                .gap_3()
                .text_xs()
                .child(
                    h_flex()
                        .gap_1()
                        .child(num(self.rate.per_sec.to_string()))
                        .child("msg/s"),
                )
                .when(show.memory, |d| {
                    let (limit, unit) = crate::palette::memory_text(self.memory_mib())
                        .split_once(' ')
                        .map(|(n, u)| (n.to_string(), u.to_string()))
                        .unwrap_or_default();
                    d.child(
                        h_flex()
                            .gap_1()
                            .text_color(theme.warning)
                            .child(num((s.used_bytes / MIB).to_string()))
                            .child("MiB /")
                            .child(num(limit))
                            .child(unit),
                    )
                })
                .when(s.list_drops > 0 && show.list_drops, |d| {
                    d.child(
                        h_flex()
                            .gap_1()
                            .child(num(s.list_drops.to_string()))
                            .child("rows removed (memory limit)"),
                    )
                })
                .when(show.drops, |d| {
                    d.child(
                        h_flex()
                            .gap_1()
                            .text_color(theme.danger)
                            .child(num(drops.to_string()))
                            .child("dropped"),
                    )
                })
                .child(details_button),
        )
    }
}

/// The facts that decide which optional parts of the status bar show.
struct StatusFacts {
    /// `Some(false)`: the transport is not encrypted. `None`: no profile.
    encrypted: Option<bool>,
    verify_tls: bool,
    drops: u64,
    list_drops: u64,
    used: usize,
    budget: usize,
    /// The active profile changed while the connection is live.
    reconnect: bool,
}

/// What the status bar shows besides the URL, msg/s and Details (design 3).
#[derive(Debug, PartialEq, Eq)]
struct StatusItems {
    security: Option<&'static str>,
    drops: bool,
    list_drops: bool,
    memory: bool,
    reconnect: bool,
}

fn status_items(f: &StatusFacts) -> StatusItems {
    StatusItems {
        security: match f.encrypted {
            Some(false) => Some("Not encrypted"),
            Some(true) if !f.verify_tls => Some("Insecure TLS"),
            _ => None,
        },
        drops: f.drops > 0,
        list_drops: f.list_drops > 0,
        // Above 80% of the budget.
        memory: f.used.saturating_mul(5) > f.budget.saturating_mul(4),
        reconnect: f.reconnect,
    }
}

impl Render for Navigator {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.metrics.frame_start(window);
        self.arm_frame(window, cx);
        self.rem = f32::from(window.rem_size());
        // Key bindings of the app need a focused control inside the root. With no focus (the
        // window starts inactive, or a click on an empty area), the root takes it.
        if window.focused(cx).is_none() {
            let root = self.root_focus.clone();
            cx.defer_in(window, move |_, window, cx| {
                if window.focused(cx).is_none() {
                    root.focus(window, cx);
                }
            });
        }
        self.sync_subs();
        // A notify during the draw does not schedule a frame, so it goes out after the draw.
        let sig = self.sub_list_sig(cx.theme());
        if sig != self.sub_list_sig {
            self.sub_list_sig = sig;
            let list = self.sub_list.clone();
            cx.defer_in(window, move |_, _, cx| list.update(cx, |_, cx| cx.notify()));
        }
        if self.resize_settled_at.is_none() {
            self.resize_settled_at = Some(Instant::now() + RESIZE_SETTLE);
            self.mark_healthy(cx);
            let s = window.viewport_size();
            self.last_size = (f32::from(s.width), f32::from(s.height));
        }
        // Below NARROW px the details open as a sheet. A wide window shows them in the pane.
        // The sheet closes in this frame when the window grows.
        self.narrow = needs_sheet(f32::from(window.viewport_size().width), self.rem);
        if self.narrow && self.want_sheet {
            self.want_sheet = false;
            cx.defer_in(window, |v, window, cx| v.open_details_sheet(window, cx));
        } else if !self.narrow && self.sheet_open {
            self.sheet_open = false;
            cx.defer_in(window, |_, window, cx| {
                // `close_sheet` moves the focus to the list: not from under an open dialog.
                let held = window
                    .has_active_dialog(cx)
                    .then(|| window.focused(cx))
                    .flatten();
                window.close_sheet(cx);
                if let Some(h) = held {
                    h.focus(window, cx);
                }
            });
        }
        let chips = self.render_chips(cx);
        let show_details = self.details_open && !self.narrow;
        // The pane is gone (a resize hid it) and a control of it holds the focus: the root takes
        // it. The sheet keeps its own focus. After the draw, `is_rendered` tells if the handle is
        // in the frame without the pane.
        if let (false, Some(held)) = (show_details || self.sheet_open, window.focused(cx)) {
            cx.defer_in(window, move |v, window, cx| {
                if window.focused(cx).as_ref() == Some(&held) && !held.is_rendered(window) {
                    v.root_focus.focus(window, cx);
                }
            });
        }
        // The pane keeps its last frame until an input of it changes (see `sync_details`).
        self.sync_details(cx);
        let details = show_details.then(|| {
            self.pane
                .clone()
                .cached(StyleRefinement::default().size_full())
        });
        if std::mem::take(&mut self.pane_reset) {
            cx.defer_in(window, |v, window, cx| v.reset_panes(window, cx));
        }
        let limits = self.pane_limits(window);
        let theme = cx.theme();
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .key_context(CONTEXT)
            .track_focus(&self.root_focus)
            .on_action(
                cx.listener(|v, _: &ShowMessages, w, cx| v.run_cmd(Cmd::ViewMessages, w, cx)),
            )
            .on_action(cx.listener(|v, _: &ShowTree, w, cx| v.run_cmd(Cmd::ViewTree, w, cx)))
            .on_action(cx.listener(|v, _: &FocusFilter, w, cx| v.run_cmd(Cmd::FocusFilter, w, cx)))
            .on_action(cx.listener(|v, _: &OpenPalette, w, cx| v.open_palette(w, cx)))
            .on_action(
                cx.listener(|v, _: &NewConnection, w, cx| v.run_cmd(Cmd::NewConnection, w, cx)),
            )
            .on_action(cx.listener(|v, _: &SendMessage, w, cx| v.run_cmd(Cmd::Send, w, cx)))
            .on_action(cx.listener(|v, _: &NewMessage, w, cx| v.run_cmd(Cmd::NewMessage, w, cx)))
            .on_action(cx.listener(|_, _: &FocusNextField, window, cx| window.focus_next(cx)))
            .on_action(cx.listener(|_, _: &FocusPrevField, window, cx| window.focus_prev(cx)))
            .child(self.render_title(cx))
            .child(
                div().flex_1().min_h_0().w_full().child(
                    h_resizable("panes")
                        .with_state(&self.panes)
                        .on_resize({
                            let weak = cx.entity().downgrade();
                            move |state, _, cx| {
                                let sizes = state.read(cx).sizes().clone();
                                weak.update(cx, |v, _| v.panes_resized(&sizes)).ok();
                            }
                        })
                        .child(
                            resizable_panel()
                                .size(limits.left.0)
                                .size_range(limits.left.1..limits.left.2)
                                .fixed()
                                .child(self.render_subs(cx)),
                        )
                        .child(
                            resizable_panel()
                                .size_range(limits.centre..Pixels::MAX)
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .h_full()
                                        .child(self.render_header(cx))
                                        .children(chips)
                                        .child(self.render_list_area(cx))
                                        .child(self.render_composer(window, cx)),
                                ),
                        )
                        .child(
                            resizable_panel()
                                .size(limits.right.0)
                                // Expanded: the pane may take the room up to the centre
                                // minimum; a drag ends Expand and is capped again.
                                .size_range(
                                    limits.right.1..if self.expanded {
                                        Pixels::MAX
                                    } else {
                                        limits.right.2
                                    },
                                )
                                .fixed()
                                .visible(show_details)
                                .children(details),
                        ),
                ),
            )
            .child(
                div()
                    .debug_selector(|| "status-bar".into())
                    .child(self.render_status(cx)),
            )
    }
}

/// Pane widths in pixels: (initial, least, most) of the left and the right pane, and the least
/// of the centre.
struct PaneLimits {
    left: (Pixels, Pixels, Pixels),
    right: (Pixels, Pixels, Pixels),
    centre: Pixels,
}

/// The subscription rows of the main view. Own view, so that gpui can reuse its last frame.
struct SubList {
    app: WeakEntity<Navigator>,
}

impl Render for SubList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.app.upgrade() {
            Some(app) => app.update(cx, |v, cx| v.render_sub_list(cx).into_any_element()),
            None => div().into_any_element(),
        }
    }
}

/// An empty state: a title, one sentence and the actions (one of them at most is primary).
pub(crate) fn empty_state(
    title: &str,
    description: &str,
    actions: impl IntoIterator<Item = AnyElement>,
) -> AnyElement {
    let mut content = EmptyContent::new();
    let mut has_action = false;
    for a in actions {
        content = content.child(a);
        has_action = true;
    }
    Empty::new()
        .m_3()
        .header(
            EmptyHeader::new()
                .title(EmptyTitle::new().child(title.to_string()))
                .description(EmptyDescription::new().child(description.to_string())),
        )
        .when(has_action, |e| e.content(content))
        .into_any_element()
}

/// The items of the `⋯` menu of a subscription row.
fn sub_menu_items(
    menu: gpui_kit::component::menu::PopupMenu,
    weak: &WeakEntity<Navigator>,
    filter: &SharedString,
    qos: Qos,
    state: &SubState,
    active: bool,
    v5: bool,
) -> gpui_kit::component::menu::PopupMenu {
    let item =
        |label: &'static str,
         run: fn(&mut Navigator, &str, &mut Window, &mut Context<Navigator>)| {
            let (weak, filter) = (weak.clone(), filter.clone());
            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                weak.update(cx, |v, cx| run(v, &filter, window, cx)).ok();
            })
        };
    let removing = *state == SubState::Removing;
    let locked = state.removal_accepted();
    let retry_remove = matches!(state, SubState::RemoveFailed(_) | SubState::NotRemoved(_));
    let mut menu =
        menu.item(item("Show only", |v, f, _, cx| v.toggle_show_only(f, cx)).checked(active));
    menu = menu.separator();
    menu = menu.item(
        item("QoS 0", |v, f, _, cx| v.set_sub_qos(f, Qos::AtMostOnce, cx))
            .checked(qos == Qos::AtMostOnce)
            .disabled(locked),
    );
    menu = menu.item(
        item("QoS 1", |v, f, _, cx| {
            v.set_sub_qos(f, Qos::AtLeastOnce, cx)
        })
        .checked(qos == Qos::AtLeastOnce)
        .disabled(locked),
    );
    if v5 {
        menu = menu
            .item(item("Options…", |v, f, w, cx| v.open_sub_options(f, w, cx)).disabled(locked));
    }
    if matches!(state, SubState::Failed(_)) {
        menu = menu.item(item("Retry", |v, f, _, cx| v.retry_sub(f, cx)));
    } else if retry_remove {
        menu = menu.item(item("Retry remove", |v, f, _, cx| v.remove_sub(f, cx)));
    }
    menu.separator()
        .item(item("Remove", |v, f, _, cx| v.remove_sub(f, cx)).disabled(removing))
}

/// A stable number for the element id of a tree row. `NodeId` keeps its fields private, and the
/// default hasher has fixed keys, so the same id gives the same number.
fn node_key(id: NodeId) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

/// Colour of a subscription chip. The colours repeat after ten subscriptions.
fn chip_colour(index: usize, theme: &Theme) -> Hsla {
    match colour_slot(index) {
        0 => theme.chart_1,
        1 => theme.chart_2,
        2 => theme.chart_3,
        3 => theme.chart_4,
        4 => theme.chart_5,
        5 => theme.blue,
        6 => theme.green,
        7 => theme.yellow,
        8 => theme.magenta,
        _ => theme.cyan,
    }
}

/// Opt-in performance log (`MQTT_NAVIGATOR_METRICS=1`), written to stderr once a second while
/// messages arrive. It adds no timers: it logs from the merge path only.
struct Metrics {
    on: bool,
    merge_ms: Vec<f32>,
    /// Time from the start of `render` to the end of that frame.
    frame_ms: std::rc::Rc<std::cell::RefCell<Vec<f32>>>,
    last: Instant,
    first_frame_logged: bool,
    /// Counters at the last log line, for the rates.
    prev: IngestCounts,
    /// Lines about allocations above the frame budget, written with the next log line.
    slow: Vec<String>,
}

/// The net queue as the merge sees it.
struct QueueView {
    msgs: u64,
    bytes: usize,
    /// Messages that the net thread dropped because the queue was full.
    net_drops: u64,
}

impl Metrics {
    fn from_env() -> Self {
        Self {
            on: std::env::var_os("MQTT_NAVIGATOR_METRICS").is_some(),
            merge_ms: Vec::new(),
            frame_ms: Default::default(),
            last: Instant::now(),
            first_frame_logged: false,
            prev: IngestCounts::default(),
            slow: Vec::new(),
        }
    }

    /// Records the time of one allocation. `parts` tells where an allocation above the frame
    /// budget spent its time (the log shows one line for it).
    fn merge(
        &mut self,
        d: Duration,
        parts: impl FnOnce() -> (ingest::Plan, ingest::Timing, Duration, Duration),
    ) {
        if self.on {
            self.merge_ms.push(d.as_secs_f32() * 1000.);
            if d > Duration::from_millis(4) {
                let (p, t, a_in, a_out) = parts();
                let ms = |d: Duration| d.as_secs_f32() * 1000.;
                self.slow.push(format!(
                    "metrics slow_merge total={:.2}ms plan_ingest={:.2}ms plan_work={:.2}ms \
                     anchor_in={:.2}ms ingest={:.2}ms messages={} work={:.2}ms anchor_out={:.2}ms",
                    ms(d),
                    ms(p.ingest),
                    ms(p.work),
                    ms(a_in),
                    ms(t.ingest),
                    t.messages,
                    ms(t.work),
                    ms(a_out),
                ));
            }
        }
    }

    /// One UI-thread step that the frame timer does not see (setting text into the editor).
    fn ui_step(&self, what: &str, bytes: usize, d: Duration) {
        if self.on {
            eprintln!(
                "metrics ui_step={what} bytes={bytes} took={:.2}ms",
                d.as_secs_f32() * 1000.
            );
        }
    }

    fn frame_start(&mut self, window: &mut Window) {
        if self.on && !self.first_frame_logged {
            self.first_frame_logged = true;
            if let Some(t0) = crate::STARTED.get().copied() {
                window.on_next_frame(move |_, cx| {
                    let line = format!(
                        "metrics first_frame={:.1}ms",
                        t0.elapsed().as_secs_f32() * 1000.
                    );
                    cx.background_executor()
                        .spawn(async move { eprintln!("{line}") })
                        .detach();
                });
            }
        }
        if self.on {
            let start = Instant::now();
            let frames = self.frame_ms.clone();
            window.on_next_frame(move |_, _| {
                frames
                    .borrow_mut()
                    .push(start.elapsed().as_secs_f32() * 1000.);
            });
        }
    }

    fn due(&self, now: Instant) -> bool {
        self.on && now.duration_since(self.last) >= Duration::from_secs(1)
    }

    /// Returns one log line (without RSS). The caller writes it on a background thread.
    fn line(
        &mut self,
        rate: u64,
        drops: u64,
        s: &navigator_core::store::StoreStats,
        c: &IngestCounts,
        q: QueueView,
        now: Instant,
    ) -> String {
        /// The median, the 95th percentile and the maximum.
        fn pcts(v: &mut [f32]) -> (f32, f32, f32) {
            if v.is_empty() {
                return (0., 0., 0.);
            }
            v.sort_by(f32::total_cmp);
            let at = |p: usize| v[(v.len() - 1) * p / 100];
            (at(50), at(95), v[v.len() - 1])
        }
        let merges = self.merge_ms.len();
        let (_, mp, mm) = pcts(&mut self.merge_ms);
        let (f50, fp, fm) = pcts(&mut self.frame_ms.borrow_mut());
        let secs = now.duration_since(self.last).as_secs_f64().max(0.001);
        let per_sec = |a: u64, b: u64| (a.saturating_sub(b) as f64 / secs) as u64;
        let (dq_rate, adm_rate) = (
            per_sec(c.dequeued, self.prev.dequeued),
            per_sec(c.admitted, self.prev.admitted),
        );
        self.prev = *c;
        // `drops` is the status bar total. The next fields split it by cause.
        let line = format!(
            "metrics rate={rate}/s drops={drops} topics={} store={}MiB \
             dequeued={} admitted={} stale={} net_drops={} mem_drops={} topic_drops={} \
             list_drops={} dequeue_rate={dq_rate}/s admit_rate={adm_rate}/s \
             backlog_merges={} queue={} queue_kib={} queue_age_max={:.1}ms \
             merges={merges} merge_p95={mp:.2}ms merge_max={mm:.2}ms frame_p50={f50:.2}ms frame_p95={fp:.2}ms frame_max={fm:.2}ms frames={}",
            s.topics,
            s.used_bytes / (1024 * 1024),
            c.dequeued,
            c.admitted,
            c.stale,
            q.net_drops,
            s.drops,
            s.topic_drops,
            s.list_drops,
            c.paced,
            q.msgs,
            q.bytes / 1024,
            c.age_max.as_secs_f32() * 1000.,
            self.frame_ms.borrow().len(),
        );
        self.merge_ms.clear();
        self.frame_ms.borrow_mut().clear();
        self.last = now;
        // The caller adds the RSS to the last line, so the slow lines go first.
        let mut out = std::mem::take(&mut self.slow);
        out.push(line);
        out.join("\n")
    }
}

/// Writes a metrics line with the RSS. Runs on a background thread (file and stderr I/O).
async fn write_metrics(line: String) {
    // Only Linux has /proc. Other platforms print "n/a": a wrong 0 would mislead.
    let rss = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or("n/a".to_string(), |pages| {
            format!("{}MiB", pages * 4096 / (1024 * 1024))
        });
    eprintln!("{line} rss={rss}");
}

/// A closed receiver, used to take the real receivers out of `Net`.
fn tokio_channel_placeholder<T>() -> tokio::sync::mpsc::Receiver<T> {
    tokio::sync::mpsc::channel(1).1
}

/// A control of the title bar. On Windows the bar is the caption of the window: a press that no
/// element stops starts the system move loop, and the loop takes the release, so a click never
/// comes. The control stops the press. The free bar still moves the window.
fn caption_control(el: impl IntoElement) -> Div {
    div()
        .flex_none()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(el)
}

// The tests find elements by `debug_selector`, which gpui has in debug builds only.
#[cfg(all(test, debug_assertions))]
#[cfg_attr(not(feature = "broker-tests"), allow(dead_code))]
mod gui_round_trip;

/// Colours of a list row. A selected row has no hover change, as in `ListItem`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct RowLook {
    bg: Option<Hsla>,
    fg: Option<Hsla>,
    /// Muted text on the selected row. `None` keeps the muted colour of the theme.
    muted: Option<Hsla>,
    hover_bg: Option<gpui_kit::component::ThemeToken>,
}

/// A selected row has two strengths. With the list focus it is strong, without it weaker
/// (`list_active`). As in `ListItem`, a theme with `active_highlight` keeps the text colour. Other
/// themes use `accent` and `accent_foreground`. `accent` is the same as `list_hover` in some
/// themes, so the strong highlight tint is the `list_active_border` colour at 0.3 alpha.
fn row_look(theme: &Theme, selected: bool, list_focused: bool) -> RowLook {
    match (selected, list_focused) {
        (false, _) => RowLook {
            bg: None,
            fg: None,
            muted: None,
            hover_bg: Some(theme.tokens.list_hover),
        },
        (true, true) if theme.list.active_highlight => RowLook {
            bg: Some(theme.list_active_border.alpha(0.3)),
            fg: None,
            // The tint lowers the contrast of the muted colour, so the muted text is the
            // foreground colour at a lower opacity.
            muted: Some(theme.foreground.alpha(0.7)),
            hover_bg: None,
        },
        (true, true) => RowLook {
            bg: Some(theme.accent),
            fg: Some(theme.accent_foreground),
            muted: Some(theme.accent_foreground.opacity(0.75)),
            hover_bg: None,
        },
        (true, false) => RowLook {
            bg: Some(theme.list_active),
            fg: None,
            muted: None,
            hover_bg: None,
        },
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the gpui glob import brings its own `test` macro.
    use super::{
        COALESCE, Caller, ConnState, ConnectionDetails, DetailsCache, GenGate, Protocol,
        REDRAW_GAP, Status, StatusFacts, StatusItems, SubState, Subs, coalesce_wait,
        connection_live, may_allocate, scroll_top_of, send_error_text, skip_redraw, status_items,
        top_row, ui, validate_filter,
    };
    use gpui_kit::{px, rems};
    use navigator_core::Qos;
    use navigator_core::net::SendError;
    use std::time::Duration;
    use std::time::Instant;

    #[test]
    fn a_selected_row_keeps_its_colour_under_hover_and_is_weaker_without_the_list_focus() {
        use super::{Theme, row_look};
        use gpui_kit::hsla;
        let mut t = Theme::default();
        t.accent = hsla(0.1, 1., 0.5, 1.);
        t.accent_foreground = hsla(0.2, 1., 0.5, 1.);
        t.list_active = hsla(0.4, 1., 0.5, 1.);
        t.list_active_border = hsla(0.5, 1., 0.5, 1.);
        t.list.active_highlight = false;
        let strong = row_look(&t, true, true);
        assert_eq!(
            (strong.bg, strong.fg),
            (Some(t.accent), Some(t.accent_foreground))
        );
        assert_eq!(strong.hover_bg, None);
        // A theme with `active_highlight`: a tint of the border colour, the text colour stays.
        t.list.active_highlight = true;
        let tint = row_look(&t, true, true);
        assert_eq!(tint.bg, Some(t.list_active_border.alpha(0.3)));
        assert_eq!(tint.muted, Some(t.foreground.alpha(0.7)));
        assert_eq!((tint.fg, tint.hover_bg), (None, None));
        // A border colour with its own alpha gets the same tint alpha.
        t.list_active_border = hsla(0.5, 1., 0.5, 0.5);
        assert_eq!(row_look(&t, true, true).bg.map(|c| c.a), Some(0.3));
        assert_ne!(tint.bg, Some(t.list_active));
        t.list.active_highlight = false;
        let weak = row_look(&t, true, false);
        assert_eq!(weak.bg, Some(t.list_active));
        assert_eq!(weak.hover_bg, None);
        assert_eq!(
            row_look(&t, false, true).hover_bg,
            Some(t.tokens.list_hover)
        );
        assert_eq!(row_look(&t, false, true).bg, None);
        assert_eq!(strong.muted, Some(t.accent_foreground.opacity(0.75)));
        assert_eq!((weak.fg, weak.muted), (None, None));
        // An unselected row looks the same with or without the list focus.
        assert_eq!(row_look(&t, false, false), row_look(&t, false, true));
    }

    #[test]
    fn the_composer_follows_the_connection_while_connected_and_the_profile_otherwise() {
        use super::{
            Protocol::{V5, V311},
            composer_protocol,
        };
        // Nothing live: the saved profile, in both directions.
        assert_eq!(composer_protocol(None, false, V311, Some(V5)), V5);
        assert_eq!(composer_protocol(None, false, V5, Some(V311)), V311);
        // A connection is active: the connection, also after a saved edit of the profile. When
        // the connection ends, the saved version applies (V311 to V5 and back).
        assert_eq!(composer_protocol(None, true, V311, Some(V5)), V311);
        assert_eq!(composer_protocol(None, false, V311, Some(V5)), V5);
        assert_eq!(composer_protocol(None, true, V5, Some(V311)), V5);
        assert_eq!(composer_protocol(None, false, V5, Some(V311)), V311);
        // No profile: the last connection.
        assert_eq!(composer_protocol(None, false, V5, None), V5);
    }

    /// The keyring read of a Connect is pending: `conn_protocol` is still the old value, and the
    /// pending profile decides, with and without an active connection, in both directions.
    /// A connected V5 session is replaced (a reconnect), and the Connect is cancelled while the
    /// keyring read waits. The old status (generation 1) is still the last one, and the gate is
    /// clear. It must not count as active, in both directions.
    #[test]
    fn a_cancelled_reconnect_leaves_no_active_connection() {
        use super::{
            ConnState, GenGate,
            Protocol::{V5, V311},
            composer_protocol, connection_active,
        };
        let mut gate = GenGate::default();
        gate.begin(1);
        assert!(connection_active(&ConnState::Connected, &gate, 1));
        gate.clear();
        // The status of generation 1 is stale now.
        assert!(!connection_active(&ConnState::Connected, &gate, 1));
        for (old, saved) in [(V5, V311), (V311, V5)] {
            let active = connection_active(&ConnState::Connected, &gate, 1);
            assert_eq!(composer_protocol(None, active, old, Some(saved)), saved);
        }
        // A new Connect (generation 2) has no status yet: it is active and uses its protocol.
        gate.begin(2);
        assert!(connection_active(&ConnState::Connected, &gate, 1));
        assert!(connection_active(&ConnState::Connecting, &gate, 2));
        assert!(!connection_active(&ConnState::Disconnected, &gate, 2));
    }

    #[test]
    fn a_connect_that_waits_for_the_keyring_uses_the_protocol_of_its_profile() {
        use super::{
            Protocol::{V5, V311},
            composer_protocol,
        };
        for active in [false, true] {
            assert_eq!(composer_protocol(Some(V5), active, V311, Some(V311)), V5);
            assert_eq!(composer_protocol(Some(V311), active, V5, Some(V5)), V311);
        }
    }

    #[test]
    fn a_connect_replaced_before_it_answers_is_never_accepted() {
        let mut g = GenGate::default();
        // Connect A (generation 1) and a Subscribe for A are queued. Nothing has come back.
        g.begin(1);
        // The user switches to B before A published anything.
        g.clear();
        // Anything of A is dropped, before and after B starts.
        assert!(!g.accept(1));
        g.begin(2);
        assert!(!g.accept(1), "status, event or message of A");
        // B works.
        assert!(g.accept(2));
        // Another switch and a manual connect of B: the older generation stays out.
        g.clear();
        assert!(!g.accept(2));
        g.begin(3);
        assert!(g.accept(3) && !g.accept(2) && !g.accept(1));
        // A start error or a reject before any Connect has generation 0.
        assert!(g.accept(0));
    }

    #[test]
    fn a_connect_without_status_counts_as_live() {
        use navigator_core::net::ConnState;
        let mut g = GenGate::default();
        let idle = ConnState::Disconnected;
        assert!(!connection_live(&idle, &g, 0));
        // Connect A is queued. The UI has no status of it yet: a save of the active profile
        // must not replace the runtime profile.
        g.begin(1);
        assert!(connection_live(&idle, &g, 0));
        // Its status arrives: the state decides.
        assert!(connection_live(&ConnState::Connecting, &g, 1));
        assert!(!connection_live(&idle, &g, 1));
        // A second Connect (a Reconnect) while the first one is Connected.
        g.begin(2);
        assert!(connection_live(&ConnState::Connected, &g, 1));
        // A cancel ends it.
        g.clear();
        assert!(!connection_live(&idle, &g, 0));
    }

    #[test]
    fn layout_sizes_follow_the_rem() {
        // At the default rem (16 px) a design pixel is a pixel. At twice the rem, it doubles.
        assert_eq!(ui(30.).to_pixels(px(16.)), px(30.));
        assert_eq!(ui(30.).to_pixels(px(32.)), px(60.));
        assert_eq!(ui(16.), rems(1.));
    }

    #[test]
    fn anchor_keeps_its_pixel_offset() {
        // Row height 40. Scrolled 85 px: row 2, with 5 px above the viewport.
        let (i, px) = top_row(85., 40.);
        assert_eq!((i, px), (2, 5.));
        // 3 new rows push the anchor to index 5. The scroll moves by 3 rows.
        assert_eq!(scroll_top_of(i + 3, px, 40.), 205.);
        assert_eq!(top_row(0., 40.), (0, 0.));
    }

    /// The rows take the options of the profile, and the profile takes only the options that
    /// differ from the default. A row whose removal was accepted writes nothing.
    #[test]
    fn subscription_rows_keep_their_options_and_write_only_non_default_ones() {
        let mut p = navigator_core::net::Profile::new();
        p.subscriptions = vec![
            ("a".into(), Qos::AtMostOnce),
            ("b".into(), Qos::AtLeastOnce),
            ("c".into(), Qos::AtMostOnce),
        ];
        let no_local = navigator_core::net::SubOptions {
            no_local: true,
            ..navigator_core::net::SubOptions::default()
        };
        let never = navigator_core::net::SubOptions {
            retain_handling: navigator_core::net::RetainHandling::Never,
            ..navigator_core::net::SubOptions::default()
        };
        p.subscription_options.insert("a".into(), no_local);
        p.subscription_options.insert("c".into(), never);
        let mut s = Subs::of_profile(Some(&p));
        let options: Vec<_> = s
            .rows
            .iter()
            .map(|r| (r.filter.to_string(), r.options))
            .collect();
        assert_eq!(
            options,
            [
                ("a".to_owned(), no_local),
                ("b".to_owned(), navigator_core::net::SubOptions::default()),
                ("c".to_owned(), never)
            ]
        );
        s.begin_remove("c");
        s.row("b").expect("row b").options = no_local;
        s.row("a").expect("row a").options = navigator_core::net::SubOptions::default();
        let mut out = navigator_core::net::Profile::new();
        s.write_to(&mut out);
        assert_eq!(
            out.subscriptions,
            [
                ("a".to_owned(), Qos::AtMostOnce),
                ("b".to_owned(), Qos::AtLeastOnce)
            ]
        );
        assert_eq!(
            out.subscription_options.into_iter().collect::<Vec<_>>(),
            [("b".to_owned(), no_local)]
        );
        assert!(Subs::of_profile(None).rows.is_empty());
    }

    fn opts(no_local: bool, rap: bool) -> navigator_core::net::SubOptions {
        navigator_core::net::SubOptions {
            no_local,
            retain_as_published: rap,
            ..Default::default()
        }
    }

    /// The UI does what `set_sub_options` does when the net thread accepts a change.
    fn accepted(s: &mut Subs, filter: &str, options: navigator_core::net::SubOptions) -> u64 {
        use super::OptionsChange;
        assert!(matches!(
            s.check_options_change(filter, options),
            OptionsChange::Go(_)
        ));
        let req = s.new_req();
        s.commit_options(filter, options, Some(req));
        req
    }

    /// A, B accepted; then A again fails to enqueue. The row keeps B and B's ID (what the net
    /// thread holds) and shows the failure. Only the result with B's ID changes it.
    #[test]
    fn a_failed_enqueue_leaves_the_row_with_what_core_holds() {
        use super::OptionsChange;
        let (a, b) = (opts(true, false), opts(false, true));
        let mut s = Subs::default();
        s.push("t", Qos::AtLeastOnce);
        let r1 = accepted(&mut s, "t", a);
        let r2 = accepted(&mut s, "t", b);
        // A again: the check passes, the send fails, the row gets only the failure text.
        assert_eq!(
            s.check_options_change("t", a),
            OptionsChange::Go(Qos::AtLeastOnce)
        );
        let failure = SubState::Failed("The queue is full.".into());
        s.row("t").expect("row").state = failure.clone();
        let row = s.row("t").expect("row");
        assert_eq!((row.options, row.req, row.state.clone()), (b, r2, failure));
        // The old result does not change the row. The result of the accepted B does.
        s.on_subscribed("t", r1, Ok(Qos::AtLeastOnce));
        assert!(matches!(
            s.row("t").expect("row").state,
            SubState::Failed(_)
        ));
        s.on_subscribed("t", r2, Ok(Qos::AtLeastOnce));
        assert_eq!(
            s.row("t").expect("row").state,
            SubState::Active(Qos::AtLeastOnce)
        );
    }

    /// A QoS change uses the same request tracking as an options change: the row takes the QoS
    /// and the ID when the net thread accepts it, and only the result with that ID decides.
    #[test]
    fn the_two_empty_states_of_the_subscriptions() {
        use super::{SubsEmpty, subs_empty};
        let mut s = Subs::default();
        assert_eq!(subs_empty(&s), Some(SubsEmpty::NoSubscriptions));
        s.push("#", Qos::AtMostOnce);
        // A pending or granted filter is not an empty state.
        assert_eq!(subs_empty(&s), None);
        s.on_subscribed("#", 0, Ok(Qos::AtMostOnce));
        assert_eq!(subs_empty(&s), None);
        // The broker rejects #.
        s.on_subscribed("#", 0, Err("Not authorized to subscribe.".into()));
        s.row("#").expect("row").state = SubState::Failed("no".into());
        assert_eq!(subs_empty(&s), Some(SubsEmpty::HashRejected));
        // Another filter that is not rejected may bring messages.
        s.push("a/#", Qos::AtMostOnce);
        assert_eq!(subs_empty(&s), None);
        s.row("a/#").expect("row").state = SubState::Failed("no".into());
        assert_eq!(subs_empty(&s), Some(SubsEmpty::HashRejected));
        // A rejected filter other than # is only a failed row, not this state.
        let mut t = Subs::default();
        t.push("a/#", Qos::AtMostOnce);
        t.row("a/#").expect("row").state = SubState::Failed("no".into());
        assert_eq!(subs_empty(&t), None);
        // A row in removal does not count: nothing is left.
        let mut u = Subs::default();
        u.push("a", Qos::AtMostOnce);
        u.begin_remove("a");
        assert_eq!(subs_empty(&u), Some(SubsEmpty::NoSubscriptions));
    }

    #[test]
    fn follow_goes_off_by_the_user_and_comes_back_by_the_jump() {
        use super::Follow;
        let mut f = Follow::default();
        assert!(f.on, "Follow is on when the list opens");
        assert!(!f.jump_visible(false));
        // A scroll or a selection turns it off once. The button shows then.
        assert!(f.leave(false));
        assert!(!f.on && f.jump_visible(false));
        assert!(
            !f.leave(false),
            "the second move does not start a new count"
        );
        // While paused the button is off.
        assert!(!f.jump_visible(true));
        f.jump();
        assert!(f.on && !f.jump_visible(false));
    }

    #[test]
    fn a_pause_keeps_the_follow_state_from_before_the_pause() {
        use super::Follow;
        let mut on = Follow::default();
        assert!(!on.leave(true), "paused: the move does not change Follow");
        assert!(on.on);
        let mut off = Follow::default();
        off.leave(false);
        assert!(!off.leave(true) && !off.on);
    }

    #[test]
    fn a_profile_change_restarts_follow_and_its_counter_together() {
        use super::Follow;
        let mut f = Follow::default();
        // Follow goes off: the store starts to count (`leave` is true once).
        assert!(f.leave(false));
        // The profile changes: a new store does not count, and Follow is on again.
        f = Follow::default();
        assert!(f.on && !f.jump_visible(false));
        // The next selection starts the count again.
        assert!(f.leave(false));
    }

    #[test]
    fn a_tag_takes_the_text_colour_that_reads_on_its_fill() {
        use super::{contrast, readable_on};
        use gpui_kit::hsla;
        let (amber, black, white) = (
            hsla(0.12, 0.9, 0.5, 1.),
            hsla(0., 0., 0.05, 1.),
            hsla(0., 0., 0.98, 1.),
        );
        // Either order of the two candidates gives the darker text on amber.
        assert_eq!(readable_on(amber, white, black), black);
        assert_eq!(readable_on(amber, black, white), black);
        assert!(contrast(amber, black) >= 4.5);
        // On a dark fill the light one wins.
        let navy = hsla(0.6, 0.6, 0.15, 1.);
        assert_eq!(readable_on(navy, black, white), white);
    }

    #[test]
    fn only_a_scroll_that_leaves_the_top_turns_follow_off() {
        use super::left_the_top;
        assert!(!left_the_top(0.0), "a wheel event at the top");
        assert!(!left_the_top(-0.2));
        assert!(left_the_top(-1.0));
        assert!(left_the_top(-300.0));
    }

    #[test]
    fn the_details_sheet_opens_when_the_three_panes_do_not_fit() {
        use super::needs_sheet;
        // 100% zoom: the least widths add up to 820 px. Only the narrow limit counts.
        assert!(needs_sheet(1099., 16.));
        assert!(!needs_sheet(1100., 16.));
        // 150% zoom: the least widths add up to 1230 px, so 1100 px is not enough.
        assert!(needs_sheet(1100., 24.));
        assert!(needs_sheet(1229., 24.));
        assert!(!needs_sheet(1230., 24.));
    }

    #[test]
    fn a_topic_count_uses_the_singular_for_one() {
        use super::topics_label;
        assert_eq!(topics_label(1), "1 topic");
        assert_eq!(topics_label(2), "2 topics");
    }

    #[test]
    fn the_jump_button_names_the_new_messages() {
        use super::jump_label;
        assert_eq!(jump_label(0), "Jump to newest");
        assert_eq!(jump_label(7), "Jump to newest (7)");
        assert_eq!(jump_label(1240), "Jump to newest (1 240)");
    }

    #[test]
    fn the_filter_hint_and_the_chips_follow_the_view() {
        use super::{View, chip_labels, filter_placeholder};
        use navigator_core::store::DirectionFilter as D;
        assert_eq!(
            filter_placeholder(View::Messages),
            "Filter by topic or payload"
        );
        assert_eq!(filter_placeholder(View::Tree), "Filter by topic");
        // No filter, no chip. The text filter never has a chip.
        assert!(chip_labels(None, D::All, View::Messages).is_empty());
        assert_eq!(chip_labels(None, D::Sent, View::Messages), ["Sent"]);
        assert_eq!(
            chip_labels(Some("a/#"), D::Received, View::Messages),
            ["Only: a/#", "Received"]
        );
        // The direction does not apply to the tree.
        assert_eq!(chip_labels(Some("a/#"), D::Sent, View::Tree), ["Only: a/#"]);
    }

    #[test]
    fn a_qos_change_is_tracked_by_its_request_id() {
        use super::QosChange;
        let o = opts(true, false);
        let mut s = Subs::default();
        s.push("t", Qos::AtMostOnce);
        accepted(&mut s, "t", o);
        let r1 = s.row("t").expect("row").req;
        // The same QoS, a missing row and a row in removal change nothing.
        assert_eq!(s.check_qos_change("t", Qos::AtMostOnce), QosChange::Ignore);
        assert_eq!(s.check_qos_change("x", Qos::AtLeastOnce), QosChange::Ignore);
        // A new QoS keeps the options for the new SUBSCRIBE.
        assert_eq!(s.check_qos_change("t", Qos::AtLeastOnce), QosChange::Go(o));
        // The send fails: the row keeps its QoS and its ID and shows the failure.
        s.row("t").expect("row").state = SubState::Failed("The queue is full.".into());
        let row = s.row("t").expect("row");
        assert_eq!((row.qos, row.req), (Qos::AtMostOnce, r1));
        // The send works: the row takes the QoS, a new ID, and waits.
        let r2 = s.new_req();
        s.commit_qos("t", Qos::AtLeastOnce, Some(r2));
        let row = s.row("t").expect("row");
        assert_eq!(
            (row.qos, row.req, row.state.clone()),
            (Qos::AtLeastOnce, r2, SubState::Pending)
        );
        // The result of the older request does not decide. The new one does.
        s.on_subscribed("t", r1, Ok(Qos::AtMostOnce));
        assert_eq!(s.row("t").expect("row").state, SubState::Pending);
        s.on_subscribed("t", r2, Ok(Qos::AtMostOnce));
        assert_eq!(
            s.row("t").expect("row").state,
            SubState::Active(Qos::AtMostOnce)
        );
        // Offline: only the row (and the profile) change.
        s.commit_qos("t", Qos::AtMostOnce, None);
        assert_eq!(s.row("t").expect("row").req, r2);
        s.begin_remove("t");
        assert_eq!(s.check_qos_change("t", Qos::AtLeastOnce), QosChange::Ignore);
    }

    #[test]
    fn a_row_shows_a_status_line_only_for_a_problem_or_a_change() {
        use super::sub_status;
        let (q0, q1) = (Qos::AtMostOnce, Qos::AtLeastOnce);
        assert_eq!(sub_status(&SubState::Active(q1), q1, true), None);
        assert_eq!(sub_status(&SubState::Active(q0), q0, true), None);
        // The broker grants less than the request.
        assert_eq!(
            sub_status(&SubState::Active(q0), q1, true),
            Some(("Granted QoS 0".into(), false))
        );
        assert_eq!(
            sub_status(&SubState::Pending, q0, true),
            Some(("Subscribing…".into(), false))
        );
        assert_eq!(
            sub_status(&SubState::Pending, q0, false),
            Some(("Waiting for connection".into(), false))
        );
        assert_eq!(
            sub_status(
                &SubState::Failed("Not authorized to subscribe.".into()),
                q0,
                true
            ),
            Some(("Not authorized to subscribe.".into(), true))
        );
    }

    /// A, B, A are all accepted: three IDs. The results of the first two do not decide the row,
    /// although the last options equal the first. A late "lost" with an old ID is ignored.
    #[test]
    fn repeated_values_are_told_apart_by_the_request_id() {
        let (a, b) = (opts(true, false), opts(false, true));
        let mut s = Subs::default();
        s.push("t", Qos::AtLeastOnce);
        let r1 = accepted(&mut s, "t", a);
        let r2 = accepted(&mut s, "t", b);
        let r3 = accepted(&mut s, "t", a);
        assert!(r1 < r2 && r2 < r3);
        s.on_subscribed("t", r1, Ok(Qos::AtLeastOnce));
        s.on_subscribed("t", r2, Ok(Qos::AtLeastOnce));
        assert_eq!(s.row("t").expect("row").state, SubState::Pending);
        s.on_subscribed("t", r3, Ok(Qos::AtMostOnce));
        assert_eq!(
            s.row("t").expect("row").state,
            SubState::Active(Qos::AtMostOnce)
        );
        s.on_subscribed("t", r1, Err("The connection closed.".into()));
        assert_eq!(
            s.row("t").expect("row").state,
            SubState::Active(Qos::AtMostOnce)
        );
        assert_eq!(s.row("t").expect("row").options, a);
    }

    /// A failed enqueue, then a reconnect: the row wants what core holds. The net thread
    /// resubscribes with the stored ID, so the resubscribe result resolves the row.
    #[test]
    fn a_failed_enqueue_then_a_reconnect_resolves_with_the_resubscribe_result() {
        let a = opts(true, false);
        let mut s = Subs::default();
        s.push("t", Qos::AtLeastOnce);
        let r1 = accepted(&mut s, "t", a);
        s.on_subscribed("t", r1, Ok(Qos::AtLeastOnce));
        // B fails to enqueue: only the failure text changes.
        s.row("t").expect("row").state = SubState::Failed("The queue is full.".into());
        // The connection is lost (Reconnecting): the row waits again.
        s.connection_reset();
        assert_eq!(s.row("t").expect("row").state, SubState::Pending);
        assert_eq!(s.row("t").expect("row").options, a);
        // The resubscribe of the net thread echoes r1 (A).
        s.on_subscribed("t", r1, Ok(Qos::AtLeastOnce));
        assert_eq!(
            s.row("t").expect("row").state,
            SubState::Active(Qos::AtLeastOnce)
        );
    }

    /// A removed filter that is added again is a new row with a new ID: results of the earlier
    /// row are ignored. A new connection sets every ID to 0 (the net thread subscribes from the
    /// profile), and an older ID is then ignored.
    #[test]
    fn an_older_row_or_connection_cannot_decide_a_new_row() {
        let mut s = Subs::default();
        s.push("t", Qos::AtLeastOnce);
        let first = s.new_req();
        s.row("t").expect("row").req = first;
        s.begin_remove("t");
        s.on_unsubscribed("t", Ok(()));
        assert!(!s.contains("t"));
        s.push("t", Qos::AtLeastOnce);
        let second = s.new_req();
        s.row("t").expect("row").req = second;
        assert_ne!(first, second);
        s.on_subscribed("t", first, Ok(Qos::AtLeastOnce));
        assert_eq!(s.row("t").expect("row").state, SubState::Pending);
        s.on_subscribed("t", second, Ok(Qos::AtLeastOnce));
        assert_eq!(
            s.row("t").expect("row").state,
            SubState::Active(Qos::AtLeastOnce)
        );
        // A new connection: ID 0 for every row.
        s.new_connection();
        assert_eq!(s.row("t").expect("row").req, 0);
        s.on_subscribed("t", second, Ok(Qos::AtLeastOnce));
        assert_eq!(s.row("t").expect("row").state, SubState::Pending);
        s.on_subscribed("t", 0, Ok(Qos::AtMostOnce));
        assert_eq!(
            s.row("t").expect("row").state,
            SubState::Active(Qos::AtMostOnce)
        );
    }

    /// An edit with no connection changes the row only: the ID stays. The next Connect reads
    /// the options from the rows through `write_to`: they reach the profile, and the core test
    /// `v5_subscribe_sends_the_options_byte` shows that the first SUBSCRIBE of a connection
    /// sends the options of the profile.
    #[test]
    fn an_edit_without_a_connection_reaches_the_profile_of_the_next_connect() {
        use navigator_core::net::{ConnectConfig, Profile};
        let o = opts(true, true);
        let mut s = Subs::default();
        s.push("t", Qos::AtLeastOnce);
        assert!(matches!(
            s.check_options_change("t", o),
            super::OptionsChange::Go(Qos::AtLeastOnce)
        ));
        s.commit_options("t", o, None);
        assert_eq!(s.row("t").expect("row").req, 0);
        let mut p = Profile::new();
        s.write_to(&mut p);
        let config = ConnectConfig::from(p);
        assert_eq!(config.profile.options_of("t"), o);
        assert_eq!(
            config.profile.subscriptions,
            [("t".to_owned(), Qos::AtLeastOnce)]
        );
    }

    /// A row that is being removed does not change its options. A shared filter refuses No
    /// Local.
    #[test]
    fn a_row_in_removal_keeps_its_options() {
        use super::OptionsChange;
        let mut s = Subs::default();
        s.push("t", Qos::AtLeastOnce);
        s.begin_remove("t");
        assert_eq!(
            s.check_options_change("t", opts(true, false)),
            OptionsChange::Ignore
        );
        assert_eq!(s.row("t").expect("row").options, opts(false, false));
        s.push("$share/g/t", Qos::AtMostOnce);
        assert!(matches!(
            s.check_options_change("$share/g/t", opts(true, false)),
            OptionsChange::Refused(_)
        ));
        assert_eq!(
            s.check_options_change("x", opts(true, false)),
            OptionsChange::Ignore
        );
    }

    fn status(state: ConnState, attempt: u64) -> Status {
        Status {
            generation: 1,
            state,
            attempt,
        }
    }

    fn details_with(receive_max: u16) -> Box<ConnectionDetails> {
        use navigator_core::net::V5Details;
        Box::new(ConnectionDetails {
            protocol: Protocol::V5,
            session_present: false,
            v5: Some(V5Details {
                assigned_client_id: None,
                keep_alive_secs: 60,
                server_keep_alive_secs: None,
                receive_max,
                max_packet_size: None,
                max_qos: 1,
                retain_available: true,
                wildcard_subscriptions: true,
                shared_subscriptions: true,
                subscription_ids: true,
                topic_alias_max: 0,
                server_reference: None,
                response_info: None,
                reason_string: None,
            }),
        })
    }

    fn shown(c: &DetailsCache, s: &Status) -> Option<u16> {
        c.current(s)
            .and_then(|d| d.v5.as_ref())
            .map(|v| v.receive_max)
    }

    /// Reconnects with other CONNACK values and a late delivery: the popover never shows the
    /// values of an older attempt, and it shows nothing until the details of the current
    /// attempt arrive.
    #[test]
    fn the_details_cache_never_shows_an_older_attempt() {
        let mut c = DetailsCache::default();
        let up = |attempt| status(ConnState::Connected, attempt);
        let lost = status(
            ConnState::Reconnecting {
                attempt: 1,
                reason: "lost".into(),
            },
            0,
        );
        // Attempt 5 connects. Its details come.
        c.offer(5, details_with(10));
        assert_eq!(shown(&c, &up(5)), Some(10));
        // The connection is lost: the cache clears, and nothing shows while not connected.
        c.clear();
        assert_eq!(shown(&c, &lost), None);
        // Attempt 6 is Connected, but the event of attempt 5 is still in the log and comes
        // first: it must not show.
        c.offer(5, details_with(10));
        assert_eq!(shown(&c, &up(6)), None);
        // The details of attempt 6 come (other values): they show.
        c.offer(6, details_with(20));
        assert_eq!(shown(&c, &up(6)), Some(20));
        // Delivery before the status: the details of attempt 7 arrive while the status still
        // says Reconnecting, and show when attempt 7 is Connected.
        c.clear();
        c.offer(7, details_with(30));
        assert_eq!(shown(&c, &lost), None);
        assert_eq!(shown(&c, &up(7)), Some(30));
        // A status that skipped the Reconnecting state (attempt 8, no clear): attempt 7
        // details do not show.
        assert_eq!(shown(&c, &up(8)), None);
    }

    #[test]
    fn the_status_bar_shows_an_optional_part_only_when_it_applies() {
        let quiet = StatusFacts {
            encrypted: Some(true),
            verify_tls: true,
            drops: 0,
            list_drops: 0,
            used: 80,
            budget: 100,
            reconnect: false,
        };
        let none = StatusItems {
            security: None,
            drops: false,
            list_drops: false,
            memory: false,
            reconnect: false,
        };
        // Exactly 80% is not above 80%.
        assert_eq!(status_items(&quiet), none);
        let busy = status_items(&StatusFacts {
            used: 81,
            drops: 3,
            list_drops: 5,
            reconnect: true,
            ..quiet
        });
        assert!(busy.memory && busy.drops && busy.list_drops && busy.reconnect);
        let plain = |encrypted, verify_tls| {
            status_items(&StatusFacts {
                encrypted,
                verify_tls,
                ..quiet
            })
            .security
        };
        assert_eq!(plain(Some(false), true), Some("Not encrypted"));
        assert_eq!(plain(Some(true), false), Some("Insecure TLS"));
        assert_eq!(plain(None, true), None);
    }

    #[test]
    fn filter_validation() {
        for ok in ["a", "a/b", "#", "a/#", "+", "a/+/b", "+/#", "/", "a//b"] {
            assert!(validate_filter(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "a/#/b",
            "a#",
            "#a",
            "a/b#",
            "a+",
            "+a/b",
            "a/b+/c",
            "a\0b",
            "a\u{7}b",
            "$share/g",
            "$share//a",
            "$share/g/",
            "$share/+/a",
        ] {
            assert!(validate_filter(bad).is_err(), "{bad}");
        }
        assert!(validate_filter("$share/g/a/+").is_ok());
        assert!(validate_filter(&"a".repeat(65_535)).is_ok());
        assert!(validate_filter(&"a".repeat(65_536)).is_err());
    }

    fn subs(filters: &[&str]) -> Subs {
        let list: Vec<(String, Qos)> = filters
            .iter()
            .map(|f| (f.to_string(), Qos::AtMostOnce))
            .collect();
        let mut s = Subs::from_profile(&list);
        for r in &mut s.rows {
            r.state = SubState::Active(Qos::AtMostOnce);
        }
        s
    }

    fn state(s: &mut Subs, f: &str) -> Option<SubState> {
        s.row(f).map(|r| r.state.clone())
    }

    #[test]
    fn remove_keeps_row_until_unsubscribed() {
        let mut s = subs(&["a", "b"]);
        s.begin_remove("a");
        assert_eq!(state(&mut s, "a"), Some(SubState::Removing));
        s.on_unsubscribed("a", Ok(()));
        assert_eq!(state(&mut s, "a"), None);
        assert!(s.row("b").is_some());
    }

    #[test]
    fn send_failure_keeps_the_subscription_wanted() {
        let mut s = subs(&["a"]);
        s.remove_not_sent("a", send_error_text(SendError::Full).into());
        assert!(matches!(state(&mut s, "a"), Some(SubState::NotRemoved(_))));
        assert_eq!(s.wanted().len(), 1);
        // Retry works and then the row goes.
        s.begin_remove("a");
        s.on_unsubscribed("a", Ok(()));
        assert!(s.rows.is_empty());
    }

    #[test]
    fn results_match_their_own_rows_in_any_order() {
        let mut s = subs(&["a", "b"]);
        s.begin_remove("a");
        s.begin_remove("b");
        // "b" answers first, with a rejection.
        s.on_unsubscribed("b", Err("Unsubscribe rejected: NotAuthorized".into()));
        assert_eq!(
            state(&mut s, "b"),
            Some(SubState::RemoveFailed(
                "Unsubscribe rejected: NotAuthorized".into()
            ))
        );
        assert_eq!(state(&mut s, "a"), Some(SubState::Removing));
        s.on_unsubscribed("a", Ok(()));
        assert_eq!(state(&mut s, "a"), None);
    }

    #[test]
    fn suback_does_not_revive_a_removed_row() {
        let mut s = subs(&["a"]);
        s.begin_remove("a");
        s.on_subscribed("a", Default::default(), Ok(Qos::AtLeastOnce));
        assert_eq!(state(&mut s, "a"), Some(SubState::Removing));
        let mut s = subs(&["a"]);
        s.on_subscribed("a", Default::default(), Err("no".into()));
        assert_eq!(state(&mut s, "a"), Some(SubState::Failed("no".into())));
    }

    #[test]
    fn accepted_removal_does_not_come_back_on_connect() {
        // Remove, then the connection drops before the UNSUBACK, then a manual Connect.
        let mut s = subs(&["a", "b", "c"]);
        s.begin_remove("a");
        s.begin_remove("c");
        s.on_unsubscribed("c", Err("Unsubscribe rejected: NotAuthorized".into()));
        assert_eq!(s.wanted(), vec![("b".to_string(), Qos::AtMostOnce)]);
        s.connection_reset();
        assert_eq!(state(&mut s, "a"), None);
        assert_eq!(state(&mut s, "c"), None);
        assert_eq!(state(&mut s, "b"), Some(SubState::Pending));
        assert_eq!(s.wanted(), vec![("b".to_string(), Qos::AtMostOnce)]);
    }

    #[test]
    fn the_index_finds_rows_after_removals() {
        let names: Vec<String> = (0..50).map(|i| format!("t/{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut s = subs(&refs);
        // Remove every third row, then add two more.
        for n in names.iter().step_by(3) {
            s.begin_remove(n);
            s.on_unsubscribed(n, Ok(()));
        }
        s.push("new/1", Qos::AtLeastOnce);
        s.push("new/2", Qos::AtMostOnce);
        for (i, n) in names.iter().enumerate() {
            assert_eq!(s.contains(n), i % 3 != 0, "{n}");
            assert_eq!(s.row(n).is_some(), i % 3 != 0, "{n}");
        }
        assert_eq!(s.row("new/1").map(|r| r.qos), Some(Qos::AtLeastOnce));
        assert_eq!(s.rows.len(), s.index.len());
        // The order of the rows is the order of the adds.
        assert!(s.rows.windows(2).all(|w| w[0].seq < w[1].seq));
    }

    #[test]
    fn a_filter_is_one_row_and_a_change_sets_the_dirty_flag() {
        let mut s = Subs::default();
        assert!(!s.take_dirty());
        s.push("a/#", Qos::AtMostOnce);
        s.push("a/#", Qos::AtLeastOnce);
        assert_eq!(s.rows.len(), 1);
        assert!(s.take_dirty() && !s.take_dirty());
        // A state change is not a change of the filter list.
        s.on_subscribed("a/#", Default::default(), Ok(Qos::AtMostOnce));
        assert!(!s.take_dirty());
        s.begin_remove("a/#");
        s.connection_reset();
        assert!(s.rows.is_empty() && s.index.is_empty() && s.take_dirty());
    }

    #[test]
    fn a_frame_after_a_quiet_time_does_not_wait() {
        assert_eq!(coalesce_wait(COALESCE), None);
        assert_eq!(coalesce_wait(COALESCE * 3), None);
        assert_eq!(coalesce_wait(Duration::ZERO), Some(COALESCE));
        assert_eq!(
            coalesce_wait(Duration::from_millis(20)),
            Some(COALESCE - Duration::from_millis(20))
        );
    }

    #[test]
    fn a_flood_skips_every_second_redraw_and_quiet_traffic_none() {
        let mut since = None;
        let (mut drawn, mut skipped) = (0, 0);
        // 10 s of merges every `COALESCE`, 330 messages each (10k a second).
        for _ in 0..300 {
            if skip_redraw(Caller::Loop, 330, since) {
                skipped += 1;
                since = since.map(|d| d + COALESCE);
            } else {
                drawn += 1;
                since = Some(COALESCE);
            }
        }
        assert_eq!(drawn, 150);
        assert_eq!(skipped, 150);
        // The timer of a skip is later than the next merge, and draws if no merge came.
        assert!(REDRAW_GAP > COALESCE && REDRAW_GAP < COALESCE * 2);
        // Quiet traffic, the first merge after a quiet time, backlog mode and input always draw.
        assert!(!skip_redraw(Caller::Loop, 255, Some(COALESCE)));
        assert!(!skip_redraw(Caller::Loop, 330, None));
        assert!(!skip_redraw(Caller::Loop, 330, Some(REDRAW_GAP)));
        assert!(!skip_redraw(Caller::Frame, 330, Some(COALESCE)));
        assert!(!skip_redraw(Caller::Fallback, 330, Some(COALESCE)));
    }

    /// A 20 Hz display (a frame each 50 ms): the callback allocates at 0, the fallback at 36, and
    /// the callback at 50 must not allocate, because the fallback used this interval.
    #[test]
    fn two_allocations_never_share_a_display_interval() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let ms = |n| Some(at(n));
        // The first callback, and the fallback 36 ms after the allocation.
        assert!(may_allocate(Caller::Frame, at(0), None, None));
        assert!(!may_allocate(Caller::Loop, at(20), ms(0), ms(0)));
        assert!(may_allocate(Caller::Loop, at(36), ms(0), ms(0)));
        // In backlog mode the fallback waits for stalled frames: at 20 Hz it never allocates.
        assert!(!may_allocate(Caller::Fallback, at(36), ms(0), ms(0)));
        assert!(!may_allocate(Caller::Fallback, at(72), ms(0), ms(50)));
        assert!(!may_allocate(Caller::Fallback, at(145), ms(100), ms(100)));
        // The callbacks allocate in every interval.
        assert!(may_allocate(Caller::Frame, at(50), ms(0), ms(0)));
        assert!(may_allocate(Caller::Frame, at(100), ms(50), ms(50)));
        // A stall of 100 ms lets the fallback allocate, and then a late callback skips.
        assert!(may_allocate(Caller::Fallback, at(133), ms(0), ms(0)));
        assert!(!may_allocate(Caller::Frame, at(140), ms(133), ms(0)));
        assert!(may_allocate(Caller::Frame, at(190), ms(133), ms(140)));
        // The first fallback of a hidden window (no callback yet) allocates.
        assert!(may_allocate(Caller::Fallback, at(33), ms(0), None));
        // At 144 Hz every callback allocates, and the loop never does.
        assert!(may_allocate(Caller::Frame, at(7), ms(0), ms(0)));
        assert!(!may_allocate(Caller::Loop, at(7), ms(0), ms(0)));
    }
}
