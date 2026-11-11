//! Main window. P3: title bar with filter, subscriptions, message list and topic tree, details,
//! composer, status bar.

mod commands;
mod compose;
mod details;
mod profiles;
mod update;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::component::WindowExt;
use gpui_kit::component::button::{Button, ButtonVariants, DropdownButton, Toggle, ToggleGroup};
use gpui_kit::component::command::{Command as PaletteList, CommandItem, CommandState};
use gpui_kit::component::empty::{Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyTitle};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::DropdownMenu;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::status_bar::StatusBar;
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
    Command, ConnState, ConnectConfig, Event, Net, NetLimits, Profile, SendError, Status,
};
use navigator_core::settings::{Load, RowHeight, Settings, ThemeMode, ViewMode};
use navigator_core::store::{
    Direction, DirectionFilter, Insert, Limits, ListFilter, Msg, NodeId, RowId, RowView,
    SendStatus, TopicStore, TreeFilter,
};
use uuid::Uuid;

use crate::connections::{Choice, ConnectionSheet, ProfileItem, SheetEvent};
use crate::messages::{
    colour_slot, format_time, group_digits, status_failed, status_from_result, status_text,
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

/// Registers the key bindings of this window. Call once at start.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-1", ShowMessages, Some(CONTEXT)),
        KeyBinding::new("cmd-1", ShowMessages, Some(CONTEXT)),
        KeyBinding::new("ctrl-2", ShowTree, Some(CONTEXT)),
        KeyBinding::new("cmd-2", ShowTree, Some(CONTEXT)),
        KeyBinding::new("ctrl-f", FocusFilter, Some(CONTEXT)),
        KeyBinding::new("cmd-f", FocusFilter, Some(CONTEXT)),
        KeyBinding::new("ctrl-k", OpenPalette, Some(CONTEXT)),
        KeyBinding::new("cmd-k", OpenPalette, Some(CONTEXT)),
        KeyBinding::new("ctrl-n", NewConnection, Some(CONTEXT)),
        KeyBinding::new("cmd-n", NewConnection, Some(CONTEXT)),
        // The payload editor inserts a newline for Ctrl+Enter and a tab for Tab. In the
        // composer, these keys send and move the focus.
        KeyBinding::new("ctrl-enter", SendMessage, Some("Composer > Input")),
        KeyBinding::new("cmd-enter", SendMessage, Some("Composer > Input")),
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
    /// Latest value of a node: node, receive time, stored length, truncated.
    Node(NodeId, SystemTime, usize, bool),
    Row(RowId),
    /// A History entry of a node: node, receive time, stored length, truncated.
    Hist(NodeId, SystemTime, usize, bool),
}

/// One visible row of the message list, copied out of the store for rendering.
enum RowData {
    Msg {
        id: RowId,
        node: Option<NodeId>,
        topic: SharedString,
        time: SystemTime,
        qos: u8,
        retain: bool,
        size: usize,
        preview: SharedString,
        sent: Option<SendStatus>,
        colour: Option<usize>,
    },
    Reconnected(RowId, SystemTime),
    Removed(RowId),
    RemovedMarker,
}

/// Most merge work on the UI thread in one frame.
const FRAME_BUDGET: Duration = Duration::from_millis(4);
/// Part of each frame that `TopicStore::work` keeps while it has work. A flood cannot take it.
const WORK_RESERVE: Duration = Duration::from_millis(1);
/// `TopicStore::work` stops this long before the end of the frame. One round takes up to 0.4 ms
/// (measured), and a round that starts before the deadline can end after it.
const WORK_MARGIN: Duration = Duration::from_micros(750);
/// Least time between two frames, so that many messages make one render.
const COALESCE: Duration = Duration::from_millis(33);

/// Merge time of one frame. While the store has work, the merge leaves `WORK_RESERVE` of it.
fn merge_budget(has_work: bool) -> Duration {
    if has_work {
        FRAME_BUDGET - WORK_RESERVE
    } else {
        FRAME_BUDGET
    }
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
    state: SubState,
    /// Rises with each push, so `rows` stays sorted by it. The index stores it, not a position.
    seq: u64,
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
            state: SubState::Pending,
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

    fn on_subscribed(&mut self, filter: &str, result: Result<Qos, String>) {
        if let Some(s) = self.row(filter)
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

/// Text for a command that the net queue did not accept.
fn send_error_text(e: SendError) -> &'static str {
    match e {
        SendError::Full => "Too many requests are waiting. Try again.",
        SendError::Closed => "The connection task stopped.",
    }
}

/// Checks a topic filter before it is sent. `Err` is the text for the user.
fn validate_filter(filter: &str) -> Result<(), &'static str> {
    if filter.is_empty() {
        return Err("Type a topic filter.");
    }
    if filter.len() > 65_535 {
        return Err("The filter is longer than 65535 bytes.");
    }
    if filter.contains('\0') {
        return Err("The filter must not contain a NUL character.");
    }
    let levels: Vec<&str> = filter.split('/').collect();
    for (i, level) in levels.iter().enumerate() {
        if level.contains('#') && (*level != "#" || i + 1 != levels.len()) {
            return Err("Use # alone as the last level, for example a/#.");
        }
        if level.contains('+') && *level != "+" {
            return Err("Use + alone as a whole level, for example a/+/b.");
        }
    }
    Ok(())
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
    profile_select: Entity<SelectState<Vec<ProfileItem>>>,
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
    follow: bool,
    two_lines: bool,
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
    insp: details::Inspector,
    /// The details pane is open. It stays closed until the user opens it again.
    details_open: bool,
    /// The window is narrower than `details::NARROW`: the details open as a sheet.
    narrow: bool,
    comp: compose::Composer,
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
    sub_qos: Qos,
    next_op: u64,
    last_result: Option<(u64, String)>,
    rate: Rate,
    metrics: Metrics,
    /// Metrics only: `MQTT_NAVIGATOR_AUTOSELECT=<topic>` (ignored without
    /// `MQTT_NAVIGATOR_METRICS`). When this topic arrives, the app selects it and applies Show
    /// all, as a click would, and the app connects to the saved profile at start. It exists to
    /// measure the frame time of a large payload without UI automation.
    autoselect: Option<String>,
    _subscriptions: Vec<Subscription>,
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
                .placeholder("Filter topics and payloads")
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
            }
        });
        let bounds = cx.observe_window_bounds(window, |v, window, cx| v.on_resize(window, cx));
        let profile_select =
            cx.new(|cx| SelectState::new(Vec::<ProfileItem>::new(), None, window, cx));
        let select_events = cx.subscribe_in(
            &profile_select,
            window,
            |v, _, ev: &SelectEvent<Vec<ProfileItem>>, window, cx| {
                let SelectEvent::Confirm(choice) = ev;
                match choice {
                    Some(Choice::Profile(id)) => v.request_switch(*id, false, window, cx),
                    Some(Choice::Manage) => {
                        // The select shows the active profile again, not "Manage connections…".
                        v.sync_select(window, cx);
                        v.open_sheet(None, false, window, cx);
                    }
                    None => {}
                }
            },
        );

        let (kick, kick_rx) = tokio::sync::mpsc::channel(1);
        Self::spawn_ingest(incoming, kick_rx, cx);
        Self::spawn_events(events, drained, cx);
        Self::spawn_status(status_rx, cx);
        // The time zone file is read off the UI thread.
        cx.spawn(async move |this, cx| {
            let offset = cx
                .background_executor()
                .spawn(async { chrono::Local::now().offset().local_minus_utc() })
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

        let subs = Subs::from_profile(profile.as_ref().map_or(&[], |p| &p.subscriptions));
        let (writer_errors, writer_errors_rx) = tokio::sync::mpsc::channel(4);
        let writer = crate::persist::settings_path().map(|p| Writer::start(p, writer_errors));
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
            profile_select,
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
            follow: true,
            two_lines: true,
            update: update::UpdateUi::default(),
            direction: DirectionFilter::All,
            show_only: None,
            filter_input,
            debounce: None,
            kick,
            anchor: None,
            utc_offset: 0,
            root_focus,
            list_focus: cx.focus_handle(),
            gate: GenGate::default(),
            insp: details::Inspector::new(window, cx),
            details_open: true,
            narrow: false,
            comp: compose::Composer::new(window, cx),
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
            sub_qos: Qos::AtMostOnce,
            next_op: 1,
            last_result: None,
            rate: Rate::default(),
            metrics: Metrics::from_env(),
            autoselect: std::env::var("MQTT_NAVIGATOR_AUTOSELECT")
                .ok()
                .filter(|_| std::env::var_os("MQTT_NAVIGATOR_METRICS").is_some()),
            // On quit, disconnect cleanly. gpui waits for quit handlers for at most 200 ms.
            _subscriptions: vec![
                appearance,
                bounds,
                select_events,
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
                        // "Restart to update": the launcher applies the update and starts the
                        // new version. This comes after the save and before the slow net stop.
                        if let Some(install) = restart {
                            background
                                .spawn(async move {
                                    let args = std::env::args_os().skip(1);
                                    if let Err(e) = navigator_update::restart(&install, args) {
                                        tracing::warn!(error = %e, "Cannot start the launcher");
                                    }
                                })
                                .await;
                        }
                        net.await
                    }
                }),
            ],
        };
        this.sync_select(window, cx);
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
        profile.subscriptions = self.subs.wanted();
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
        tracing::info!(url = %self.url(), "Connect");
        match self.net.connect(config) {
            Ok(generation) => {
                self.gate.begin(generation);
                self.connect_owners.push((generation, owner));
                let extra = self.connect_owners.len().saturating_sub(16);
                self.connect_owners.drain(..extra);
                // The broker forgets the subscriptions of the old connection.
                self.subs.connection_reset();
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
            Ok(()) if self.subs.contains(&filter) => {
                Some(format!("You are already subscribed to {filter}."))
            }
            Ok(()) => None,
        };
        if error.is_some() {
            self.sub_error = error;
            cx.notify();
            return;
        }
        let qos = self.sub_qos;
        // No row and no input reset if the queue refuses the command.
        if let Err(e) = self.send_subscribe(&filter, qos) {
            self.sub_error = Some(send_error_text(e).into());
            cx.notify();
            return;
        }
        self.sub_error = None;
        self.sub_input
            .update(cx, |i, cx| i.set_value("", window, cx));
        self.subs.push(&filter, qos);
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

    fn send_subscribe(&mut self, filter: &str, qos: Qos) -> Result<(), SendError> {
        self.net.send(Command::Subscribe {
            filter: filter.to_string(),
            qos,
        })
    }

    fn retry_sub(&mut self, filter: &str, cx: &mut Context<Self>) {
        let Some(qos) = self.subs.row(filter).map(|s| s.qos) else {
            return;
        };
        let state = match self.send_subscribe(filter, qos) {
            Ok(()) => SubState::Pending,
            Err(e) => SubState::Failed(send_error_text(e).into()),
        };
        if let Some(s) = self.subs.row(filter) {
            s.state = state;
        }
        cx.notify();
    }

    /// Starts the removal. The row stays until the UNSUBACK (or the offline answer) arrives.
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
        match self.net.send(cmd) {
            Ok(()) => {
                self.subs.begin_remove(filter);
                self.persist_subs();
            }
            Err(e) => self.subs.remove_not_sent(filter, send_error_text(e).into()),
        }
        cx.notify();
    }

    /// The frame loop. It is the only scheduler of UI-thread store work. Each frame has one
    /// deadline, shared by the merge and `TopicStore::work`. While work remains, one more frame
    /// follows after `COALESCE`. With no work, the loop waits for a message or a kick and has
    /// no timer.
    fn spawn_ingest(
        mut rx: tokio::sync::mpsc::Receiver<navigator_core::Incoming>,
        mut kick: tokio::sync::mpsc::Receiver<()>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let mut pending = false;
            let mut last_frame: Option<Instant> = None;
            loop {
                let first = if pending {
                    rx.try_recv().ok()
                } else {
                    tokio::select! {
                        m = rx.recv() => match m {
                            Some(m) => Some(m),
                            None => break,
                        },
                        k = kick.recv() => {
                            if k.is_none() {
                                break;
                            }
                            None
                        }
                    }
                };
                // The wait comes before the frame, so an idle loop has no timer.
                if let Some(wait) = last_frame.and_then(|t| coalesce_wait(t.elapsed())) {
                    cx.background_executor().timer(wait).await;
                }
                last_frame = Some(Instant::now());
                let frame = this.update(cx, |v, cx| {
                    let line = v.merge(first, &mut rx);
                    cx.notify();
                    v.schedule_rate_zero(cx);
                    (line, v.store.has_work())
                });
                match frame {
                    Err(_) => break,
                    Ok((line, more)) => {
                        pending = more;
                        if let Some(line) = line {
                            cx.background_executor().spawn(write_metrics(line)).detach();
                        }
                    }
                }
            }
        })
        .detach();
    }

    /// A design height in pixels now. The rows are `ui(..)` high, so this equals their height.
    fn scaled(&self, design: f32) -> f32 {
        design * self.rem / 16.
    }

    /// Height of one message row in pixels (the scroll anchor needs it).
    fn row_height(&self) -> f32 {
        self.scaled(if self.two_lines { ROW_H2 } else { ROW_H1 })
    }

    /// Remembers the top visible row, so that `restore_anchor` can undo the shift of new rows.
    /// Follow and a pause need no anchor. A scroll that is not yet drawn has no valid offset.
    fn capture_anchor(&mut self) {
        self.anchor = None;
        if self.follow || self.store.is_paused() {
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

    /// One frame: merges the messages and then runs store work, all inside `FRAME_BUDGET`.
    fn merge(
        &mut self,
        first: Option<navigator_core::Incoming>,
        rx: &mut tokio::sync::mpsc::Receiver<navigator_core::Incoming>,
    ) -> Option<String> {
        let start = Instant::now();
        let work = self.store.has_work();
        let deadline = start + merge_budget(work);
        self.capture_anchor();
        let mut n = 0u64;
        let mut metrics_line = None;
        let mut next = first;
        while let Some(msg) = next.take() {
            self.net.release(&msg);
            if self.accept(msg.generation) {
                let pick = self.autoselect.as_deref() == Some(&*msg.topic);
                if let Insert::Stored(id) = self.store.insert(msg)
                    && pick
                {
                    self.sel_topic = Some(id);
                    self.sel_msg = None;
                    self.detail_of = DetailOf::Topic;
                    self.details_open = true;
                    self.insp.hist_pick = None;
                    self.insp.set_auto_all();
                }
                n += 1;
            }
            if Instant::now() < deadline {
                next = rx.try_recv().ok();
            }
        }
        // Filter rebuilds and cap trims keep a part of the frame. `work` always does one round,
        // also when the merge used its time, so a flood cannot starve it.
        if self.store.has_work() {
            self.store.work(start + FRAME_BUDGET - WORK_MARGIN);
        }
        self.restore_anchor();
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
        self.metrics.merge(now - start);
        if self.metrics.due(now) {
            let s = self.store.stats();
            let drops = s.drops
                + s.topic_drops
                + self
                    .net
                    .stats
                    .drops
                    .load(std::sync::atomic::Ordering::Relaxed);
            let line = self
                .metrics
                .line(self.rate.per_sec, drops, s.topics, s.used_bytes, now);
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

    fn toggle_show_only(&mut self, filter: &str, cx: &mut Context<Self>) {
        self.show_only = if self.show_only.as_deref() == Some(filter) {
            None
        } else {
            Some(filter.to_string())
        };
        self.apply_filters(cx);
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        let pause = !self.store.is_paused();
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
        }
        if !self.accept(g) {
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
            Event::Subscribed { filter, result, .. } => {
                if let Err(why) = &result {
                    tracing::warn!(%why, "Subscribe failed");
                }
                self.subs.on_subscribed(&filter, result);
            }
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

    /// Selects a topic of the tree. `generation` is the data generation when the row was
    /// rendered. A click on a row of an older generation does nothing, because its `NodeId` may
    /// now name another topic.
    fn select(&mut self, id: NodeId, generation: u64, cx: &mut Context<Self>) {
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
        self.insp.hist_pick = None;
        if has_children {
            self.store.set_expanded(id, !expanded);
            // A paused tree takes a new snapshot: wake the frame loop for the work.
            self.kick_work();
        }
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
            ..
        }) = self.store.list_row(i)
        {
            self.sel_msg = Some(SelMsg {
                id,
                node,
                topic: Arc::from(topic),
                msg: msg.clone(),
                direction,
            });
            self.detail_of = DetailOf::Message;
            self.insp.hist_pick = None;
            self.follow = false;
            self.list_scroll.scroll_to_item(i, ScrollStrategy::Nearest);
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
                    self.insp.hist_pick = None;
                    if let Some(n) = next {
                        self.tree_scroll.scroll_to_item(n, ScrollStrategy::Nearest);
                    }
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
    fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if self.view == view {
            return;
        }
        self.view = view;
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
        format!("{scheme}://{}:{}", p.host, p.port)
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
                .child(
                    div().w(ui(180.)).child(
                        Select::new(&self.profile_select)
                            .small()
                            .placeholder("No connections")
                            .menu_width(ui(260.)),
                    ),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(div().size_2().rounded_full().bg(dot))
                        .child(div().text_sm().child(self.state_text()))
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
                .child(
                    div().w(ui(300.)).child(
                        Input::new(&self.filter_input)
                            .small()
                            .prefix(IconName::Search),
                    ),
                )
                .children(self.render_update_badge(cx))
                .child(
                    Button::new("palette")
                        .small()
                        .ghost()
                        .label("Commands…")
                        .tooltip(format!("Command palette ({}+K)", crate::palette::MOD))
                        .on_click(cx.listener(|v, _, window, cx| v.open_palette(window, cx))),
                )
                .child(self.render_theme_menu(cx))
                .child(
                    Button::new("connect")
                        .small()
                        .primary()
                        .label(label)
                        .disabled(
                            self.status.state == ConnState::Disconnecting || self.profile.is_none(),
                        )
                        .on_click(cx.listener(|v, _, window, cx| v.toggle_connection(window, cx))),
                ),
        )
    }

    /// The title bar menu: "Appearance" (System, Light, Dark) and "Theme" (the presets). The
    /// checked items show the current choice.
    fn render_theme_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.settings.ui.theme_mode;
        let current = crate::themes::preset_by_id(&self.settings.ui.theme_preset);
        let weak = cx.entity().downgrade();
        Button::new("theme")
            .small()
            .ghost()
            .icon(IconName::Palette)
            .label(current.label)
            .dropdown_caret(true)
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu.label("Appearance");
                for (label, m) in [
                    ("System", ThemeMode::System),
                    ("Light", ThemeMode::Light),
                    ("Dark", ThemeMode::Dark),
                ] {
                    let weak = weak.clone();
                    menu = menu.item(PopupMenuItem::new(label).checked(mode == m).on_click(
                        move |_, window, cx| {
                            weak.update(cx, |v, cx| v.set_appearance(m, window, cx))
                                .ok();
                        },
                    ));
                }
                menu = menu.separator().label("Theme");
                for p in crate::themes::PRESETS {
                    let weak = weak.clone();
                    menu = menu.item(
                        PopupMenuItem::new(p.label)
                            .checked(p.id == current.id)
                            .on_click(move |_, window, cx| {
                                weak.update(cx, |v, cx| v.set_preset(p.id, window, cx)).ok();
                            }),
                    );
                }
                menu
            })
    }

    /// One subscription row. Its height is `SUB_ROW_H`, so that the list can be virtual.
    fn sub_row(&self, i: usize, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(s) = self.subs.rows.get(i) else {
            return div().into_any_element();
        };
        let state: SharedString = match &s.state {
            SubState::Pending if self.connected() => "Subscribing…".into(),
            SubState::Pending => "Waiting for connection".into(),
            SubState::Active(q) => format!("granted {}", q.as_u8()).into(),
            SubState::Failed(e) | SubState::RemoveFailed(e) | SubState::NotRemoved(e) => {
                e.clone().into()
            }
            SubState::Removing => "Removing…".into(),
        };
        let retry_remove = matches!(s.state, SubState::RemoveFailed(_) | SubState::NotRemoved(_));
        let failed = retry_remove || matches!(s.state, SubState::Failed(_));
        // The button of a failed row repeats the action that failed.
        let retry_label = if retry_remove {
            "Retry remove"
        } else {
            "Retry"
        };
        // The ids come from the filter, not from the index: they stay when rows move.
        let row_id = ElementId::from(s.filter.clone());
        let (retry, remove, pick) = (s.filter.clone(), s.filter.clone(), s.filter.clone());
        let active = self.show_only.as_deref() == Some(&*s.filter);
        let line = h_flex()
            .w_full()
            .gap_2()
            .child(
                div()
                    .flex_none()
                    .size_2()
                    .rounded_full()
                    .bg(chip_colour(i, theme)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family(theme.mono_font_family.clone())
                    .text_sm()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(s.filter.clone()),
            )
            .child(Tag::secondary().xsmall().child(match s.qos {
                Qos::AtMostOnce => "Q0",
                Qos::AtLeastOnce => "Q1",
            }))
            .child(
                Button::new((row_id.clone(), "remove"))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Remove subscription")
                    .disabled(s.state == SubState::Removing)
                    .on_click(cx.listener(move |v, _, _, cx| {
                        // The row itself toggles "Show only": keep this click on the button.
                        cx.stop_propagation();
                        v.remove_sub(&remove, cx)
                    })),
            );
        let status = h_flex()
            .w_full()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(if failed {
                        theme.danger
                    } else if active {
                        theme.accent_foreground
                    } else {
                        theme.muted_foreground
                    })
                    .child(state),
            )
            .when(failed, |d| {
                d.child(
                    Button::new((row_id.clone(), "retry"))
                        .xsmall()
                        .label(retry_label)
                        .on_click(cx.listener(move |v, _, _, cx| {
                            cx.stop_propagation();
                            if retry_remove {
                                v.remove_sub(&retry, cx)
                            } else {
                                v.retry_sub(&retry, cx)
                            }
                        })),
                )
            });
        // Click the row to show only its messages. Click again to show all. The state of the
        // row (selected) is the fill of the row.
        ListItem::new(row_id)
            .h(ui(SUB_ROW_H))
            .flex_none()
            .overflow_hidden()
            .rounded_sm()
            .px_2()
            .py_0p5()
            .selected(active)
            .on_click(cx.listener(move |v, _, _, cx| v.toggle_show_only(&pick, cx)))
            .child(v_flex().w_full().child(line).child(status))
            .into_any_element()
    }

    fn render_subs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // A toggle group with a selected state: QoS 0 or QoS 1. A click on the selected one
        // keeps it.
        let qos = ToggleGroup::new("sub-qos")
            .segmented()
            .single()
            .small()
            .child(
                Toggle::new("sub-qos-0")
                    .label("Q0")
                    .checked(self.sub_qos == Qos::AtMostOnce),
            )
            .child(
                Toggle::new("sub-qos-1")
                    .label("Q1")
                    .checked(self.sub_qos == Qos::AtLeastOnce),
            )
            .on_click(cx.listener(|v, checks: &Vec<bool>, _, cx| {
                if let Some(ix) = checks.iter().position(|c| *c) {
                    v.sub_qos = if ix == 0 {
                        Qos::AtMostOnce
                    } else {
                        Qos::AtLeastOnce
                    };
                    cx.notify();
                }
            }));
        let has_hash = self.subs.contains("#");
        v_flex()
            .size_full()
            .border_r_1()
            .border_color(theme.border)
            .p_2()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Subscriptions"),
            )
            // Only the visible rows are built. The add input stays pinned below them.
            .child(
                self.sub_list
                    .clone()
                    .cached(StyleRefinement::default().w_full().flex_1().min_h_0()),
            )
            .when(has_hash && self.subs.rows.len() > 1, |d| {
                d.child(
                    h_flex()
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
                h_flex()
                    .gap_1()
                    .child(div().flex_1().child(Input::new(&self.sub_input).small()))
                    .child(qos),
            )
            .when_some(self.sub_error.clone(), |d, e| {
                d.child(div().text_xs().text_color(theme.danger).child(e))
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
            self.rem.to_bits(),
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

    /// The centre header: view switch, direction filter, pause, follow and row height.
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let paused = self.store.is_paused();
        let messages = self.view == View::Messages;
        let dir_ix = match self.direction {
            DirectionFilter::All => 0,
            DirectionFilter::Received => 1,
            DirectionFilter::Sent => 2,
        };
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
                        .on_click(cx.listener(|v, ix: &usize, _, cx| {
                            v.set_view(if *ix == 0 { View::Messages } else { View::Tree }, cx)
                        }))
                        .child("Messages")
                        .child("Tree"),
                ),
            )
            .when(messages, |d| {
                d.child(
                    div().flex_none().w(ui(210.)).child(
                        TabBar::new("direction")
                            .segmented()
                            .small()
                            .selected_index(dir_ix)
                            .on_click(cx.listener(|v, ix: &usize, _, cx| {
                                v.direction = match *ix {
                                    1 => DirectionFilter::Received,
                                    2 => DirectionFilter::Sent,
                                    _ => DirectionFilter::All,
                                };
                                v.apply_filters(cx);
                            }))
                            .child("All")
                            .child("Received")
                            .child("Sent"),
                    ),
                )
            })
            .child(div().flex_1())
            .child(
                Button::new("toggle-details")
                    .small()
                    .ghost()
                    .icon(IconName::PanelRight)
                    .label("Details")
                    .on_click(cx.listener(|v, _, window, cx| v.toggle_details(window, cx))),
            )
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
            .when(messages, |d| {
                d.child(
                    // A toggle with a selected state, not a primary/ghost swap.
                    Toggle::new("follow")
                        .small()
                        .icon(IconName::ArrowDown)
                        .label("Follow")
                        .checked(self.follow)
                        .on_click(cx.listener(|v, on: &bool, _, cx| {
                            v.follow = *on;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("row-height")
                        .small()
                        .ghost()
                        .label(if self.two_lines { "2 lines" } else { "1 line" })
                        .on_click(cx.listener(|v, _, _, cx| {
                            v.two_lines = !v.two_lines;
                            v.settings.ui.row_height = if v.two_lines {
                                RowHeight::TwoLines
                            } else {
                                RowHeight::OneLine
                            };
                            v.persist();
                            cx.notify();
                        })),
                )
            })
    }

    /// Active filters as small chips, with one "Clear filters" button.
    fn render_chips(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.filters_active() {
            return None;
        }
        let theme = cx.theme();
        let list = self.store.list_filter();
        let mut chips: Vec<String> = Vec::new();
        if !list.text.is_empty() {
            chips.push(format!("Text: {}", list.text));
        }
        if let Some(f) = &list.show_only {
            chips.push(format!("Only: {f}"));
        }
        if self.view == View::Messages {
            match list.direction {
                DirectionFilter::All => {}
                DirectionFilter::Received => chips.push("Received".into()),
                DirectionFilter::Sent => chips.push("Sent".into()),
            }
        }
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
                    Tag::secondary()
                        .small()
                        .max_w(ui(260.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(c)
                }))
                .child(
                    Button::new("clear-filters")
                        .xsmall()
                        .ghost()
                        .label("Clear filters")
                        .on_click(cx.listener(|v, _, window, cx| v.clear_filters(window, cx))),
                )
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
            return empty_state(
                "No messages yet",
                "Messages show here when the broker sends them.",
                [],
            );
        }
        let connecting = self.connecting()
            || matches!(
                self.status.state,
                ConnState::Connecting | ConnState::Reconnecting { .. }
            );
        let (title, what, action) = if connecting {
            (
                format!("Connecting to {}…", self.url()),
                "Messages show here when the connection is up.",
                "Cancel",
            )
        } else {
            (
                "Not connected".to_string(),
                "Connect to a broker to see messages.",
                "Connect",
            )
        };
        empty_state(
            &title,
            what,
            [Button::new("connect-empty")
                .small()
                .label(action)
                .on_click(cx.listener(|v, _, window, cx| v.toggle_connection(window, cx)))
                .into_any_element()],
        )
    }

    /// The centre pane: header, filter chips and the active view. The composer is added by the
    /// caller.
    fn render_list_area(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.view {
            View::Messages => self.render_messages(cx),
            View::Tree => self.render_tree(cx),
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .track_focus(&self.list_focus)
            .key_context(LIST_CONTEXT)
            .on_action(cx.listener(|v, _: &SelectPrev, _, cx| v.step_selection(-1, cx)))
            .on_action(cx.listener(|v, _: &SelectNext, _, cx| v.step_selection(1, cx)))
            .on_action(cx.listener(|v, _: &ExpandNode, _, cx| v.expand_selected(true, cx)))
            .on_action(cx.listener(|v, _: &CollapseNode, _, cx| v.expand_selected(false, cx)))
            // A user scroll turns Follow off.
            .on_scroll(cx.listener(|v, _, _, cx| {
                if v.view == View::Messages && v.follow && v.list_scroll.is_scrollable() {
                    v.follow = false;
                    cx.notify();
                }
            }))
            .child(body)
    }

    fn render_messages(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.store.list_len();
        if count == 0 {
            return self.render_empty(cx);
        }
        if self.follow && !self.store.is_paused() {
            self.list_scroll
                .scroll_to_item_strict(0, ScrollStrategy::Top);
        }
        uniform_list(
            "messages",
            count,
            cx.processor(|v, range: std::ops::Range<usize>, _window, cx| {
                let theme = cx.theme().clone();
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
                            qos: msg.qos.as_u8(),
                            retain: msg.retain,
                            size: msg.original_size,
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
                    .map(|row| v.message_row(row, &theme, cx))
                    .collect()
            }),
        )
        .track_scroll(&self.list_scroll)
        .flex_1()
        .into_any_element()
    }

    /// One row of the message list. Its height is fixed, so that the list can be virtual.
    fn message_row(&self, row: RowData, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let height = ui(if self.two_lines { ROW_H2 } else { ROW_H1 });
        let mono = theme.mono_font_family.clone();
        let muted = theme.muted_foreground;
        match row {
            RowData::Msg {
                id,
                node: _,
                topic,
                time,
                qos,
                retain,
                size,
                preview,
                sent,
                colour,
            } => {
                let selected = self.sel_msg.as_ref().is_some_and(|s| s.id == id);
                let chip = div()
                    .flex_none()
                    .size_2()
                    .rounded_full()
                    .map(|d| match colour {
                        Some(c) => d.bg(chip_colour(c, theme)),
                        None => d.border_1().border_color(theme.border),
                    });
                let topic = div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_family(mono.clone())
                    .text_sm()
                    .map(|d| {
                        if self.two_lines {
                            d.flex_1()
                        } else {
                            d.max_w(ui(320.))
                        }
                    })
                    .child(topic);
                let line = h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_none()
                            .font_family(mono.clone())
                            .text_xs()
                            .text_color(muted)
                            .child(format_time(time, self.utc_offset, true)),
                    )
                    .child(chip)
                    .when(sent.is_some(), |d| {
                        d.child(
                            h_flex()
                                .flex_none()
                                .gap_0p5()
                                .text_xs()
                                .text_color(muted)
                                .child(Icon::new(IconName::ArrowUp).xsmall())
                                .child("Sent"),
                        )
                    })
                    .child(topic)
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("QoS {qos}")),
                    )
                    .when(retain, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .child("R"),
                        )
                    })
                    .when_some(sent, |d, st| {
                        d.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(if status_failed(&st) {
                                    theme.danger
                                } else {
                                    muted
                                })
                                .child(status_text(&st)),
                        )
                    });
                let preview = div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_family(mono.clone())
                    .text_xs()
                    .text_color(muted)
                    .child(preview);
                let size = div()
                    .flex_none()
                    .w(ui(64.))
                    .flex()
                    .justify_end()
                    .font_family(mono)
                    .text_xs()
                    .text_color(muted)
                    .child(format!("{size} B"));
                let content = if self.two_lines {
                    v_flex()
                        .size_full()
                        .justify_center()
                        .child(line.child(size))
                        .child(preview.w_full())
                } else {
                    v_flex()
                        .size_full()
                        .justify_center()
                        .child(line.child(preview.flex_1()).child(size))
                };
                content
                    .id(("msg", id.0 as usize))
                    .h(height)
                    .px_2()
                    .when(selected, |d| d.bg(theme.accent))
                    .hover(|d| d.bg(theme.muted))
                    .on_click(cx.listener(move |v, _, window, cx| {
                        v.list_focus.focus(window, cx);
                        v.select_row(id, cx);
                        // A click opens the sheet. The keys (Up, Down) must keep the list focus.
                        v.insp.want_sheet = v.narrow && v.details_open;
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
        uniform_list(
            "topics",
            count,
            cx.processor(|v, range: std::ops::Range<usize>, _window, cx| {
                let theme = cx.theme().clone();
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
                        h_flex()
                            .id(("row", node_key(id)))
                            .h(ui(TREE_ROW_H))
                            .px_2()
                            .gap_2()
                            .text_sm()
                            .when(selected, |d| d.bg(theme.accent))
                            .hover(|d| d.bg(theme.muted))
                            .child(div().w(ui(n.depth as f32 * 14.)))
                            .child(
                                div().w(ui(14.)).flex_none().when_some(chevron, |d, icon| {
                                    d.child(Icon::new(icon).xsmall())
                                }),
                            )
                            .child(
                                div()
                                    .font_family(theme.mono_font_family.clone())
                                    .child(n.name.to_string()),
                            )
                            .when(n.subtree_topics > 0, |d| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(format!("{} topics", n.subtree_topics)),
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                div()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_color(theme.muted_foreground)
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
                            .on_click(cx.listener(move |v, _, window, cx| {
                                v.list_focus.focus(window, cx);
                                v.select(id, generation, cx);
                                v.insp.want_sheet = v.narrow && v.details_open;
                            }))
                            .into_any_element()
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.tree_scroll)
        .flex_1()
        .into_any_element()
    }

    /// "used / limit". A click opens the list of limits (the same ones as in the palette).
    fn render_memory(&self, used: usize, weak: WeakEntity<Self>) -> impl IntoElement {
        let current = self.memory_mib();
        DropdownButton::new("memory-limit")
            .button(
                Button::new("memory-limit-button")
                    .xsmall()
                    .ghost()
                    .label(format!(
                        "{} MiB / {}",
                        used / MIB,
                        crate::palette::memory_text(current)
                    )),
            )
            .dropdown_menu(move |menu, _, _| {
                crate::palette::MEMORY_OPTIONS_MIB.into_iter().fold(
                    menu.label("Memory limit"),
                    |menu, mib| {
                        let weak = weak.clone();
                        menu.item(
                            PopupMenuItem::new(crate::palette::memory_text(mib))
                                .checked(mib == current)
                                .on_click(move |_, _, cx| {
                                    weak.update(cx, |v, cx| v.set_memory_limit(mib, cx)).ok();
                                }),
                        )
                    },
                )
            })
    }

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
            self.insp.expanded = false;
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
        let memory = self.render_memory(s.used_bytes, cx.entity().downgrade());
        let drops = s.drops
            + s.topic_drops
            + self
                .net
                .stats
                .drops
                .load(std::sync::atomic::Ordering::Relaxed);
        let theme = cx.theme();
        let mut left = h_flex().gap_3().text_xs().child(self.url());
        if let Some(p) = &self.profile {
            if !p.transport.is_encrypted() {
                left = left.child(Tag::warning().xsmall().child("Not encrypted"));
            } else if !p.verify_tls {
                left = left.child(Tag::warning().xsmall().child("Insecure TLS"));
            }
        }
        if self.reconnected_at.is_some() {
            left = left.child("Reconnected");
        }
        if self.reconnect_pending && self.is_live() {
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
        StatusBar::new().left(left).right(
            h_flex()
                .gap_3()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .child(format!("{} msg/s", self.rate.per_sec))
                .child(format!("{} topics", s.topics))
                .child(format!("{} rows", s.list_rows))
                .when(s.list_drops > 0, |d| {
                    d.child(format!("{} rows evicted", s.list_drops))
                })
                .child(memory)
                .child(
                    div()
                        .when(drops > 0, |d| d.text_color(theme.danger))
                        .child(format!("{drops} dropped")),
                ),
        )
    }
}

impl Render for Navigator {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.metrics.frame_start(window);
        self.rem = f32::from(window.rem_size());
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
        self.narrow = f32::from(window.viewport_size().width) < details::NARROW;
        if self.narrow && self.insp.want_sheet {
            self.insp.want_sheet = false;
            cx.defer_in(window, |v, window, cx| v.open_details_sheet(window, cx));
        } else if !self.narrow && self.insp.sheet_open {
            self.insp.sheet_open = false;
            cx.defer_in(window, |_, window, cx| window.close_sheet(cx));
        }
        let chips = self.render_chips(cx);
        let show_details = self.details_open && !self.narrow;
        let details = show_details.then(|| self.render_details(window, false, cx));
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
                                    limits.right.1..if self.insp.expanded {
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
            .child(self.render_status(cx))
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
}

impl Metrics {
    fn from_env() -> Self {
        Self {
            on: std::env::var_os("MQTT_NAVIGATOR_METRICS").is_some(),
            merge_ms: Vec::new(),
            frame_ms: Default::default(),
            last: Instant::now(),
            first_frame_logged: false,
        }
    }

    fn merge(&mut self, d: Duration) {
        if self.on {
            self.merge_ms.push(d.as_secs_f32() * 1000.);
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
    fn line(&mut self, rate: u64, drops: u64, topics: usize, used: usize, now: Instant) -> String {
        fn p95(v: &mut [f32]) -> (f32, f32) {
            if v.is_empty() {
                return (0., 0.);
            }
            v.sort_by(f32::total_cmp);
            (v[(v.len() - 1) * 95 / 100], v[v.len() - 1])
        }
        let (mp, mm) = p95(&mut self.merge_ms);
        let (fp, fm) = p95(&mut self.frame_ms.borrow_mut());
        let line = format!(
            "metrics rate={rate}/s drops={drops} topics={topics} store={}MiB \
             merge_p95={mp:.2}ms merge_max={mm:.2}ms frame_p95={fp:.2}ms frame_max={fm:.2}ms frames={}",
            used / (1024 * 1024),
            self.frame_ms.borrow().len(),
        );
        self.merge_ms.clear();
        self.frame_ms.borrow_mut().clear();
        self.last = now;
        line
    }
}

/// Writes a metrics line with the RSS. Runs on a background thread (file and stderr I/O).
async fn write_metrics(line: String) {
    let rss_mib = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * 4096 / (1024 * 1024));
    eprintln!("{line} rss={rss_mib}MiB");
}

/// A closed receiver, used to take the real receivers out of `Net`.
fn tokio_channel_placeholder<T>() -> tokio::sync::mpsc::Receiver<T> {
    tokio::sync::mpsc::channel(1).1
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the gpui glob import brings its own `test` macro.
    use super::{
        COALESCE, FRAME_BUDGET, GenGate, SubState, Subs, WORK_RESERVE, coalesce_wait,
        connection_live, merge_budget, scroll_top_of, send_error_text, top_row, ui,
        validate_filter,
    };
    use gpui_kit::{px, rems};
    use navigator_core::Qos;
    use navigator_core::net::SendError;
    use std::time::Duration;

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

    #[test]
    fn filter_validation() {
        for ok in ["a", "a/b", "#", "a/#", "+", "a/+/b", "+/#", "/", "a//b"] {
            assert!(validate_filter(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "a/#/b", "a#", "#a", "a/b#", "a+", "+a/b", "a/b+/c", "a\0b",
        ] {
            assert!(validate_filter(bad).is_err(), "{bad}");
        }
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
        s.on_subscribed("a", Ok(Qos::AtLeastOnce));
        assert_eq!(state(&mut s, "a"), Some(SubState::Removing));
        let mut s = subs(&["a"]);
        s.on_subscribed("a", Err("no".into()));
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
        s.on_subscribed("a/#", Ok(Qos::AtMostOnce));
        assert!(!s.take_dirty());
        s.begin_remove("a/#");
        s.connection_reset();
        assert!(s.rows.is_empty() && s.index.is_empty() && s.take_dirty());
    }

    #[test]
    fn the_merge_leaves_the_work_reserve_only_when_there_is_work() {
        assert_eq!(merge_budget(false), FRAME_BUDGET);
        assert_eq!(merge_budget(true) + WORK_RESERVE, FRAME_BUDGET);
        assert!(merge_budget(true) >= Duration::from_millis(3));
    }

    #[test]
    fn a_frame_after_a_quiet_time_does_not_wait() {
        assert_eq!(coalesce_wait(COALESCE), None);
        assert_eq!(coalesce_wait(COALESCE * 3), None);
        assert_eq!(coalesce_wait(Duration::ZERO), Some(COALESCE));
        assert_eq!(
            coalesce_wait(Duration::from_millis(30)),
            Some(COALESCE - Duration::from_millis(30))
        );
    }
}
