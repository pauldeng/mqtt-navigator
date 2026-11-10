//! Main window. P3: title bar with filter, subscriptions, message list and topic tree, details,
//! composer, status bar.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::tab::TabBar;
use gpui_kit::component::{
    ActiveTheme, Disableable, IconName, Sizable, Theme, TitleBar, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use navigator_core::net::{
    Command, ConnState, Event, Net, NetLimits, Outgoing, Profile, SendError, Status,
};
use navigator_core::store::{
    Direction, DirectionFilter, Limits, ListFilter, Msg, NodeId, RowId, RowView, SendStatus,
    TopicStore, TreeFilter,
};
use navigator_core::{MsgProps, Qos};

use crate::messages::{
    colour_slot, format_time, group_digits, status_failed, status_from_result, status_text,
};

actions!(
    navigator_ui,
    [
        ShowMessages,
        ShowTree,
        FocusFilter,
        SelectPrev,
        SelectNext,
        ExpandNode,
        CollapseNode
    ]
);

const CONTEXT: &str = "Navigator";
const LIST_CONTEXT: &str = "NavigatorList";

/// Registers the key bindings of this window. Call once at start.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-1", ShowMessages, Some(CONTEXT)),
        KeyBinding::new("cmd-1", ShowMessages, Some(CONTEXT)),
        KeyBinding::new("ctrl-2", ShowTree, Some(CONTEXT)),
        KeyBinding::new("cmd-2", ShowTree, Some(CONTEXT)),
        KeyBinding::new("ctrl-f", FocusFilter, Some(CONTEXT)),
        KeyBinding::new("cmd-f", FocusFilter, Some(CONTEXT)),
        KeyBinding::new("up", SelectPrev, Some(LIST_CONTEXT)),
        KeyBinding::new("down", SelectNext, Some(LIST_CONTEXT)),
        KeyBinding::new("right", ExpandNode, Some(LIST_CONTEXT)),
        KeyBinding::new("left", CollapseNode, Some(LIST_CONTEXT)),
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
/// Wait between merges, so that many messages make one render.
const COALESCE: Duration = Duration::from_millis(33);
/// Wait before a filter text change applies.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Row heights of the message list and the tree.
const MIB: usize = 1024 * 1024;
const ROW_HEIGHT: f32 = 24.;
const ROW_HEIGHT_2: f32 = 40.;

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
    filter: String,
    qos: Qos,
    state: SubState,
}

/// The subscription rows. Free of gpui types, so that tests can run it.
#[derive(Default)]
struct Subs {
    rows: Vec<Sub>,
}

impl Subs {
    fn row(&mut self, filter: &str) -> Option<&mut Sub> {
        self.rows.iter_mut().find(|s| s.filter == filter)
    }

    /// Filters that the next Connect must subscribe. An accepted removal is not one of them.
    fn wanted(&self) -> Vec<(String, Qos)> {
        self.rows
            .iter()
            .filter(|s| !s.state.removal_accepted())
            .map(|s| (s.filter.clone(), s.qos))
            .collect()
    }

    /// The connection changed. The broker forgot its subscriptions (clean start), so SUBACKs
    /// come again. A row whose removal the net thread accepted is gone: the filter is not in the
    /// desired list, and the new session does not have it.
    fn connection_reset(&mut self) {
        self.rows.retain(|s| !s.state.removal_accepted());
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
            Ok(()) => self
                .rows
                .retain(|s| s.filter != filter || !s.state.removal_accepted()),
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
    profile: Profile,
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
    /// Subscription filters that the store has, in row order.
    synced_subs: Vec<String>,
    /// Local time zone offset in seconds. It is read once in the background.
    utc_offset: i32,
    root_focus: FocusHandle,
    list_focus: FocusHandle,
    /// Connection generation of the shown data. Newer data from the net thread replaces it.
    generation: u64,
    /// Formatted payload of the selected message, with the key it was built for.
    detail_cache: Option<(DetailKey, SharedString)>,
    list_scroll: UniformListScrollHandle,
    tree_scroll: UniformListScrollHandle,
    topic_input: Entity<InputState>,
    sub_input: Entity<InputState>,
    /// Error text under the subscribe input.
    sub_error: Option<String>,
    /// Set when the connection came back by itself. Cleared when the connection ends again.
    reconnected_at: Option<std::time::SystemTime>,
    payload_input: Entity<InputState>,
    qos: Qos,
    sub_qos: Qos,
    next_op: u64,
    last_result: Option<(u64, String)>,
    rate: Rate,
    metrics: Metrics,
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
    pub fn new(profile: Profile, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut net = Net::start(NetLimits::default());
        let incoming = std::mem::replace(&mut net.incoming, tokio_channel_placeholder());
        let events = std::mem::replace(&mut net.events, tokio_channel_placeholder());
        let status_rx = net.status.clone();

        let topic_input = cx.new(|cx| InputState::new(window, cx).placeholder("Topic"));
        let payload_input = cx.new(|cx| InputState::new(window, cx).placeholder("Payload"));
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

        // Theme follows the system (event-driven).
        Theme::sync_system_appearance(Some(window), cx);
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
        });

        let (kick, kick_rx) = tokio::sync::mpsc::channel(1);
        Self::spawn_ingest(incoming, kick_rx, cx);
        Self::spawn_events(events, cx);
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

        let subs = Subs {
            rows: profile
                .subscriptions
                .iter()
                .map(|(filter, qos)| Sub {
                    filter: filter.clone(),
                    qos: *qos,
                    state: SubState::Pending,
                })
                .collect(),
        };
        let mut this = Self {
            profile,
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
            direction: DirectionFilter::All,
            show_only: None,
            filter_input,
            debounce: None,
            kick,
            anchor: None,
            synced_subs: Vec::new(),
            utc_offset: 0,
            root_focus,
            list_focus: cx.focus_handle(),
            generation: 0,
            detail_cache: None,
            list_scroll: UniformListScrollHandle::new(),
            tree_scroll: UniformListScrollHandle::new(),
            topic_input,
            sub_input,
            sub_error: None,
            reconnected_at: None,
            payload_input,
            qos: Qos::AtMostOnce,
            sub_qos: Qos::AtMostOnce,
            next_op: 1,
            last_result: None,
            rate: Rate::default(),
            metrics: Metrics::from_env(),
            // On quit, disconnect cleanly. gpui waits for quit handlers for at most 200 ms.
            _subscriptions: vec![
                appearance,
                sub_events,
                filter_events,
                cx.on_app_quit(|v: &mut Self, _| v.net.shutdown()),
            ],
        };
        this.sync_subs();
        this.connect();
        this
    }

    /// Accepts data of generation `g`. Data of an older generation is dropped. A newer generation
    /// (a manual connect) keeps the data, because data belongs to one profile and there is one
    /// profile. Only the SUBACK results restart. Messages, events and status can arrive in any
    /// order, so each of them calls this first.
    fn accept(&mut self, g: u64) -> bool {
        if g < self.generation {
            return false;
        }
        if g > self.generation {
            self.generation = g;
            self.subs.connection_reset();
            self.sync_subs();
        }
        true
    }

    fn connect(&self) {
        // The subscription list is what the user sees now, not what the command line gave.
        let mut profile = self.profile.clone();
        profile.subscriptions = self.subs.wanted();
        tracing::info!(url = %self.url(), "Connect");
        if self.net.send(Command::Connect(Box::new(profile))).is_err() {
            // ponytail: the command queue (64) is full only under extreme UI spam.
        }
    }

    fn add_sub(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let filter = self.sub_input.read(cx).value().to_string();
        let error = match validate_filter(&filter) {
            Err(text) => Some(text.to_string()),
            Ok(()) if self.subs.rows.iter().any(|s| s.filter == filter) => {
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
        self.subs.rows.push(Sub {
            filter,
            qos,
            state: SubState::Pending,
        });
        self.sync_subs();
        cx.notify();
    }

    /// Gives the store the subscription filters in row order (the colours follow the order).
    fn sync_subs(&mut self) {
        if self
            .synced_subs
            .iter()
            .eq(self.subs.rows.iter().map(|s| &s.filter))
        {
            return;
        }
        self.synced_subs = self.subs.rows.iter().map(|s| s.filter.clone()).collect();
        self.store.set_subscriptions(self.synced_subs.clone());
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
            Ok(()) => self.subs.begin_remove(filter),
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
                cx.background_executor().timer(COALESCE).await;
            }
        })
        .detach();
    }

    fn row_height(&self) -> f32 {
        if self.two_lines {
            ROW_HEIGHT_2
        } else {
            ROW_HEIGHT
        }
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
        let deadline = start + FRAME_BUDGET;
        self.capture_anchor();
        let mut n = 0u64;
        let mut metrics_line = None;
        let mut next = first;
        while let Some(msg) = next.take() {
            self.net.release(&msg);
            if self.accept(msg.generation) {
                self.store.insert(msg);
                n += 1;
            }
            if Instant::now() < deadline {
                next = rx.try_recv().ok();
            }
        }
        // Filter rebuilds and cap trims use what is left of the frame budget.
        if self.store.has_work() && Instant::now() < deadline {
            self.store.work(deadline);
        }
        self.restore_anchor();
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

    fn spawn_events(mut rx: tokio::sync::mpsc::Receiver<Event>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
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
            | Event::Error { generation, .. } => *generation,
        };
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
            Event::Published { .. } => {}
            Event::Error { message, .. } => self.last_result = Some((0, message)),
            // An UNSUBSCRIBE result shows on its row, not in the publish line.
            Event::Unsubscribed { filter, result, .. } => {
                self.subs.on_unsubscribed(&filter, result);
                self.sync_subs();
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
                            v.sync_subs();
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

    fn toggle_connection(&mut self, cx: &mut Context<Self>) {
        match self.status.state {
            ConnState::Connected | ConnState::Connecting | ConnState::Reconnecting { .. } => {
                self.net.disconnect()
            }
            ConnState::Disconnecting => {}
            ConnState::Disconnected | ConnState::Failed(_) => self.connect(),
        }
        cx.notify();
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        // Spaces are part of an MQTT topic name, so the topic is not trimmed.
        let topic = self.topic_input.read(cx).value().to_string();
        if topic.is_empty() {
            self.last_result = Some((0, "Type a topic.".into()));
            cx.notify();
            return;
        }
        let payload = self.payload_input.read(cx).value().to_string();
        let op_id = self.next_op;
        self.next_op += 1;
        let payload = Bytes::from(payload);
        let out = Outgoing {
            op_id,
            topic: topic.clone(),
            payload: payload.clone(),
            qos: self.qos,
            retain: false,
            props: MsgProps::default(),
        };
        self.last_result = match self.net.send(Command::Publish(Box::new(out))) {
            Ok(()) => {
                // The list row shows the progress. The broker sends the message back as a
                // normal received message if a subscription matches.
                self.capture_anchor();
                self.store.record_sent(
                    op_id,
                    &topic,
                    payload,
                    self.qos,
                    false,
                    MsgProps::default(),
                    SystemTime::now(),
                );
                self.restore_anchor();
                Some((op_id, status_text(&SendStatus::Sending)))
            }
            Err(_) => Some((op_id, "Too many messages are waiting. Try again.".into())),
        };
        cx.notify();
    }

    /// Selects a topic of the tree. `generation` is the data generation when the row was
    /// rendered. A click on a row of an older generation does nothing, because its `NodeId` may
    /// now name another topic.
    fn select(&mut self, id: NodeId, generation: u64, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        let Some(node) = self.store.get(id) else {
            return;
        };
        let (has_children, expanded) = (node.has_children, node.expanded);
        self.sel_topic = Some(id);
        self.sel_msg = None;
        self.detail_of = DetailOf::Topic;
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
        match view {
            View::Tree => {
                let node = self
                    .sel_msg
                    .as_ref()
                    .and_then(|s| s.node.or_else(|| self.store.find(&s.topic)));
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

    fn connected(&self) -> bool {
        self.status.state == ConnState::Connected
    }

    fn state_text(&self) -> String {
        match &self.status.state {
            ConnState::Disconnected => "Disconnected".into(),
            ConnState::Connecting => "Connecting…".into(),
            ConnState::Reconnecting { attempt, .. } => format!("Reconnecting ({attempt})…"),
            ConnState::Connected => "Connected".into(),
            ConnState::Disconnecting => "Disconnecting…".into(),
            ConnState::Failed(why) => format!("Disconnected: {why}"),
        }
    }

    fn url(&self) -> String {
        let p = &self.profile;
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
            ConnState::Connected => "Disconnect",
            ConnState::Connecting | ConnState::Reconnecting { .. } => "Cancel",
            _ => "Connect",
        };
        TitleBar::new().child(
            h_flex()
                .w_full()
                .pr_2()
                .gap_3()
                .child(div().text_sm().child(self.profile.host.clone()))
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
                                    .max_w(px(360.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(why),
                            )
                        }),
                )
                .child(div().flex_1())
                .child(
                    div().w(px(300.)).child(
                        Input::new(&self.filter_input)
                            .small()
                            .prefix(IconName::Search),
                    ),
                )
                .child(
                    Button::new("connect")
                        .small()
                        .primary()
                        .label(label)
                        .disabled(self.status.state == ConnState::Disconnecting)
                        .on_click(cx.listener(|v, _, _, cx| v.toggle_connection(cx))),
                ),
        )
    }

    fn render_subs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let qos_button = |q: Qos| {
            let b = Button::new(("sub-qos", q.as_u8() as usize))
                .small()
                .label(format!("Q{}", q.as_u8()));
            let b = if self.sub_qos == q {
                b.primary()
            } else {
                b.ghost()
            };
            b.on_click(cx.listener(move |v, _, _, cx| {
                v.sub_qos = q;
                cx.notify();
            }))
        };
        let has_hash = self.subs.rows.iter().any(|s| s.filter == "#");
        let rows = self.subs.rows.iter().enumerate().map(|(i, s)| {
            let state: SharedString = match &s.state {
                SubState::Pending if self.connected() => "Subscribing…".into(),
                SubState::Pending => "Waiting for connection".into(),
                SubState::Active(q) => format!("granted {}", q.as_u8()).into(),
                SubState::Failed(e) | SubState::RemoveFailed(e) | SubState::NotRemoved(e) => {
                    e.clone().into()
                }
                SubState::Removing => "Removing…".into(),
            };
            let failed = matches!(
                s.state,
                SubState::Failed(_) | SubState::RemoveFailed(_) | SubState::NotRemoved(_)
            );
            // The button of a failed row repeats the action that failed.
            let retry_label =
                if matches!(s.state, SubState::RemoveFailed(_) | SubState::NotRemoved(_)) {
                    "Retry remove"
                } else {
                    "Retry"
                };
            let retry_remove =
                matches!(s.state, SubState::RemoveFailed(_) | SubState::NotRemoved(_));
            let (retry, remove) = (s.filter.clone(), s.filter.clone());
            let (id_retry, id_remove, id_pick) = (
                SharedString::from(format!("retry:{}", s.filter)),
                SharedString::from(format!("remove:{}", s.filter)),
                SharedString::from(format!("show-only:{}", s.filter)),
            );
            let (pick, active) = (
                s.filter.clone(),
                self.show_only.as_deref() == Some(s.filter.as_str()),
            );
            let chip = chip_colour(i, theme);
            v_flex()
                .rounded_sm()
                .when(active, |d| d.bg(theme.accent))
                .child(
                    h_flex()
                        .gap_2()
                        // Click the filter to show only its messages. Click again to show all.
                        .child(
                            h_flex()
                                .id(id_pick)
                                .flex_1()
                                .min_w_0()
                                .gap_2()
                                .cursor_pointer()
                                .on_click(
                                    cx.listener(move |v, _, _, cx| v.toggle_show_only(&pick, cx)),
                                )
                                .child(div().flex_none().size_2().rounded_full().bg(chip))
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
                                ),
                        )
                        .child(div().text_xs().child(format!("Q{}", s.qos.as_u8())))
                        .child(
                            Button::new(id_remove)
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip(format!("Remove subscription {}", s.filter))
                                .disabled(s.state == SubState::Removing)
                                .on_click(
                                    cx.listener(move |v, _, _, cx| v.remove_sub(&remove, cx)),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .text_color(if failed {
                                    theme.danger
                                } else {
                                    theme.muted_foreground
                                })
                                .child(state),
                        )
                        .when(failed, |d| {
                            d.child(Button::new(id_retry).xsmall().label(retry_label).on_click(
                                cx.listener(move |v, _, _, cx| {
                                    if retry_remove {
                                        v.remove_sub(&retry, cx)
                                    } else {
                                        v.retry_sub(&retry, cx)
                                    }
                                }),
                            ))
                        }),
                )
        });
        v_flex()
            .w(px(260.))
            .h_full()
            .border_r_1()
            .border_color(theme.border)
            .p_2()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("SUBSCRIPTIONS"),
            )
            // The rows scroll. The add input stays pinned below them.
            .child(
                div()
                    .id("subs")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(v_flex().gap_1().children(rows)),
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
                    .child(qos_button(Qos::AtMostOnce))
                    .child(qos_button(Qos::AtLeastOnce)),
            )
            .when_some(self.sub_error.clone(), |d, e| {
                d.child(div().text_xs().text_color(theme.danger).child(e))
            })
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
                div().flex_none().w(px(150.)).child(
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
                    div().flex_none().w(px(210.)).child(
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
                    Button::new("follow")
                        .small()
                        .when(self.follow, |b| b.primary())
                        .when(!self.follow, |b| b.ghost())
                        .label("Follow")
                        .tooltip("Keep the newest message in view")
                        .on_click(cx.listener(|v, _, _, cx| {
                            v.follow = !v.follow;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("row-height")
                        .small()
                        .ghost()
                        .label(if self.two_lines { "2 lines" } else { "1 line" })
                        .tooltip("Row height")
                        .on_click(cx.listener(|v, _, _, cx| {
                            v.two_lines = !v.two_lines;
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
                    div()
                        .max_w(px(260.))
                        .px_2()
                        .rounded_full()
                        .border_1()
                        .border_color(theme.border)
                        .text_xs()
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
        let filtered = self.filters_active();
        let text = if filtered {
            match self.view {
                View::Messages => "No messages match the filters.",
                View::Tree => "No topics match the filters.",
            }
        } else if self.connected() {
            "No messages yet. Messages show here when the broker sends them."
        } else {
            "Connect to a broker to see messages."
        };
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .text_color(cx.theme().muted_foreground)
            .child(text)
            .when(filtered, |d| {
                d.child(
                    Button::new("clear-filters-empty")
                        .small()
                        .label("Clear filters")
                        .on_click(cx.listener(|v, _, window, cx| v.clear_filters(window, cx))),
                )
            })
            .into_any_element()
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
        let height = px(self.row_height());
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
                            d.max_w(px(320.))
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
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(muted)
                                .child("↑ Sent"),
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
                    .w(px(64.))
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
                let generation = v.generation;
                let ids: Vec<NodeId> = v.store.rows()[range].to_vec();
                ids.into_iter()
                    .map(|id| {
                        let n = v.store.node(id);
                        let chevron = if !n.has_children {
                            " "
                        } else if n.expanded {
                            "▾"
                        } else {
                            "▸"
                        };
                        // Paused: the store has the preview of the snapshot.
                        let preview: SharedString = match (n.preview, n.latest) {
                            (Some(p), _) => p.to_string().into(),
                            (None, Some(m)) => preview(&m.payload, 80).into(),
                            (None, None) => SharedString::default(),
                        };
                        let selected = v.sel_topic == Some(id);
                        h_flex()
                            .id(("row", id.0 as usize))
                            .h(px(ROW_HEIGHT))
                            .px_2()
                            .gap_2()
                            .text_sm()
                            .when(selected, |d| d.bg(theme.accent))
                            .hover(|d| d.bg(theme.muted))
                            .child(div().w(px(n.depth as f32 * 14.)))
                            .child(div().w(px(12.)).child(chevron))
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
                                    .max_w(px(320.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(preview),
                            )
                            .child(
                                div()
                                    .w(px(72.))
                                    .flex()
                                    .justify_end()
                                    .font_family(theme.mono_font_family.clone())
                                    .child(n.subtree_msgs.to_string()),
                            )
                            .on_click(cx.listener(move |v, _, window, cx| {
                                v.list_focus.focus(window, cx);
                                v.select(id, generation, cx)
                            }))
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.tree_scroll)
        .flex_1()
        .into_any_element()
    }

    /// Formatted payload, built once for each message and capped in size.
    fn detail_text(&mut self, key: DetailKey, payload: &Bytes) -> SharedString {
        match &self.detail_cache {
            Some((k, text)) if *k == key => text.clone(),
            _ => {
                let text: SharedString = text_or_hex(payload, DETAIL_MAX).into();
                self.detail_cache = Some((key, text.clone()));
                text
            }
        }
    }

    fn meta_text(&self, m: &Msg, direction: Direction) -> String {
        format!(
            "{} · {}QoS {} · {}{} B{}",
            format_time(m.received_at, self.utc_offset, true),
            if direction == Direction::Sent {
                "↑ Sent · "
            } else {
                ""
            },
            m.qos.as_u8(),
            if m.retain { "Retained · " } else { "" },
            m.original_size,
            if m.truncated { " · truncated" } else { "" },
        )
    }

    fn render_details(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // What to show: topic, meta line and payload. The Messages view shows the selected
        // message. The Tree view shows the latest value of the selected topic.
        let mut evicted = false;
        let shown: Option<(String, Option<(String, SharedString)>)> = match self.detail_of {
            DetailOf::Nothing => None,
            DetailOf::Message => self.sel_msg.take().map(|s| {
                evicted = self.store.row_evicted(s.id);
                let text = self.detail_text(DetailKey::Row(s.id), &s.msg.payload);
                let shown = (
                    s.topic.to_string(),
                    Some((self.meta_text(&s.msg, s.direction), text)),
                );
                self.sel_msg = Some(s);
                shown
            }),
            DetailOf::Topic => self.sel_topic.and_then(|id| {
                let n = self.store.get(id)?;
                let path = self.store.path(id);
                let Some(m) = n.latest.cloned() else {
                    return Some((path, None));
                };
                let key = DetailKey::Node(id, m.received_at, m.payload.len(), m.truncated);
                let text = self.detail_text(key, &m.payload);
                Some((path, Some((self.meta_text(&m, Direction::Received), text))))
            }),
        };
        let theme = cx.theme();
        let hint = |t: &'static str| div().text_color(theme.muted_foreground).child(t);
        let body = match shown {
            None => hint("Select a topic with a message.").into_any_element(),
            Some((path, value)) => {
                let head = div()
                    .font_family(theme.mono_font_family.clone())
                    .child(path);
                match value {
                    None => v_flex()
                        .gap_2()
                        .child(head)
                        .child(hint("Select a topic with a message."))
                        .into_any_element(),
                    Some((meta, text)) => v_flex()
                        .gap_2()
                        .child(head)
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(meta),
                        )
                        .when(evicted, |d| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(theme.warning)
                                    .child("Removed from the message list (memory limit)."),
                            )
                        })
                        .child(
                            div()
                                .font_family(theme.mono_font_family.clone())
                                .text_sm()
                                .child(text),
                        )
                        .into_any_element(),
                }
            }
        };
        v_flex()
            .w(px(360.))
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .p_3()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("MESSAGE"),
            )
            .child(body)
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let qos_button = |q: Qos| {
            let label = format!("QoS {}", q.as_u8());
            let b = Button::new(("qos", q.as_u8() as usize))
                .small()
                .label(label);
            let b = if self.qos == q {
                b.primary()
            } else {
                b.ghost()
            };
            b.on_click(cx.listener(move |v, _, _, cx| {
                v.qos = q;
                cx.notify();
            }))
        };
        let result = self
            .last_result
            .as_ref()
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        v_flex()
            .border_t_1()
            .border_color(theme.border)
            .p_2()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("PUBLISH"),
                    )
                    .child(div().flex_1().child(Input::new(&self.topic_input).small())),
            )
            .child(Input::new(&self.payload_input).small())
            .child(
                h_flex()
                    .gap_2()
                    .child(qos_button(Qos::AtMostOnce))
                    .child(qos_button(Qos::AtLeastOnce))
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(if self.connected() {
                                result
                            } else {
                                "Connect to a broker first.".into()
                            }),
                    )
                    .child(
                        Button::new("send")
                            .small()
                            .primary()
                            .label("Send")
                            .disabled(!self.connected())
                            .on_click(cx.listener(|v, _, _, cx| v.send(cx))),
                    ),
            )
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
        let theme = cx.theme();
        let mut left = h_flex().gap_3().text_xs().child(self.url());
        if !self.profile.transport.is_encrypted() {
            left = left.child(div().text_color(theme.warning).child("Not encrypted"));
        } else if !self.profile.verify_tls {
            left = left.child(div().text_color(theme.warning).child("Insecure TLS"));
        }
        if self.reconnected_at.is_some() {
            left = left.child("Reconnected");
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
                .child(format!(
                    "{} / {} MiB",
                    s.used_bytes / MIB,
                    Limits::default().budget / MIB
                ))
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
        let chips = self.render_chips(cx);
        let theme = cx.theme();
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .key_context(CONTEXT)
            .track_focus(&self.root_focus)
            .on_action(cx.listener(|v, _: &ShowMessages, _, cx| v.set_view(View::Messages, cx)))
            .on_action(cx.listener(|v, _: &ShowTree, _, cx| v.set_view(View::Tree, cx)))
            .on_action(cx.listener(|v, _: &FocusFilter, window, cx| {
                v.filter_input.update(cx, |i, cx| i.focus(window, cx));
            }))
            .child(self.render_title(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_subs(cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_header(cx))
                            .children(chips)
                            .child(self.render_list_area(cx))
                            .child(self.render_composer(cx)),
                    )
                    .child(self.render_details(cx)),
            )
            .child(self.render_status(cx))
    }
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

/// One-line preview: whitespace collapsed, control characters as `·`, invalid UTF-8 as hex.
fn preview(payload: &[u8], max_chars: usize) -> String {
    let head = &payload[..payload.len().min(512)];
    let Some(text) = utf8_prefix(head) else {
        let hex: Vec<String> = head.iter().take(16).map(|b| format!("{b:02x}")).collect();
        return format!("bin {}", hex.join(" "));
    };
    let mut out = String::with_capacity(max_chars + 1);
    let mut last_space = false;
    for (i, c) in text.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            return out;
        }
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(if c.is_control() { '·' } else { c });
            last_space = false;
        }
    }
    if payload.len() > head.len() {
        out.push('…');
    }
    out
}

/// Valid UTF-8 prefix of a slice that may end inside a character. `None` if the data is not text.
fn utf8_prefix(b: &[u8]) -> Option<&str> {
    match std::str::from_utf8(b) {
        Ok(s) => Some(s),
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&b[..e.valid_up_to()]).ok(),
        Err(_) => None,
    }
}

/// Most payload bytes the details pane formats. P5 adds "Show all" and background formatting.
// ponytail: fixed cap on the UI thread; P5 moves large payloads to a cancellable background job.
const DETAIL_MAX: usize = 64 * 1024;

/// Payload as text, or as a hex dump when it is not UTF-8. At most `max` bytes are formatted.
fn text_or_hex(payload: &[u8], max: usize) -> String {
    use std::fmt::Write as _;
    let head = &payload[..payload.len().min(max)];
    let more = payload.len() > head.len();
    let mut out = match utf8_prefix(head) {
        Some(text) => text.to_string(),
        None => {
            let mut hex = String::with_capacity(head.len() * 3);
            for (i, b) in head.iter().enumerate() {
                let sep = if i == 0 { "" } else { " " };
                let _ = write!(hex, "{sep}{b:02x}");
            }
            hex
        }
    };
    if more {
        let _ = write!(out, "\n… {} more bytes", payload.len() - head.len());
    }
    out
}

/// A closed receiver, used to take the real receivers out of `Net`.
fn tokio_channel_placeholder<T>() -> tokio::sync::mpsc::Receiver<T> {
    tokio::sync::mpsc::channel(1).1
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the gpui glob import brings its own `test` macro.
    use super::{
        Sub, SubState, Subs, preview, scroll_top_of, send_error_text, top_row, validate_filter,
    };
    use navigator_core::Qos;
    use navigator_core::net::SendError;

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

    #[test]
    fn preview_collapses_whitespace_and_marks_binary() {
        assert_eq!(preview(b"{\n  \"v\": 1\n}", 80), "{ \"v\": 1 }");
        assert_eq!(preview(&[0xff, 0x00, 0x10], 80), "bin ff 00 10");
        assert_eq!(preview("abcdef".as_bytes(), 3), "abc…");
        // A cut inside a multi-byte character is not binary.
        let long = "é".repeat(400);
        assert!(!preview(long.as_bytes(), 80).starts_with("bin"));
    }

    fn subs(filters: &[&str]) -> Subs {
        Subs {
            rows: filters
                .iter()
                .map(|f| Sub {
                    filter: f.to_string(),
                    qos: Qos::AtMostOnce,
                    state: SubState::Active(Qos::AtMostOnce),
                })
                .collect(),
        }
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
}
