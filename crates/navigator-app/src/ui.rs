//! Main window. P1: title bar, subscriptions, live topic tree, latest value, composer, status bar.

use std::time::{Duration, Instant};

use bytes::Bytes;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::{
    ActiveTheme, Disableable, IconName, Sizable, Theme, TitleBar, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use navigator_core::Qos;
use navigator_core::net::{
    Command, ConnState, Event, Net, NetLimits, Outgoing, Profile, PublishResult, SendError, Status,
};
use navigator_core::store::{Limits, NodeId, TopicStore};

/// Identifies one stored value: node, receive time, stored length, truncated.
type DetailKey = (NodeId, std::time::SystemTime, usize, bool);

/// Most merge work on the UI thread in one frame.
const FRAME_BUDGET: Duration = Duration::from_millis(4);
/// Wait between merges, so that many messages make one render.
const COALESCE: Duration = Duration::from_millis(33);
const ROW_HEIGHT: f32 = 24.;

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
    selected: Option<NodeId>,
    /// Connection generation of the shown data. Newer data from the net thread replaces it.
    generation: u64,
    /// Formatted payload of the selected message, with the key it was built for.
    detail_cache: Option<(DetailKey, SharedString)>,
    scroll: UniformListScrollHandle,
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

        Self::spawn_ingest(incoming, cx);
        Self::spawn_events(events, cx);
        Self::spawn_status(status_rx, cx);

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
        let this = Self {
            profile,
            net,
            store: TopicStore::new(Limits::default()),
            status: Status::default(),
            subs,
            selected: None,
            generation: 0,
            detail_cache: None,
            scroll: UniformListScrollHandle::new(),
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
                cx.on_app_quit(|v: &mut Self, _| v.net.shutdown()),
            ],
        };
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
        cx.notify();
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

    /// Waits for the first message, merges a batch within the frame budget, then coalesces.
    fn spawn_ingest(
        mut rx: tokio::sync::mpsc::Receiver<navigator_core::Incoming>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(first) = rx.recv().await {
                let line = this.update(cx, |v, cx| {
                    let line = v.merge(first, &mut rx);
                    cx.notify();
                    v.schedule_rate_zero(cx);
                    line
                });
                match line {
                    Err(_) => break,
                    Ok(Some(line)) => cx.background_executor().spawn(write_metrics(line)).detach(),
                    Ok(None) => {}
                }
                cx.background_executor().timer(COALESCE).await;
            }
        })
        .detach();
    }

    fn merge(
        &mut self,
        first: navigator_core::Incoming,
        rx: &mut tokio::sync::mpsc::Receiver<navigator_core::Incoming>,
    ) -> Option<String> {
        let start = Instant::now();
        let mut n = 0u64;
        let mut metrics_line = None;
        let mut next = Some(first);
        while let Some(msg) = next.take() {
            self.net.release(&msg);
            if self.accept(msg.generation) {
                self.store.insert(msg);
                n += 1;
            }
            if start.elapsed() < FRAME_BUDGET {
                next = rx.try_recv().ok();
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
        if !self.accept(g) {
            return;
        }
        match ev {
            Event::Reconnected { at, .. } => {
                tracing::info!("Reconnected");
                self.reconnected_at = Some(at);
            }
            Event::Subscribed { filter, result, .. } => {
                if let Err(why) = &result {
                    tracing::warn!(%why, "Subscribe failed");
                }
                self.subs.on_subscribed(&filter, result);
            }
            Event::Published { op_id, result, .. } => {
                let text = match result {
                    PublishResult::Sent => "Sent".to_string(),
                    PublishResult::Acknowledged => "Acknowledged".to_string(),
                    PublishResult::Rejected(r) => format!("Rejected: {r}"),
                    PublishResult::TimedOut => "No acknowledgement after 10 s.".to_string(),
                    PublishResult::LostOnDisconnect => {
                        "Not sent: the connection closed.".to_string()
                    }
                };
                // The line shows the newest send only. A late result of an older send is ignored.
                if self
                    .last_result
                    .as_ref()
                    .is_some_and(|(id, _)| *id == op_id)
                {
                    self.last_result = Some((op_id, text));
                }
            }
            Event::Error { message, .. } => self.last_result = Some((0, message)),
            // An UNSUBSCRIBE result shows on its row, not in the publish line.
            Event::Unsubscribed { filter, result, .. } => {
                self.subs.on_unsubscribed(&filter, result)
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
        let out = Outgoing {
            op_id,
            topic,
            payload: Bytes::from(payload),
            qos: self.qos,
            retain: false,
            props: Default::default(),
        };
        self.last_result = match self.net.send(Command::Publish(Box::new(out))) {
            Ok(()) => Some((op_id, "Sending…".into())),
            Err(_) => Some((op_id, "Too many messages are waiting. Try again.".into())),
        };
        cx.notify();
    }

    /// `generation` is the data generation when the row was rendered. A click on a row of an
    /// older generation does nothing, because its `NodeId` may now name another topic.
    fn select(&mut self, id: NodeId, generation: u64, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        let Some(node) = self.store.get(id) else {
            return;
        };
        let (has_children, expanded) = (node.has_children, node.expanded);
        self.selected = Some(id);
        if has_children {
            self.store.set_expanded(id, !expanded);
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
        let rows = self.subs.rows.iter().map(|s| {
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
            let (id_retry, id_remove) = (
                SharedString::from(format!("retry:{}", s.filter)),
                SharedString::from(format!("remove:{}", s.filter)),
            );
            v_flex()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .font_family(theme.mono_font_family.clone())
                                .text_sm()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(s.filter.clone()),
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

    fn render_tree(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.store.rows().len();
        let theme = cx.theme();
        if count == 0 {
            let text = if self.connected() {
                "No messages yet. Messages show here when the broker sends them."
            } else {
                "Connect to a broker to see messages."
            };
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child(text)
                .into_any_element();
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
                        let preview: SharedString = n
                            .latest
                            .map(|m| preview(&m.payload, 80))
                            .unwrap_or_default()
                            .into();
                        let selected = v.selected == Some(id);
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
                            .on_click(cx.listener(move |v, _, _, cx| v.select(id, generation, cx)))
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.scroll)
        .flex_1()
        .into_any_element()
    }

    /// Formatted payload of the selected node, built once per message and capped in size.
    fn detail_text(&mut self, id: NodeId) -> Option<SharedString> {
        let m = self.store.get(id)?.latest?;
        // Eviction empties a payload but keeps `received_at`, so the length and the flag are in
        // the key too.
        let key = (id, m.received_at, m.payload.len(), m.truncated);
        match &self.detail_cache {
            Some((k, text)) if *k == key => Some(text.clone()),
            _ => {
                let text: SharedString = text_or_hex(&m.payload, DETAIL_MAX).into();
                self.detail_cache = Some((key, text.clone()));
                Some(text)
            }
        }
    }

    fn render_details(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let text = self.selected.and_then(|id| self.detail_text(id));
        let theme = cx.theme();
        let node = self.selected.and_then(|id| Some((id, self.store.get(id)?)));
        let body = match node {
            None => div()
                .text_color(theme.muted_foreground)
                .child("Select a topic with a message.")
                .into_any_element(),
            Some((id, n)) => {
                let path = self.store.path(id);
                match n.latest {
                    None => v_flex()
                        .gap_2()
                        .child(
                            div()
                                .font_family(theme.mono_font_family.clone())
                                .child(path),
                        )
                        .child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child("Select a topic with a message."),
                        )
                        .into_any_element(),
                    Some(m) => {
                        let meta = format!(
                            "QoS {} · {}{} B{}",
                            m.qos.as_u8(),
                            if m.retain { "Retained · " } else { "" },
                            m.original_size,
                            if m.truncated { " · truncated" } else { "" },
                        );
                        v_flex()
                            .gap_2()
                            .child(
                                div()
                                    .font_family(theme.mono_font_family.clone())
                                    .child(path),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(meta),
                            )
                            .child(
                                div()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_sm()
                                    .child(text.clone().unwrap_or_default()),
                            )
                            .into_any_element()
                    }
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
                .child(format!("{} / 256 MiB", s.used_bytes / (1024 * 1024)))
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
        let theme = cx.theme();
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
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
                            .child(self.render_tree(cx))
                            .child(self.render_composer(cx)),
                    )
                    .child(self.render_details(cx)),
            )
            .child(self.render_status(cx))
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
    use super::{Sub, SubState, Subs, preview, send_error_text, validate_filter};
    use navigator_core::Qos;
    use navigator_core::net::SendError;

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
