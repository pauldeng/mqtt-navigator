//! The "net" thread: one tokio `current_thread` runtime that owns the MQTT client.
//!
//! The UI talks to it only through [`Net`]. The net thread never waits for the UI. See the
//! "Threads and data flow" section of the plan, ADR 0002 and ARCHITECTURE.md.

use std::cell::Cell;
use std::future::Future;
use std::num::{NonZeroU16, NonZeroU32};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use ms_mqtt_client::client::{
    Client, ClientOptions, ConnectResult, DisconnectedEvent, KeepAliveConfig, new_client,
};
use ms_mqtt_client::error::ConnectError;
use ms_mqtt_client::local_limits;
use ms_mqtt_client::packet::{
    ConnAckReason, ConnectProperties, DeliveryQoS, DisconnectProperties, PayloadFormatIndicator,
    Publish, PublishProperties, QoS as WireQos, RetainOptions, SubAckReason, SubscribeProperties,
    UnsubscribeProperties,
};
use ms_mqtt_client::topic::{TopicFilter, TopicName};
use ms_mqtt_client::transport::{
    ConnectionTransportConfig, ConnectionTransportType, IntoWsRequest as _, TlsConfig,
};
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode, SslVersion};
use openssl::x509::X509;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::{JoinSet, LocalSet};

use crate::model::{Incoming, MsgProps, Qos};

/// Transport. TLS is the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transport {
    #[default]
    Tls,
    Tcp,
    Wss,
    Ws,
}

impl Transport {
    pub fn default_port(self) -> u16 {
        match self {
            Self::Tls => 8883,
            Self::Tcp => 1883,
            Self::Wss => 443,
            Self::Ws => 80,
        }
    }

    pub fn is_encrypted(self) -> bool {
        matches!(self, Self::Tls | Self::Wss)
    }
}

/// Connection settings for one connect. Secrets are in memory only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub host: String,
    pub port: u16,
    pub transport: Transport,
    /// WebSocket path, for example `/mqtt`.
    pub ws_path: String,
    /// `None`: the broker assigns a client ID.
    pub client_id: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Extra CA certificates (PEM), added to the system roots.
    pub ca_file: Option<PathBuf>,
    /// Client certificate chain and key (PEM), for mTLS.
    pub client_cert: Option<(PathBuf, PathBuf)>,
    /// Verify the server certificate and host name. Default true.
    pub verify_tls: bool,
    pub keepalive_secs: u16,
    /// Filters to subscribe after each connect.
    pub subscriptions: Vec<(String, Qos)>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: Transport::Tls.default_port(),
            transport: Transport::Tls,
            ws_path: "/mqtt".into(),
            client_id: None,
            username: None,
            password: None,
            ca_file: None,
            client_cert: None,
            verify_tls: true,
            keepalive_secs: 60,
            subscriptions: vec![("#".into(), Qos::AtMostOnce)],
        }
    }
}

/// One message to send.
#[derive(Clone, Debug)]
pub struct Outgoing {
    /// Chosen by the UI. Results carry it back.
    pub op_id: u64,
    pub topic: String,
    pub payload: Bytes,
    pub qos: Qos,
    pub retain: bool,
    pub props: MsgProps,
}

/// Requests from the UI. Send them with [`Net::send`]. Disconnect is not here: see
/// [`Net::disconnect`].
#[derive(Clone, Debug)]
pub enum Command {
    Connect(Box<Profile>),
    Subscribe { filter: String, qos: Qos },
    Unsubscribe { filter: String },
    Publish(Box<Outgoing>),
}

/// Connection state. P2 adds `Reconnecting`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ConnState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
    /// The connection ended with an error. The text is for the user.
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Status {
    pub generation: u64,
    pub state: ConnState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishResult {
    /// QoS 0: released for transmission. There is no acknowledgement.
    Sent,
    /// QoS 1: PUBACK success (includes "no matching subscribers").
    Acknowledged,
    /// QoS 1: PUBACK with an error reason, as text for the user.
    Rejected(String),
    TimedOut,
    LostOnDisconnect,
}

/// Results and errors. Bounded log: when full, new events are dropped (P1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// SUBACK result: granted QoS, or the reason as text.
    Subscribed {
        generation: u64,
        filter: String,
        result: Result<Qos, String>,
    },
    Unsubscribed {
        generation: u64,
        filter: String,
    },
    Published {
        generation: u64,
        op_id: u64,
        result: PublishResult,
    },
    /// An error the user must see, for example an invalid filter.
    Error {
        generation: u64,
        message: String,
    },
}

/// Counters shared with the UI.
#[derive(Debug, Default)]
pub struct NetStats {
    /// Messages dropped because a queue was full (both the client queue and ours).
    pub drops: AtomicU64,
}

/// Queue limits of the net thread.
#[derive(Clone, Copy, Debug)]
pub struct NetLimits {
    /// Largest incoming packet, in bytes. Also advertised as Maximum Packet Size.
    pub max_packet: usize,
    /// Capacity of the queue to the UI, in messages.
    pub queue_len: usize,
    /// Capacity of the queue to the UI, in bytes. Each message costs [`queue_cost`]: payload,
    /// topic, properties and a fixed overhead.
    pub queue_bytes: usize,
}

impl Default for NetLimits {
    fn default() -> Self {
        Self {
            max_packet: 2 * 1024 * 1024,
            queue_len: 50_000,
            queue_bytes: 32 * 1024 * 1024,
        }
    }
}

/// Cancel value that means "stop the thread".
const SHUTDOWN: u64 = u64::MAX;
/// Time limit for the transport set-up and for the CONNACK.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Time limit for one publish, subscribe or unsubscribe, from submit to the acknowledgement.
/// A timeout ends the session, because the client keeps the operation and its payload.
/// Unit tests use a short limit.
#[cfg(not(test))]
const OP_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const OP_TIMEOUT: Duration = Duration::from_millis(300);
/// Time that a session gets to write DISCONNECT before we abort it.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Time that the open operations get to report "lost" after the connection ended.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// Largest number of publish, subscribe and unsubscribe operations in flight at one time.
const MAX_PENDING_OPS: usize = 256;
const TOO_MANY_OPS_TEXT: &str = "Too many messages are waiting. Try again.";
const NO_ANSWER_TEXT: &str = "No answer from the broker after 10 s.";
const OP_TIMEOUT_END_TEXT: &str = "The broker did not answer for 10 s. Connect again.";
const PANIC_TEXT: &str = "The MQTT library stopped unexpectedly. Connect again.";

/// Why [`Net::send`] did not queue a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// The command queue is full. Try again later.
    #[error("Too many pending requests.")]
    Full,
    /// The net thread has ended.
    #[error("The network thread has stopped.")]
    Closed,
}

/// Memory that one queued message costs: payload, topic, properties and a fixed overhead.
///
/// The net thread charges this value when it queues a message. [`Net::release`] frees the same
/// value. Both use this function, so the two can never differ.
pub fn queue_cost(msg: &Incoming) -> usize {
    std::mem::size_of::<Incoming>() + msg.payload.len() + msg.topic.len() + msg.props.heap_bytes()
}

/// The UI side of the net thread.
pub struct Net {
    /// Messages for the UI. Call [`Net::release`] for each one that you take.
    pub incoming: mpsc::Receiver<Incoming>,
    pub status: watch::Receiver<Status>,
    pub events: mpsc::Receiver<Event>,
    pub stats: Arc<NetStats>,
    commands: mpsc::Sender<(u64, Command)>,
    /// Cancel epoch: counts disconnect requests. `SHUTDOWN` stops the thread. Each command
    /// carries the epoch at the time of [`Net::send`]. A cancel makes all older commands void.
    cancel: watch::Sender<u64>,
    /// Bytes ([`queue_cost`]) that are in `incoming` and not yet released.
    queued_bytes: Arc<AtomicUsize>,
    /// Becomes true when the net thread has ended.
    done: watch::Receiver<bool>,
}

impl Net {
    /// Starts the net thread. It waits for `Command::Connect`.
    pub fn start(limits: NetLimits) -> Self {
        let (commands, cmd_rx) = mpsc::channel(64);
        let (incoming_tx, incoming) = mpsc::channel(limits.queue_len.max(1));
        let (events_tx, events) = mpsc::channel(256);
        let (status_tx, status) = watch::channel(Status::default());
        let (cancel, cancel_rx) = watch::channel(0u64);
        let stats = Arc::new(NetStats::default());
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let (done_tx, done) = watch::channel(false);

        let parts = (
            incoming_tx,
            events_tx,
            status_tx.clone(),
            stats.clone(),
            queued_bytes.clone(),
        );
        // The thread is detached. `done_tx` is dropped if the thread cannot start or dies, and
        // that also ends `shutdown`.
        let thread = std::thread::Builder::new()
            .name("mqtt-net".into())
            .spawn(move || {
                let (incoming, events, status, stats, queued_bytes) = parts;
                let shared = Rc::new(Shared {
                    incoming,
                    events,
                    status,
                    stats,
                    queued_bytes,
                    limits,
                });
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        shared.set_status(
                            0,
                            ConnState::Failed(format!("Cannot start the network thread: {e}")),
                        );
                        return;
                    }
                };
                LocalSet::new().block_on(&rt, supervisor(cmd_rx, cancel_rx, shared));
                done_tx.send_replace(true);
            });
        if let Err(e) = thread {
            status_tx.send_replace(Status {
                generation: 0,
                state: ConnState::Failed(format!("Cannot start the network thread: {e}")),
            });
        }
        Self {
            commands,
            incoming,
            status,
            events,
            stats,
            cancel,
            queued_bytes,
            done,
        }
    }

    /// Queues a command. Never blocks. Returns [`SendError::Full`] if 64 commands are waiting.
    ///
    /// A [`Net::disconnect`] voids every command that was sent before it, even if the net
    /// thread has not read the command yet. A rejected command is answered with an event.
    pub fn send(&self, cmd: Command) -> Result<(), SendError> {
        let epoch = *self.cancel.borrow();
        self.commands.try_send((epoch, cmd)).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => SendError::Full,
            mpsc::error::TrySendError::Closed(_) => SendError::Closed,
        })
    }

    /// Asks for a clean disconnect. Never blocks. Works even if the command queue is full.
    /// Commands that were sent before this call do not run.
    pub fn disconnect(&self) {
        self.cancel.send_modify(|v| {
            if *v != SHUTDOWN {
                *v += 1;
            }
        });
    }

    /// Stops the net thread after a clean disconnect. Returns at once with a future that is
    /// ready when the thread has finished (it can take up to 5 s). The future does not borrow
    /// `self`. The UI awaits it, with its own time limit, in the quit handler.
    ///
    /// After this call the connection cannot be started again.
    pub fn shutdown(&self) -> impl Future<Output = ()> + 'static + use<> {
        self.cancel.send_replace(SHUTDOWN);
        let mut done = self.done.clone();
        async move {
            // An error means that the thread dropped the sender, so it has ended.
            let _ = done.wait_for(|d| *d).await;
        }
    }

    /// Call when the UI takes a message from `incoming`, to free its bytes in the queue budget.
    /// Pass the message unchanged: it frees exactly what [`queue_cost`] charged.
    pub fn release(&self, msg: &Incoming) {
        let n = queue_cost(msg);
        // The update closure never returns `None`, so this cannot fail.
        let _ = self
            .queued_bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                Some(v.saturating_sub(n))
            });
    }

    /// Bytes ([`queue_cost`]) in `incoming` that the UI has not released yet.
    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::Acquire)
    }
}

impl Drop for Net {
    /// Signals the stop and detaches the thread. It never waits. Dropping the command sender
    /// closes the command queue. To wait for the clean disconnect, await [`Net::shutdown`]
    /// before the drop.
    fn drop(&mut self) {
        self.cancel.send_replace(SHUTDOWN);
    }
}

// ---------------------------------------------------------------------------------------------
// Net thread
// ---------------------------------------------------------------------------------------------

/// State that all tasks on the net thread share.
struct Shared {
    incoming: mpsc::Sender<Incoming>,
    events: mpsc::Sender<Event>,
    status: watch::Sender<Status>,
    stats: Arc<NetStats>,
    queued_bytes: Arc<AtomicUsize>,
    limits: NetLimits,
}

impl Shared {
    fn set_status(&self, generation: u64, state: ConnState) {
        self.status.send_replace(Status { generation, state });
    }

    /// Connecting to Connected, unless a stop request already moved the state on.
    fn set_connected(&self, generation: u64) {
        self.status.send_if_modified(|s| {
            let ok = s.generation == generation && s.state == ConnState::Connecting;
            if ok {
                s.state = ConnState::Connected;
            }
            ok
        });
    }

    /// The event log is bounded. A full log drops the new event.
    fn event(&self, event: Event) {
        let _ = self.events.try_send(event);
    }
}

/// How a session task ended.
enum SessionEnd {
    Disconnected,
    Failed(String),
}

struct Current {
    generation: u64,
    handle: tokio::task::JoinHandle<SessionEnd>,
    commands: mpsc::Sender<Command>,
    stop: Rc<Notify>,
    /// Cancel epoch under which the session started. A cancel stops only older sessions.
    epoch: u64,
}

fn end_state(end: SessionEnd) -> ConnState {
    match end {
        SessionEnd::Disconnected => ConnState::Disconnected,
        SessionEnd::Failed(text) => ConnState::Failed(text),
    }
}

fn join_state(result: Result<SessionEnd, tokio::task::JoinError>) -> ConnState {
    match result {
        Ok(end) => end_state(end),
        Err(e) if e.is_panic() => ConnState::Failed(PANIC_TEXT.into()),
        Err(_) => ConnState::Failed("The connection task was cancelled.".into()),
    }
}

/// Owns the state machine. Starts, stops and joins sessions.
async fn supervisor(
    mut commands: mpsc::Receiver<(u64, Command)>,
    mut cancel: watch::Receiver<u64>,
    shared: Rc<Shared>,
) {
    let mut generation = 0u64;
    let mut current: Option<Current> = None;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None => break,
                // A disconnect came after this command was sent. The command is void.
                Some((epoch, command)) if epoch < *cancel.borrow() => {
                    reject(&shared, generation, command, "Not connected.");
                }
                Some((epoch, Command::Connect(profile))) => {
                    if let Some(old) = current.take() {
                        stop_session(old, &shared).await;
                        // A disconnect can come while we stop the old session.
                        if epoch < *cancel.borrow() {
                            continue;
                        }
                    }
                    generation += 1;
                    shared.set_status(generation, ConnState::Connecting);
                    let (tx, rx) = mpsc::channel(256);
                    let stop = Rc::new(Notify::new());
                    let handle = tokio::task::spawn_local(run_session(
                        profile,
                        generation,
                        shared.clone(),
                        rx,
                        stop.clone(),
                    ));
                    current = Some(Current { generation, handle, commands: tx, stop, epoch });
                }
                Some((_, other)) => match &current {
                    Some(c) => {
                        if let Err(e) = c.commands.try_send(other) {
                            reject(&shared, c.generation, e.into_inner(), "Too many pending requests.");
                        }
                    }
                    None => reject(&shared, generation, other, "Not connected."),
                },
            },
            changed = cancel.changed() => {
                let value = if changed.is_ok() { *cancel.borrow_and_update() } else { SHUTDOWN };
                // This cancel is older than the session. A Connect that was sent after the
                // disconnect can be read before the cancel. Do not stop that session.
                if value != SHUTDOWN && current.as_ref().is_some_and(|c| c.epoch >= value) {
                    continue;
                }
                if let Some(c) = current.take() {
                    stop_session(c, &shared).await;
                } else if matches!(shared.status.borrow().state, ConnState::Failed(_)) {
                    shared.set_status(generation, ConnState::Disconnected);
                }
                if value == SHUTDOWN {
                    break;
                }
            },
            result = async {
                match current.as_mut() {
                    Some(c) => (&mut c.handle).await,
                    None => std::future::pending().await,
                }
            } => {
                // The session ended by itself (error, or the broker closed the connection).
                // After a panic nothing of the old client is kept: its handles died with the task.
                current = None;
                shared.set_status(generation, join_state(result));
            },
        }
    }
    if let Some(c) = current.take() {
        stop_session(c, &shared).await;
    }
}

/// Requests a clean disconnect and waits (bounded) for the session to end.
async fn stop_session(mut current: Current, shared: &Shared) {
    shared.set_status(current.generation, ConnState::Disconnecting);
    current.stop.notify_one();
    let state = match tokio::time::timeout(STOP_TIMEOUT, &mut current.handle).await {
        Ok(Err(e)) if e.is_panic() => ConnState::Failed(PANIC_TEXT.into()),
        Ok(_) => ConnState::Disconnected,
        Err(_) => {
            current.handle.abort();
            let _ = current.handle.await;
            ConnState::Disconnected
        }
    };
    shared.set_status(current.generation, state);
}

/// Answers a command that cannot run.
fn reject(shared: &Shared, generation: u64, command: Command, why: &str) {
    match command {
        Command::Publish(o) => shared.event(Event::Published {
            generation,
            op_id: o.op_id,
            result: PublishResult::Rejected(why.into()),
        }),
        Command::Subscribe { filter, .. } => shared.event(Event::Subscribed {
            generation,
            filter,
            result: Err(why.into()),
        }),
        Command::Unsubscribe { filter } => shared.event(Event::Error {
            generation,
            message: format!("Cannot unsubscribe from {filter}: {why}"),
        }),
        Command::Connect(_) => {}
    }
}

/// One connection, from connect to disconnect. Runs in a task, so a panic in the MQTT library
/// ends only this session.
async fn run_session(
    profile: Box<Profile>,
    generation: u64,
    shared: Rc<Shared>,
    mut commands: mpsc::Receiver<Command>,
    stop: Rc<Notify>,
) -> SessionEnd {
    let end = session(&profile, generation, &shared, &mut commands, &stop).await;
    commands.close();
    while let Ok(command) = commands.try_recv() {
        reject(&shared, generation, command, "Not connected.");
    }
    end
}

async fn session(
    profile: &Profile,
    generation: u64,
    shared: &Rc<Shared>,
    commands: &mut mpsc::Receiver<Command>,
    stop: &Notify,
) -> SessionEnd {
    local_limits::set_limits(
        shared.limits.max_packet,
        shared.limits.queue_len,
        shared.limits.queue_bytes,
    );
    let transport = match transport(profile) {
        Ok(t) => t,
        Err(text) => return SessionEnd::Failed(text),
    };
    let (client, connect_handle, mut receiver) = new_client(ClientOptions {
        client_id: profile.client_id.clone(),
        ..Default::default()
    });
    let keep_alive = match NonZeroU16::new(profile.keepalive_secs) {
        Some(ping_after) => KeepAliveConfig::Duration {
            ping_after,
            response_timeout: CONNECT_TIMEOUT,
        },
        None => KeepAliveConfig::Infinite,
    };
    let max_packet = u32::try_from(shared.limits.max_packet).unwrap_or(u32::MAX);
    let connect = connect_handle.connect(
        ConnectionTransportConfig {
            transport_type: transport,
            timeout: Some(CONNECT_TIMEOUT),
            proxy: None,
            tcp_nodelay: true,
        },
        true,
        keep_alive,
        None,
        profile.username.clone(),
        profile
            .password
            .as_ref()
            .map(|p| Bytes::copy_from_slice(p.as_bytes())),
        ConnectProperties {
            maximum_packet_size: NonZeroU32::new(max_packet).unwrap_or(NonZeroU32::MAX),
            ..Default::default()
        },
        Some(CONNECT_TIMEOUT),
    );
    let result = tokio::select! {
        result = connect => result,
        () = stop.notified() => return SessionEnd::Disconnected,
    };
    let (connection, disconnect_handle) = match result {
        ConnectResult::Success(connection, _, handle) => (connection, handle),
        ConnectResult::Failure(_, error) => return SessionEnd::Failed(connect_error_text(&error)),
    };
    shared.set_connected(generation);

    let mut ops = JoinSet::new();
    // An operation that times out reports it here. The session then ends.
    let expired = Rc::new(Notify::new());
    for (filter, qos) in &profile.subscriptions {
        let subscribe = Command::Subscribe {
            filter: filter.clone(),
            qos: *qos,
        };
        dispatch(&client, subscribe, generation, shared, &expired, &mut ops);
    }

    let mut ingest = Ingest {
        shared: shared.clone(),
        generation,
        last_drops: Cell::new(local_limits::dropped()),
    };
    let mut driver = Box::pin(connection.run_until_disconnect());
    let mut disconnect_handle = Some(disconnect_handle);
    let (mut receiver_open, mut commands_open) = (true, true);
    let ended = loop {
        tokio::select! {
            result = &mut driver => break Some(result),
            () = expired.notified() => break None,
            message = receiver.recv(), if receiver_open => match message {
                // Dropping the acknowledgement token acknowledges a QoS 1 message.
                Some((publish, _ack)) => ingest.publish(publish),
                None => receiver_open = false,
            },
            command = commands.recv(), if disconnect_handle.is_some() && commands_open => match command {
                Some(command) => dispatch(&client, command, generation, shared, &expired, &mut ops),
                None => commands_open = false,
            },
            () = stop.notified(), if disconnect_handle.is_some() => {
                if let Some(handle) = disconnect_handle.take() {
                    let _ = handle.disconnect(&DisconnectProperties::default());
                }
            },
            Some(_) = ops.join_next(), if !ops.is_empty() => {},
        }
    };
    ingest.sync_drops();
    // Dropping the client side ends the open operations. Let them report "lost".
    let Some((handle, event)) = ended else {
        // An operation timed out and has reported it. Drop the connection at once.
        drop((driver, disconnect_handle, receiver, client));
        drain(&mut ops).await;
        return SessionEnd::Failed(OP_TIMEOUT_END_TEXT.into());
    };
    drop((handle, receiver, client));
    drain(&mut ops).await;
    match event {
        DisconnectedEvent::ApplicationDisconnect => SessionEnd::Disconnected,
        DisconnectedEvent::ServerDisconnect(d) => {
            let reason = d
                .properties
                .reason_string
                .unwrap_or_else(|| format!("{:?}", d.reason));
            SessionEnd::Failed(format!("The broker closed the connection: {reason}."))
        }
        DisconnectedEvent::IoError(e) => SessionEnd::Failed(io_text(&e)),
        DisconnectedEvent::ProtocolError(_) => SessionEnd::Failed(PROTOCOL_TEXT.into()),
        DisconnectedEvent::PingTimeout => {
            SessionEnd::Failed("The broker did not answer the keep-alive ping.".into())
        }
    }
}

async fn drain(ops: &mut JoinSet<()>) {
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, async {
        while ops.join_next().await.is_some() {}
    })
    .await;
}

const PROTOCOL_TEXT: &str = "The broker sent data that is not valid MQTT 5.";

/// Turns incoming publishes into `Incoming` and queues them for the UI. Never waits for the UI.
struct Ingest {
    shared: Rc<Shared>,
    generation: u64,
    /// Value of `local_limits::dropped()` that we already counted.
    last_drops: Cell<u64>,
}

impl Ingest {
    fn publish(&mut self, publish: Publish) {
        self.sync_drops();
        let s = &*self.shared;
        // The client queue already bounds `publish`, so the conversion is bounded too.
        let message = self.convert(publish);
        let cost = queue_cost(&message);
        // Only this thread adds to the counter, so check-then-add is safe.
        let fits =
            s.queued_bytes.load(Ordering::Acquire).saturating_add(cost) <= s.limits.queue_bytes;
        if fits {
            s.queued_bytes.fetch_add(cost, Ordering::AcqRel);
            if s.incoming.try_send(message).is_ok() {
                return;
            }
            s.queued_bytes.fetch_sub(cost, Ordering::AcqRel);
        }
        s.stats.drops.fetch_add(1, Ordering::Relaxed);
    }

    fn convert(&self, p: Publish) -> Incoming {
        let props = p.properties;
        Incoming {
            generation: self.generation,
            topic: Arc::from(p.topic_name.as_str()),
            payload: p.payload,
            qos: match p.qos {
                DeliveryQoS::AtMostOnce => Qos::AtMostOnce,
                // We subscribe with maximum QoS 1. QoS 2 cannot arrive.
                DeliveryQoS::AtLeastOnce(_) | DeliveryQoS::ExactlyOnce(_) => Qos::AtLeastOnce,
            },
            retain: p.retain,
            props: MsgProps {
                content_type: props.content_type,
                response_topic: props.response_topic.map(|t| t.as_str().to_owned()),
                correlation_data: props.correlation_data,
                message_expiry: props.message_expiry_interval,
                user: props.user_properties,
                payload_utf8: props.payload_format_indicator == PayloadFormatIndicator::UTF8,
            },
            received_at: SystemTime::now(),
        }
    }

    /// Adds the drops of the client's own queue (the local patch) to our counter.
    fn sync_drops(&self) {
        let now = local_limits::dropped();
        let delta = now.saturating_sub(self.last_drops.replace(now));
        if delta > 0 {
            self.shared.stats.drops.fetch_add(delta, Ordering::Relaxed);
        }
    }
}

/// Starts one operation in its own task, so a full client queue never stalls the session loop.
/// At most `MAX_PENDING_OPS` operations run at one time. More are rejected.
fn dispatch(
    client: &Client,
    command: Command,
    generation: u64,
    shared: &Rc<Shared>,
    expired: &Rc<Notify>,
    ops: &mut JoinSet<()>,
) {
    if ops.len() >= MAX_PENDING_OPS {
        reject(shared, generation, command, TOO_MANY_OPS_TEXT);
        return;
    }
    let (client, shared, expired) = (client.clone(), shared.clone(), expired.clone());
    match command {
        Command::Subscribe { filter, qos } => {
            ops.spawn_local(subscribe_op(
                client, filter, qos, generation, shared, expired,
            ));
        }
        Command::Unsubscribe { filter } => {
            ops.spawn_local(unsubscribe_op(client, filter, generation, shared, expired));
        }
        Command::Publish(outgoing) => {
            ops.spawn_local(publish_op(client, *outgoing, generation, shared, expired));
        }
        Command::Connect(_) => {}
    }
}

fn wire_qos(qos: Qos) -> WireQos {
    match qos {
        Qos::AtMostOnce => WireQos::AtMostOnce,
        Qos::AtLeastOnce => WireQos::AtLeastOnce,
    }
}

async fn subscribe_op(
    client: Client,
    filter: String,
    qos: Qos,
    generation: u64,
    shared: Rc<Shared>,
    expired: Rc<Notify>,
) {
    let result = tokio::time::timeout(OP_TIMEOUT, async {
        let topic_filter =
            TopicFilter::new(&filter).map_err(|e| format!("Invalid topic filter: {e}"))?;
        let token = client
            .subscribe(
                topic_filter,
                wire_qos(qos),
                false,
                RetainOptions::default(),
                SubscribeProperties::default(),
            )
            .await
            .map_err(|_| "Not connected.".to_owned())?;
        let ack = token.await.map_err(|e| e.to_string())?;
        match ack.reasons.first() {
            Some(SubAckReason::GrantedQoS0) => Ok(Qos::AtMostOnce),
            Some(SubAckReason::GrantedQoS1) => Ok(Qos::AtLeastOnce),
            Some(other) => Err(suback_text(other)),
            None => Err("The broker sent an empty SUBACK.".to_owned()),
        }
    })
    .await
    .unwrap_or_else(|_| {
        expired.notify_one();
        Err(NO_ANSWER_TEXT.to_owned())
    });
    shared.event(Event::Subscribed {
        generation,
        filter,
        result,
    });
}

async fn unsubscribe_op(
    client: Client,
    filter: String,
    generation: u64,
    shared: Rc<Shared>,
    expired: Rc<Notify>,
) {
    let fail = |why: &str| format!("Cannot unsubscribe from {filter}: {why}");
    let result = tokio::time::timeout(OP_TIMEOUT, async {
        let topic_filter =
            TopicFilter::new(&filter).map_err(|e| fail(&format!("Invalid topic filter: {e}")))?;
        let token = client
            .unsubscribe(topic_filter, UnsubscribeProperties::default())
            .await
            .map_err(|_| fail("Not connected."))?;
        let ack = token.await.map_err(|e| fail(&e.to_string()))?;
        ack.as_result()
            .map_err(|e| format!("Unsubscribe rejected: {}", e.reason))
    })
    .await
    .unwrap_or_else(|_| {
        expired.notify_one();
        Err(fail(NO_ANSWER_TEXT))
    });
    shared.event(match result {
        Ok(()) => Event::Unsubscribed { generation, filter },
        Err(message) => Event::Error {
            generation,
            message,
        },
    });
}

async fn publish_op(
    client: Client,
    o: Outgoing,
    generation: u64,
    shared: Rc<Shared>,
    expired: Rc<Notify>,
) {
    let result = match tokio::time::timeout(OP_TIMEOUT, publish(&client, &o)).await {
        Ok(result) => result,
        Err(_) => {
            expired.notify_one();
            PublishResult::TimedOut
        }
    };
    shared.event(Event::Published {
        generation,
        op_id: o.op_id,
        result,
    });
}

async fn publish(client: &Client, o: &Outgoing) -> PublishResult {
    let topic = match TopicName::new(&o.topic) {
        Ok(t) => t,
        Err(e) => return PublishResult::Rejected(format!("Invalid topic: {e}")),
    };
    let response_topic = match o
        .props
        .response_topic
        .as_deref()
        .map(TopicName::new)
        .transpose()
    {
        Ok(t) => t,
        Err(e) => return PublishResult::Rejected(format!("Invalid response topic: {e}")),
    };
    let props = PublishProperties {
        content_type: o.props.content_type.clone(),
        response_topic,
        correlation_data: o.props.correlation_data.clone(),
        message_expiry_interval: o.props.message_expiry,
        user_properties: o.props.user.clone(),
        payload_format_indicator: if o.props.payload_utf8 {
            PayloadFormatIndicator::UTF8
        } else {
            PayloadFormatIndicator::Unspecified
        },
        ..Default::default()
    };
    match o.qos {
        Qos::AtMostOnce => {
            let Ok(token) = client
                .publish_qos0(topic, o.payload.clone(), o.retain, props)
                .await
            else {
                return PublishResult::LostOnDisconnect;
            };
            match token.await {
                Ok(()) => PublishResult::Sent,
                Err(_) => PublishResult::LostOnDisconnect,
            }
        }
        Qos::AtLeastOnce => {
            let Ok(token) = client
                .publish_qos1(topic, o.payload.clone(), o.retain, props)
                .await
            else {
                return PublishResult::LostOnDisconnect;
            };
            match token.await {
                Ok(ack) if ack.is_success() => PublishResult::Acknowledged,
                Ok(ack) => PublishResult::Rejected(match ack.properties.reason_string {
                    Some(text) => format!("{:?}: {text}", ack.reason),
                    None => format!("{:?}", ack.reason),
                }),
                Err(_) => PublishResult::LostOnDisconnect,
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Transport and error text
// ---------------------------------------------------------------------------------------------

fn transport(p: &Profile) -> Result<ConnectionTransportType, String> {
    let host = p.host.clone();
    match p.transport {
        Transport::Tcp => Ok(ConnectionTransportType::Tcp {
            hostname: host,
            port: p.port,
        }),
        Transport::Tls => Ok(ConnectionTransportType::Tls {
            hostname: host,
            port: p.port,
            tls_config: tls_config(p)?,
        }),
        Transport::Ws | Transport::Wss => {
            let secure = p.transport == Transport::Wss;
            let path = if p.ws_path.starts_with('/') {
                p.ws_path.clone()
            } else {
                format!("/{}", p.ws_path)
            };
            let host = if host.contains(':') && !host.starts_with('[') {
                format!("[{host}]")
            } else {
                host
            };
            let url = format!(
                "{}://{host}:{}{path}",
                if secure { "wss" } else { "ws" },
                p.port
            );
            let request = url
                .into_client_request()
                .map_err(|e| format!("Invalid WebSocket address: {e}"))?;
            Ok(ConnectionTransportType::Ws {
                request,
                tls_config: if secure { Some(tls_config(p)?) } else { None },
            })
        }
    }
}

/// Reads the PEM files here. This runs on the net thread, never on the UI thread.
fn tls_config(p: &Profile) -> Result<TlsConfig, String> {
    fn read(path: &Path, what: &str) -> Result<Vec<u8>, String> {
        std::fs::read(path)
            .map_err(|e| format!("Cannot read the {what} file {}: {e}", path.display()))
    }
    let tls = |e: openssl::error::ErrorStack| format!("TLS set-up failed: {e}");
    let mut builder = SslConnector::builder(SslMethod::tls_client()).map_err(tls)?;
    builder
        .set_min_proto_version(Some(SslVersion::TLS1_2))
        .map_err(tls)?;
    if let Some(path) = &p.ca_file {
        let certs = X509::stack_from_pem(&read(path, "CA")?)
            .map_err(|e| format!("The CA file {} is not valid PEM: {e}", path.display()))?;
        let store = builder.cert_store_mut();
        for cert in certs {
            store.add_cert(cert).map_err(tls)?;
        }
    }
    if let Some((cert_path, key_path)) = &p.client_cert {
        let mut chain = X509::stack_from_pem(&read(cert_path, "client certificate")?)
            .map_err(|e| format!("The client certificate is not valid PEM: {e}"))?
            .into_iter();
        let cert = chain
            .next()
            .ok_or("The client certificate file has no certificate.")?;
        let key = openssl::pkey::PKey::private_key_from_pem(&read(key_path, "client key")?)
            .map_err(|e| format!("The client key is not valid PEM: {e}"))?;
        builder.set_certificate(&cert).map_err(tls)?;
        builder.set_private_key(&key).map_err(tls)?;
        for extra in chain {
            builder.add_extra_chain_cert(extra).map_err(tls)?;
        }
    }
    if !p.verify_tls {
        builder.set_verify(SslVerifyMode::NONE);
    }
    Ok(builder.into())
}

fn io_text(e: &std::io::Error) -> String {
    use std::io::ErrorKind as K;
    let text = e.to_string();
    let lower = text.to_lowercase();
    match e.kind() {
        K::ConnectionRefused => {
            "The broker refused the connection. Check the host and the port.".into()
        }
        K::TimedOut => "The connection timed out.".into(),
        K::UnexpectedEof | K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe => {
            "The broker closed the connection.".into()
        }
        K::InvalidData if lower.contains("maximum packet size") => {
            "The broker sent a packet that is larger than the allowed size.".into()
        }
        _ if lower.contains("certificate") => {
            format!(
                "TLS: the server certificate is not valid ({text}). Add its CA file, or turn off verification."
            )
        }
        _ if lower.contains("failed to lookup") || lower.contains("name or service") => {
            "The host name was not found.".into()
        }
        _ => format!("Network error: {text}"),
    }
}

fn connect_error_text(e: &ConnectError) -> String {
    match e {
        ConnectError::Io(e) => io_text(e),
        ConnectError::Protocol(_) => PROTOCOL_TEXT.into(),
        ConnectError::ResponseTimeout => "The broker did not answer the connection request.".into(),
        ConnectError::Rejected(ack) => {
            let reason = match ack.reason {
                ConnAckReason::BadUserNameOrPassword => "the user name or password is wrong",
                ConnAckReason::NotAuthorized => "this client is not authorized",
                ConnAckReason::ClientIdentifierNotValid => "the client ID is not valid",
                ConnAckReason::ServerUnavailable => "the server is not available",
                ConnAckReason::ServerBusy => "the server is busy",
                ConnAckReason::Banned => "this client is banned",
                ConnAckReason::UnsupportedProtocolVersion => "the broker does not support MQTT 5",
                ConnAckReason::PacketTooLarge => "the CONNECT packet is too large",
                ConnAckReason::QuotaExceeded => "a quota is exceeded",
                ConnAckReason::UseAnotherServer | ConnAckReason::ServerMoved => {
                    "use another server"
                }
                ConnAckReason::ConnectionRateExceeded => "the connection rate is exceeded",
                _ => "",
            };
            match (&ack.properties.reason_string, reason) {
                (Some(s), "") => format!("The broker rejected the connection: {s}."),
                (Some(s), r) => format!("The broker rejected the connection: {r} ({s})."),
                (None, "") => format!("The broker rejected the connection: {:?}.", ack.reason),
                (None, r) => format!("The broker rejected the connection: {r}."),
            }
        }
    }
}

fn suback_text(r: &SubAckReason) -> String {
    match r {
        SubAckReason::NotAuthorized => "Not authorized to subscribe.".into(),
        SubAckReason::TopicFilterInvalid => "The broker says the topic filter is not valid.".into(),
        SubAckReason::QuotaExceeded => "The subscription quota is exceeded.".into(),
        SubAckReason::SharedSubscriptionsNotSupported => {
            "The broker does not support shared subscriptions.".into()
        }
        SubAckReason::WildcardSubscriptionsNotSupported => {
            "The broker does not support wildcard subscriptions.".into()
        }
        other => format!("The broker refused the subscription: {other:?}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Connect and Subscribe are queued (queue full), then a disconnect comes. The net thread
    /// has read nothing yet. No connection must start.
    #[tokio::test(flavor = "current_thread")]
    async fn disconnect_voids_older_commands_even_when_queue_is_full() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("addr").port();

        let (cmd_tx, cmd_rx) = mpsc::channel(2);
        let (cancel_tx, cancel_rx) = watch::channel(0u64);
        let (incoming, _incoming_rx) = mpsc::channel(1);
        let (events, mut events_rx) = mpsc::channel(8);
        let (status, status_rx) = watch::channel(Status::default());
        let shared = Rc::new(Shared {
            incoming,
            events,
            status,
            stats: Arc::default(),
            queued_bytes: Arc::default(),
            limits: NetLimits::default(),
        });

        let profile = Profile {
            host: "127.0.0.1".into(),
            port,
            transport: Transport::Tcp,
            subscriptions: vec![],
            ..Profile::default()
        };
        cmd_tx
            .try_send((0, Command::Connect(Box::new(profile))))
            .expect("queue connect");
        let subscribe = Command::Subscribe {
            filter: "a/b".into(),
            qos: Qos::AtMostOnce,
        };
        cmd_tx
            .try_send((0, subscribe.clone()))
            .expect("queue subscribe");
        assert!(cmd_tx.try_send((0, subscribe)).is_err(), "queue is full");
        // Same step as `Net::disconnect`.
        cancel_tx.send_modify(|v| *v += 1);
        drop(cmd_tx); // The supervisor ends when the queue is empty.

        LocalSet::new()
            .run_until(supervisor(cmd_rx, cancel_rx, shared))
            .await;

        assert_eq!(
            *status_rx.borrow(),
            Status::default(),
            "no connection started"
        );
        assert_eq!(
            listener.accept().expect_err("no client").kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(matches!(
            events_rx.try_recv(),
            Ok(Event::Subscribed { result: Err(_), .. })
        ));
    }

    /// A fake broker that accepts CONNECT and then never answers.
    fn silent_broker() -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf); // CONNECT
            let _ = sock.write_all(&[0x20, 0x03, 0x00, 0x00, 0x00]); // CONNACK success
            sock.set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            while sock.read(&mut buf).is_ok_and(|n| n > 0) {}
        });
        (port, server)
    }

    async fn connect_to(net: &mut Net, port: u16) {
        let previous = net.status.borrow().generation;
        let profile = Profile {
            host: "127.0.0.1".into(),
            port,
            transport: Transport::Tcp,
            subscriptions: vec![],
            ..Profile::default()
        };
        net.send(Command::Connect(Box::new(profile))).expect("send");
        tokio::time::timeout(
            Duration::from_secs(5),
            net.status
                .wait_for(|s| s.generation > previous && s.state == ConnState::Connected),
        )
        .await
        .expect("timeout")
        .expect("status");
    }

    fn publish_cmd(op_id: u64) -> Command {
        Command::Publish(Box::new(Outgoing {
            op_id,
            topic: "t/x".into(),
            payload: Bytes::from_static(b"x"),
            qos: Qos::AtLeastOnce,
            retain: false,
            props: MsgProps::default(),
        }))
    }

    /// A QoS 1 publish that gets no PUBACK ends the session. The client keeps the payload of a
    /// timed out publish, so the bound of open operations needs this. A new Connect works.
    #[tokio::test(flavor = "current_thread")]
    async fn operation_timeout_ends_the_session() {
        let (port, server) = silent_broker();
        let mut net = Net::start(NetLimits::default());
        connect_to(&mut net, port).await;

        net.send(publish_cmd(1)).expect("send");
        tokio::time::sleep(OP_TIMEOUT / 2).await;
        net.send(publish_cmd(2)).expect("send");

        let mut results = std::collections::HashMap::new();
        while results.len() < 2 {
            let event = tokio::time::timeout(Duration::from_secs(5), net.events.recv())
                .await
                .expect("event timeout")
                .expect("events closed");
            if let Event::Published { op_id, result, .. } = event {
                results.insert(op_id, result);
            }
        }
        assert_eq!(results[&1], PublishResult::TimedOut);
        assert_eq!(results[&2], PublishResult::LostOnDisconnect);
        tokio::time::timeout(
            Duration::from_secs(5),
            net.status
                .wait_for(|s| matches!(&s.state, ConnState::Failed(t) if t == OP_TIMEOUT_END_TEXT)),
        )
        .await
        .expect("timeout")
        .expect("status");
        server.join().expect("server");

        let (port, server) = silent_broker();
        connect_to(&mut net, port).await;
        net.shutdown().await;
        server.join().expect("server");
    }

    #[test]
    fn queue_cost_counts_topic_and_properties() {
        let empty = Incoming {
            generation: 1,
            topic: Arc::from(""),
            payload: Bytes::new(),
            qos: Qos::AtMostOnce,
            retain: false,
            props: MsgProps::default(),
            received_at: SystemTime::now(),
        };
        let mut heavy = empty.clone();
        heavy.topic = Arc::from("a/long/topic");
        heavy.props.user = vec![("key".into(), "x".repeat(100))];
        assert!(queue_cost(&heavy) >= queue_cost(&empty) + 12 + 103);
    }
}
