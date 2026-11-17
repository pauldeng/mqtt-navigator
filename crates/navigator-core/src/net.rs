//! The "net" thread: one tokio `current_thread` runtime that owns the MQTT client (ntex-mqtt, in
//! the `wire` module, with our own dialer in `dial`).
//!
//! The UI talks to it only through [`Net`]. The net thread never waits for the UI. See the
//! "Threads and data flow" section of the plan, ADR 0002 and ARCHITECTURE.md.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::{JoinSet, LocalSet};

use crate::model::{Incoming, MsgProps, Qos};
use crate::tls::{TlsDiag, TlsMaterial};

mod dial;
mod wire;

use crate::topic::filter_error;
use wire::{Connected, Params, UnsubFail, Wire};

pub use crate::profile::{Profile, RetainHandling, SubOptions, Transport, Will};

/// A filter that the broker must have: the filter, its QoS, its MQTT 5 options and the request
/// ID of the SUBSCRIBE that set them (0 for a filter from the profile).
type Want = (String, Qos, SubOptions, u64);

/// The MQTT version of a connection. In JSON: `"v311"` or `"v5"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// MQTT 3.1.1. The profile's `clean_start` is the Clean Session flag. Session expiry, will
    /// delay and properties do not exist in this version.
    V311,
    /// MQTT 5.
    V5,
}

/// One connect: the saved profile plus the secrets. The secrets live in memory only.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectConfig {
    pub profile: Profile,
    /// The MQTT version. [`ConnectConfig::from`] takes it from the profile.
    pub protocol: Protocol,
    pub password: Option<String>,
    /// Decrypts an encrypted client key. Never prompted for.
    pub key_passphrase: Option<String>,
    /// The generation of this connection. [`Net::connect`] and [`Net::send`] set it. The UI
    /// chooses it, not the net thread: the UI knows the generation of a connect at the moment
    /// it sends the command, so it can ignore everything that has another generation.
    pub generation: u64,
    /// Filters that the broker may still hold for this profile's persistent session (from the
    /// last [`Event::SessionFilters`]). They seed the session set when the net thread has no
    /// newer one for the same host, port and client ID.
    pub session_filters: Vec<String>,
}

impl From<Profile> for ConnectConfig {
    fn from(profile: Profile) -> Self {
        Self {
            protocol: profile.protocol,
            profile,
            password: None,
            key_passphrase: None,
            generation: 0,
            session_filters: Vec::new(),
        }
    }
}

/// Hides the secrets, so a log line never shows them.
impl std::fmt::Debug for ConnectConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectConfig")
            .field("profile", &self.profile)
            .field("protocol", &self.protocol)
            .field("generation", &self.generation)
            .field("session_filters", &self.session_filters)
            .field("password", &self.password.as_ref().map(|_| "<hidden>"))
            .field(
                "key_passphrase",
                &self.key_passphrase.as_ref().map(|_| "<hidden>"),
            )
            .finish()
    }
}

/// The broker keeps the session after the connection ends (D3): MQTT 5 with a session expiry,
/// MQTT 3.1.1 with Clean Session off.
fn session_persists(c: &ConnectConfig) -> bool {
    match c.protocol {
        Protocol::V311 => !c.profile.clean_start,
        Protocol::V5 => c.profile.session_expiry_secs > 0,
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
    Connect(Box<ConnectConfig>),
    /// `options` are MQTT 5 only. MQTT 3.1.1 never sends them. `req` is a request ID of the
    /// caller. Every `Subscribed` result for the filter has the `req` of the latest accepted
    /// Subscribe, also the result of an automatic resubscribe.
    Subscribe {
        filter: String,
        qos: Qos,
        options: SubOptions,
        req: u64,
    },
    Unsubscribe {
        filter: String,
    },
    Publish(Box<Outgoing>),
}

/// Connection state.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ConnState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    /// The connection was lost for a temporary reason. The net thread connects again after a
    /// backoff (1 s to 30 s, with jitter). Data and the generation stay.
    Reconnecting {
        /// 1 for the first retry.
        attempt: u32,
        /// Why the connection was lost, as text for the user.
        reason: String,
    },
    Disconnecting,
    /// The connection ended with an error that a retry cannot fix. The text is for the user.
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Status {
    pub generation: u64,
    pub state: ConnState,
    /// The attempt that is `Connected`: it tells the attempts of one generation apart. 0 in
    /// every other state. [`Event::ConnectionDetails`] has the same number.
    pub attempt: u64,
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

/// Results and errors. The log is bounded, and results are never lost: see [`EVENTS_CAP`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The connection is up again after `Reconnecting`. The subscriptions are sent again.
    Reconnected {
        generation: u64,
        at: std::time::SystemTime,
    },
    /// SUBACK result: granted QoS, or the reason as text.
    Subscribed {
        generation: u64,
        filter: String,
        /// The `req` of the Subscribe that this result answers. The automatic resubscribe uses
        /// the `req` of the latest accepted Subscribe of the filter (0 for a filter from the
        /// profile). A result with an older `req` belongs to an older state of the filter.
        req: u64,
        result: Result<Qos, String>,
    },
    /// UNSUBSCRIBE result for `filter`. `Ok` also when the net thread removed the filter while
    /// offline. In every case the filter is no longer in the desired list.
    Unsubscribed {
        generation: u64,
        filter: String,
        result: Result<(), String>,
    },
    Published {
        generation: u64,
        op_id: u64,
        result: PublishResult,
    },
    /// The filters that the broker may hold for the persistent session of the connection
    /// changed. The list is the whole set (it replaces the older one). It comes at most every
    /// 500 ms, and only for a profile with a session expiry. Save it with the profile and give
    /// it back in [`ConnectConfig::session_filters`].
    SessionFilters {
        generation: u64,
        filters: Vec<String>,
    },
    /// What the CONNACK of this connection says. It comes once for each successful connect,
    /// after the status `Connected`.
    ConnectionDetails {
        generation: u64,
        /// The attempt of these details: [`Status::attempt`] of the matching `Connected` status.
        /// An older attempt of the same generation can still have an event in the log.
        attempt: u64,
        details: Box<ConnectionDetails>,
    },
    /// An error the user must see, for example an invalid filter.
    Error { generation: u64, message: String },
}

/// The facts of a CONNACK, for the "Connection details" view. MQTT 3.1.1 has only
/// `session_present`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionDetails {
    pub protocol: Protocol,
    pub session_present: bool,
    /// `None` for MQTT 3.1.1.
    pub v5: Option<V5Details>,
}

/// The MQTT 5 CONNACK properties. A text from the broker has gone through `display_safe`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V5Details {
    pub assigned_client_id: Option<String>,
    /// The keep alive in use: the Server Keep Alive if the CONNACK has one, else the profile's.
    /// 0 means off.
    pub keep_alive_secs: u16,
    /// The Server Keep Alive property, if the CONNACK has it.
    pub server_keep_alive_secs: Option<u16>,
    pub receive_max: u16,
    /// `None`: the broker announced no limit.
    pub max_packet_size: Option<u32>,
    /// The highest QoS of the broker (0, 1 or 2). This app uses at most 1.
    pub max_qos: u8,
    pub retain_available: bool,
    pub wildcard_subscriptions: bool,
    pub shared_subscriptions: bool,
    pub subscription_ids: bool,
    pub topic_alias_max: u16,
    pub server_reference: Option<String>,
    pub response_info: Option<String>,
    pub reason_string: Option<String>,
}

/// Counters shared with the UI.
#[derive(Debug, Default)]
pub struct NetStats {
    /// Messages dropped because a queue was full (the byte budget or the message count).
    pub drops: AtomicU64,
    /// Messages in the queue to the UI that the UI has not released yet.
    pub queued_msgs: AtomicU64,
    /// Events dropped because the event log was full. This stays 0 while the UI drains the log
    /// (see [`EVENTS_CAP`]). It can grow only if the UI stops draining during many reconnects.
    pub event_drops: AtomicU64,
}

/// Queue limits of the net thread.
#[derive(Clone, Copy, Debug)]
pub struct NetLimits {
    /// Largest incoming packet (the whole packet), in bytes. The default is 1 MiB. It is
    /// advertised as Maximum Packet Size in CONNECT, and the ntex codec enforces it.
    pub max_packet: usize,
    /// Capacity of the queue to the UI, in messages.
    pub queue_len: usize,
    /// Capacity of the queue to the UI, in bytes. Each message costs [`queue_cost`]: payload,
    /// topic, properties and a fixed overhead.
    pub queue_bytes: usize,
    /// The time limits. Only tests change them (see [`Timers`]).
    pub timers: Timers,
}

impl Default for NetLimits {
    fn default() -> Self {
        Self {
            max_packet: 1024 * 1024,
            queue_len: 50_000,
            queue_bytes: 32 * 1024 * 1024,
            timers: Timers::default(),
        }
    }
}

/// The time limits of a connection. The default is the shipped value of each. Integration tests
/// set shorter values, so that a test does not wait for the real ones. One test for each limit
/// keeps the real value. The end texts keep the text of the real value (for example "10 s").
/// ntex counts the write, TLS handshake and frame read-rate limits in whole seconds, so these
/// round up to a whole second.
#[derive(Clone, Copy, Debug)]
pub struct Timers {
    /// The transport set-up and the CONNACK (10 s).
    pub connect: Duration,
    /// One publish, subscribe or unsubscribe, from the submit to the acknowledgement (10 s).
    pub op: Duration,
    /// A write that stays blocked ends the connection (10 s).
    pub write: Duration,
    /// The TLS handshake (5 s, the default of ntex-tls).
    pub tls_handshake: Duration,
    /// The WebSocket upgrade answer (5 s, the default of ntex).
    pub ws_upgrade: Duration,
    /// Frame read rate: the time of one check (10 s) ...
    pub frame_check: Duration,
    /// ... and the time that a whole packet may take (60 s).
    pub frame_cap: Duration,
}

impl Default for Timers {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            op: OP_TIMEOUT,
            write: Duration::from_secs(10),
            tls_handshake: Duration::from_secs(5),
            ws_upgrade: Duration::from_secs(5),
            frame_check: Duration::from_secs(10),
            frame_cap: Duration::from_secs(60),
        }
    }
}

/// Cancel value that means "stop the thread".
const SHUTDOWN: u64 = u64::MAX;
/// Time limit for one publish, subscribe or unsubscribe, from submit to the acknowledgement.
/// A timeout ends the session, because the client keeps the operation and its payload.
/// Unit tests use a short limit.
#[cfg(not(test))]
const OP_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const OP_TIMEOUT: Duration = Duration::from_millis(300);
/// First wait before a retry. It doubles for each retry, up to `BACKOFF_MAX`. Unit tests use
/// short times.
#[cfg(not(test))]
const BACKOFF_BASE: Duration = Duration::from_secs(1);
#[cfg(not(test))]
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A connection that stayed up this long resets the retry counter.
#[cfg(not(test))]
const STABLE_AFTER: Duration = Duration::from_secs(30);
#[cfg(test)]
const BACKOFF_BASE: Duration = Duration::from_millis(100);
#[cfg(test)]
const BACKOFF_MAX: Duration = Duration::from_millis(800);
#[cfg(test)]
const STABLE_AFTER: Duration = Duration::from_secs(2);
/// Time that a session gets to write DISCONNECT before we abort it.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Time that the open operations get to report "lost" after the connection ended.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// Capacity of the event log.
///
/// A result must never be lost, or the UI waits for it for ever. The net thread cannot wait for
/// the UI, so every producer of results asks first. Each one starts new work only while the log
/// has [`EVENTS_RESERVE`] free places. That is more than the results of all the work that can be
/// in flight at one time, so the log is never full when a result must go in:
///
/// - [`Net::send`] accepts a publish, subscribe or unsubscribe (each gives at most one result).
///   When the UI drains too slowly, `send` returns [`SendError::Full`] and the UI shows its
///   normal "try again" text. `Connect` gives no result, so it does not ask.
/// - The automatic re-subscribe after a connect asks before it starts each filter. It also
///   asks before it sends `Reconnected`. If the log has no room, it waits for the UI to drain
///   the log (see [`Net::recv_event`] and [`Net::events_drained`]). It runs no timer while it
///   waits. So a profile with thousands of filters needs a UI that drains.
const EVENTS_CAP: usize = 2048;
/// Free room in the event log that a producer needs. Work in flight: the command queue (64),
/// the session queue (256), the operations (`MAX_PENDING_OPS`), one in the supervisor and
/// `Reconnected`. The rest is slack.
const EVENTS_RESERVE: usize = 64 + 256 + MAX_PENDING_OPS + 1 + 64;
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
    /// Weak, so the event log still closes when the net thread ends. Used to read the free room.
    events_tx: mpsc::WeakSender<Event>,
    /// The last generation that [`Net::connect`] gave out. It only grows.
    last_generation: AtomicU64,
    /// Cancel epoch: counts disconnect requests. `SHUTDOWN` stops the thread. Each command
    /// carries the epoch at the time of [`Net::send`]. A cancel makes all older commands void.
    cancel: watch::Sender<u64>,
    /// Bytes ([`queue_cost`]) that are in `incoming` and not yet released.
    queued_bytes: Arc<AtomicUsize>,
    /// See [`Shared::broker_max_packet`].
    broker_max_packet: Arc<AtomicU32>,
    /// Becomes true when the net thread has ended.
    done: watch::Receiver<bool>,
    /// Wakes the net thread when the UI took events out of the log (see [`Net::recv_event`]).
    drained: Arc<Notify>,
}

/// A handle that tells the net thread "I took events out of the log". See
/// [`Net::events_drained`].
#[derive(Clone, Debug)]
pub struct EventsDrained(Arc<Notify>);

impl EventsDrained {
    /// Call after you took events out of [`Net::events`]. It costs one atomic operation, so you
    /// can call it after each event or after each batch. It never blocks.
    pub fn notify(&self) {
        self.0.notify_one();
    }
}

impl Net {
    /// Starts the net thread. It waits for `Command::Connect`.
    pub fn start(limits: NetLimits) -> Self {
        let (commands, cmd_rx) = mpsc::channel(64);
        let (incoming_tx, incoming) = mpsc::channel(limits.queue_len.max(1));
        let (events_tx, events) = mpsc::channel(EVENTS_CAP);
        let (status_tx, status) = watch::channel(Status::default());
        let (cancel, cancel_rx) = watch::channel(0u64);
        let stats = Arc::new(NetStats::default());
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let broker_max_packet = Arc::new(AtomicU32::new(0));
        let (done_tx, done) = watch::channel(false);

        let events_weak = events_tx.downgrade();
        let drained = Arc::new(Notify::new());
        let parts = (
            incoming_tx,
            events_tx,
            status_tx.clone(),
            stats.clone(),
            queued_bytes.clone(),
            broker_max_packet.clone(),
            drained.clone(),
        );
        // The thread is detached. `done_tx` is dropped if the thread cannot start or dies, and
        // that also ends `shutdown`.
        let thread = std::thread::Builder::new()
            .name("mqtt-net".into())
            .spawn(move || {
                let (incoming, events, status, stats, queued_bytes, broker_max_packet, drained) =
                    parts;
                let shared = Rc::new(Shared {
                    incoming,
                    events,
                    status,
                    stats,
                    queued_bytes,
                    broker_max_packet,
                    drained,
                    details: RefCell::default(),
                    limits,
                    active: Cell::new(0),
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
                // One reactor scope for the whole thread body. ntex needs it for its I/O and
                // timers. A nested call panics, so nothing below may start another scope.
                let reactor: Box<dyn ntex_net::Reactor> = Box::new(ntex_net::tokio::Reactor);
                ntex_net::with_reactor(&reactor, || {
                    LocalSet::new().block_on(&rt, supervisor(cmd_rx, cancel_rx, shared));
                });
                done_tx.send_replace(true);
            });
        if let Err(e) = thread {
            status_tx.send_replace(Status {
                generation: 0,
                state: ConnState::Failed(format!("Cannot start the network thread: {e}")),
                attempt: 0,
            });
        }
        Self {
            commands,
            events_tx: events_weak,
            incoming,
            status,
            events,
            stats,
            last_generation: AtomicU64::new(0),
            cancel,
            queued_bytes,
            broker_max_packet,
            done,
            drained,
        }
    }

    /// Takes the next event out of the log and tells the net thread that there is more room. Use
    /// it instead of `events.recv()`. The net thread waits for this signal when it has events to
    /// send and the log is nearly full (see [`EVENTS_CAP`]).
    pub async fn recv_event(&mut self) -> Option<Event> {
        let event = self.events.recv().await;
        self.drained.notify_one();
        event
    }

    /// A handle for code that reads [`Net::events`] by itself (for example a task that owns the
    /// receiver). Call [`EventsDrained::notify`] after you took events out of the log. Without
    /// it, a re-subscribe that waits for room in a full log waits until some other net event
    /// (an operation that ends, a command) wakes it.
    pub fn events_drained(&self) -> EventsDrained {
        EventsDrained(self.drained.clone())
    }

    /// Queues a Connect and returns its generation. The numbers grow with each call, and the
    /// net thread uses the number as the generation of the connection. Every status, event and
    /// message of this connection carries it, so the UI can tell it from an older connection
    /// before the net thread has answered anything.
    pub fn connect(&self, mut config: ConnectConfig) -> Result<u64, SendError> {
        let generation = self.last_generation.fetch_add(1, Ordering::Relaxed) + 1;
        config.generation = generation;
        let epoch = *self.cancel.borrow();
        self.commands
            .try_send((epoch, Command::Connect(Box::new(config))))
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => SendError::Full,
                mpsc::error::TrySendError::Closed(_) => SendError::Closed,
            })?;
        Ok(generation)
    }

    /// Queues a command. Never blocks. Returns [`SendError::Full`] if 64 commands are waiting,
    /// or if the UI has not read enough events (see [`EVENTS_CAP`]).
    ///
    /// A [`Net::disconnect`] voids every command that was sent before it, even if the net
    /// thread has not read the command yet. A rejected command is answered with an event.
    ///
    /// A publish is checked first with [`crate::publish::validate_outgoing`] (and the broker's
    /// Maximum Packet Size, see [`Net::broker_max_packet`]). If the check fails, `send` returns
    /// `Ok(())`, does not queue the command, and gives [`Event::Published`] with
    /// [`PublishResult::Rejected`] and the reason. The generation is the latest one that
    /// [`Net::connect`] gave out.
    pub fn send(&self, cmd: Command) -> Result<(), SendError> {
        if let Command::Connect(config) = cmd {
            return self.connect(*config).map(|_| ());
        }
        let events = match self.events_tx.upgrade() {
            Some(tx) if tx.capacity() < EVENTS_RESERVE => return Err(SendError::Full),
            Some(tx) => tx,
            None => return Err(SendError::Closed),
        };
        if let Command::Publish(o) = &cmd
            && let Err(why) = crate::publish::validate_outgoing(o, self.broker_max_packet())
        {
            // The reserve above makes room for this result (see `EVENTS_CAP`).
            return events
                .try_send(Event::Published {
                    generation: self.last_generation.load(Ordering::Relaxed),
                    op_id: o.op_id,
                    result: PublishResult::Rejected(why),
                })
                .map_err(|_| SendError::Full);
        }
        let epoch = *self.cancel.borrow();
        self.commands.try_send((epoch, cmd)).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => SendError::Full,
            mpsc::error::TrySendError::Closed(_) => SendError::Closed,
        })
    }

    /// The broker's Maximum Packet Size from its CONNACK, in bytes. `None` while the session is
    /// not connected, or when the broker sets no limit.
    pub fn broker_max_packet(&self) -> Option<u32> {
        if self.status.borrow().state != ConnState::Connected {
            return None;
        }
        Some(self.broker_max_packet.load(Ordering::Relaxed)).filter(|&n| n != 0 && n != u32::MAX)
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
        let _ = self
            .stats
            .queued_msgs
            .try_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                Some(v.saturating_sub(1))
            });
    }

    /// Messages in `incoming` that the UI has not released yet.
    pub fn queued_msgs(&self) -> u64 {
        self.stats.queued_msgs.load(Ordering::Acquire)
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
    /// Maximum Packet Size of the broker from the last CONNACK. 0: not known.
    broker_max_packet: Arc<AtomicU32>,
    /// The `ConnectionDetails` of the latest connection that the UI has not got yet. A newer
    /// connection replaces it. `Resub::feed` sends it when the event log has room.
    details: RefCell<Option<(u64, u64, Box<ConnectionDetails>)>>,
    /// The UI signals here when it took events out of the log (see [`Net::recv_event`]).
    drained: Arc<Notify>,
    limits: NetLimits,
    /// Token of the one attempt that may change the status and queue messages. 0: none. The
    /// supervisor sets it when it starts an attempt, and clears it when it cancels or ends the
    /// attempt. Results of any other attempt are dropped.
    active: Cell<u64>,
}

impl Shared {
    fn set_status(&self, generation: u64, state: ConnState) {
        self.status.send_replace(Status {
            generation,
            state,
            attempt: 0,
        });
    }

    /// Connecting or Reconnecting to Connected. Does nothing if `token` is not the active
    /// attempt, or if a stop request already moved the state on.
    fn set_connected(&self, generation: u64, token: u64) -> bool {
        if self.active.get() != token {
            return false;
        }
        self.status.send_if_modified(|s| {
            let ok = s.generation == generation
                && matches!(
                    s.state,
                    ConnState::Connecting | ConnState::Reconnecting { .. }
                );
            if ok {
                s.state = ConnState::Connected;
                s.attempt = token;
            }
            ok
        })
    }

    /// True if the log has room for the results of all work that can be in flight.
    fn has_room(&self) -> bool {
        self.events.capacity() >= EVENTS_RESERVE
    }

    /// Every producer asks for room first (see [`EVENTS_CAP`]), so this does not fail. If it fails
    /// anyway (a UI that stops reading), the event is dropped and counted.
    fn event(&self, event: Event) {
        if self.events.try_send(event).is_err() {
            self.stats.event_drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// What the supervisor does after an attempt ended.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The user asked for it.
    Disconnected,
    /// A temporary problem. Connect again after a backoff. The text is for the user.
    Retry(String),
    /// A retry cannot help. The text is for the user.
    Failed(String),
}

/// How one connection attempt ended.
struct AttemptEnd {
    outcome: Outcome,
    /// How long the connection was up. `None`: it never came up.
    up_for: Option<Duration>,
}

impl AttemptEnd {
    fn never_up(outcome: Outcome) -> Self {
        Self {
            outcome,
            up_for: None,
        }
    }
}

/// What an attempt task needs to know.
struct AttemptCtx {
    /// Identity of the attempt. See [`Shared::active`].
    token: u64,
    generation: u64,
    /// The attempt follows a lost connection.
    reconnect: bool,
    /// The filters to subscribe. The attempt reads them at the CONNACK.
    desired: Rc<RefCell<Vec<Want>>>,
    /// The attempt sets this at the CONNACK. From then on, the supervisor sends it every
    /// subscribe and unsubscribe. Before, the attempt takes them from `desired`.
    up: Rc<Cell<bool>>,
    /// Filters that the broker may hold for this session (see [`SessionSubs`]).
    maybe: SessionSubs,
    /// The profile has a session expiry, so the set is reported (see [`report_filters`]).
    report: bool,
    /// The set was reported once for this connection (after its first CONNACK).
    announced: Rc<Cell<bool>>,
    /// The parsed TLS files of this connect (see [`tls_material`]).
    tls: TlsSlot,
}

/// The TLS files of one user Connect, read and parsed once. Empty until the first attempt has
/// built them. Later attempts (after a lost connection) reuse them.
type TlsSlot = Rc<RefCell<Option<Arc<TlsMaterial>>>>;

/// The filters that a broker session may still hold. A filter comes in when its SUBSCRIBE is
/// sent. It goes out only after a successful UNSUBACK, or when a CONNACK says that the session
/// is new (session present is false). So a removal that never reached the broker (the user was
/// offline, or the connection died first) is not forgotten: after a CONNACK that resumes the
/// session, the attempt sends an UNSUBSCRIBE for every filter here that is not desired.
type SessionSubs = Rc<RefCell<SessionSet>>;

/// The set, and whether it changed since the last [`Event::SessionFilters`].
#[derive(Default)]
struct SessionSet {
    filters: BTreeSet<String>,
    dirty: bool,
}

impl SessionSet {
    fn insert(&mut self, filter: &str) {
        if self.filters.insert(filter.to_owned()) {
            self.dirty = true;
        }
    }

    fn remove(&mut self, filter: &str) {
        if self.filters.remove(filter) {
            self.dirty = true;
        }
    }

    fn clear(&mut self) {
        if !self.filters.is_empty() {
            self.filters.clear();
            self.dirty = true;
        }
    }
}

/// Wait before a change of the set is reported. Changes in this time make one event.
const REPORT_DELAY: Duration = Duration::from_millis(500);

/// Reports the set if it changed and the event log has room. Does nothing for a profile
/// without a session expiry: its session ends with the connection.
fn report_filters(shared: &Shared, ctx: &AttemptCtx) {
    if !ctx.report {
        return;
    }
    let mut set = ctx.maybe.borrow_mut();
    if set.dirty && shared.has_room() {
        set.dirty = false;
        shared.event(Event::SessionFilters {
            generation: ctx.generation,
            filters: set.filters.iter().cloned().collect(),
        });
    }
}

/// Identity of a broker session: host, port and client ID.
type SessionKey = (String, u16, String);

/// How many sessions the supervisor remembers. The oldest goes first.
const MAX_SESSIONS: usize = 8;

/// One running attempt: the connect, and the session after it.
struct Attempt {
    handle: tokio::task::JoinHandle<AttemptEnd>,
    commands: mpsc::Sender<Command>,
    stop: Rc<Notify>,
    up: Rc<Cell<bool>>,
}

/// One user Connect. It has one generation, and it can have many attempts.
struct Current {
    generation: u64,
    /// Cancel epoch under which the connect started. A cancel stops only older connects.
    epoch: u64,
    config: Rc<ConnectConfig>,
    /// Filters that the broker must have: the profile's, plus live subscribes, minus live
    /// unsubscribes. Changes while we are not connected count too.
    desired: Rc<RefCell<Vec<Want>>>,
    /// The filters of the broker session of this connect. It outlives the connect.
    maybe: SessionSubs,
    /// See [`AttemptCtx::announced`].
    announced: Rc<Cell<bool>>,
    /// See [`AttemptCtx::tls`].
    tls: TlsSlot,
    /// Number of retries since the last stable connection.
    retries: u32,
    attempt: Option<Attempt>,
    /// End of the backoff. Only set while there is no attempt.
    wake: Option<tokio::time::Instant>,
}

/// Retry counter after an attempt: it starts again if the connection was stable.
fn next_retries(previous: u32, up_for: Option<Duration>) -> u32 {
    let base = if up_for.is_some_and(|d| d >= STABLE_AFTER) {
        0
    } else {
        previous
    };
    base.saturating_add(1)
}

/// Backoff before retry number `attempt` (1 is the first). `random` is any `u64`. The base
/// doubles for each retry up to the maximum. The result is spread by up to 20 % either way.
fn backoff_delay(attempt: u32, random: u64) -> Duration {
    let exp = attempt.saturating_sub(1).min(16);
    let base = BACKOFF_BASE
        .saturating_mul(1u32 << exp)
        .min(BACKOFF_MAX)
        .as_secs_f64();
    // 0.8 to 1.2
    let jitter = 0.8 + 0.4 * (random as f64 / u64::MAX as f64);
    Duration::from_secs_f64(base * jitter)
}

/// Owns the state machine. Starts, stops and joins attempts.
struct Sup {
    shared: Rc<Shared>,
    /// The filter sets of the last broker sessions, newest first.
    sessions: Vec<(SessionKey, SessionSubs)>,
    generation: u64,
    next_token: u64,
    /// State of the jitter generator (xorshift64). Time seeds it. No `rand` crate needed.
    rng: u64,
}

impl Sup {
    fn random(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// The filter set of the broker session that `profile` connects to. The set survives a
    /// disconnect and the next Connect to the same session.
    fn session_subs(&mut self, config: &ConnectConfig) -> SessionSubs {
        let profile = &config.profile;
        let key = (
            profile.host.clone(),
            profile.port,
            profile.client_id.clone(),
        );
        let at = self.sessions.iter().position(|(k, _)| *k == key);
        let entry = match at {
            Some(i) => self.sessions.remove(i),
            // A session that the supervisor has not seen (the app started again) begins with
            // what the UI saved. A cached set is newer.
            None => {
                let set = SessionSet {
                    filters: config.session_filters.iter().cloned().collect(),
                    dirty: false,
                };
                (key, Rc::new(RefCell::new(set)))
            }
        };
        let set = entry.1.clone();
        self.sessions.insert(0, entry);
        self.sessions.truncate(MAX_SESSIONS);
        set
    }

    /// A user Connect.
    fn begin(&mut self, config: ConnectConfig, epoch: u64) -> Current {
        let generation = config.generation;
        self.generation = generation;
        self.shared.set_status(generation, ConnState::Connecting);
        tracing::info!(generation, transport = ?config.profile.transport, "connect requested");
        let mut current = Current {
            generation,
            epoch,
            desired: Rc::new(RefCell::new(
                config
                    .profile
                    .subscriptions
                    .iter()
                    .map(|(f, q)| (f.clone(), *q, config.profile.options_of(f), 0))
                    .collect(),
            )),
            maybe: self.session_subs(&config),
            announced: Rc::default(),
            tls: Rc::default(),
            config: Rc::new(config),
            retries: 0,
            attempt: None,
            wake: None,
        };
        self.start_attempt(&mut current);
        current
    }

    fn start_attempt(&mut self, c: &mut Current) {
        let token = self.next_token;
        self.next_token += 1;
        self.shared.active.set(token);
        c.wake = None;
        let (commands, rx) = mpsc::channel(256);
        let stop = Rc::new(Notify::new());
        let up = Rc::new(Cell::new(false));
        let ctx = AttemptCtx {
            token,
            generation: c.generation,
            reconnect: c.retries > 0,
            desired: c.desired.clone(),
            up: up.clone(),
            maybe: c.maybe.clone(),
            report: session_persists(&c.config),
            announced: c.announced.clone(),
            tls: c.tls.clone(),
        };
        tracing::info!(generation = c.generation, retry = c.retries, "connecting");
        let handle = tokio::task::spawn_local(run_session(
            c.config.clone(),
            ctx,
            self.shared.clone(),
            rx,
            stop.clone(),
        ));
        c.attempt = Some(Attempt {
            handle,
            commands,
            stop,
            up,
        });
    }

    /// The attempt task ended by itself. Returns the connect if it goes on (backoff).
    fn attempt_ended(
        &mut self,
        mut c: Current,
        result: Result<AttemptEnd, tokio::task::JoinError>,
    ) -> Option<Current> {
        // After a panic nothing of the old client is kept: its handles died with the task.
        c.attempt = None;
        self.shared.active.set(0);
        let generation = c.generation;
        let end = result.unwrap_or_else(|e| {
            AttemptEnd::never_up(Outcome::Failed(if e.is_panic() {
                PANIC_TEXT.into()
            } else {
                "The connection task was cancelled.".into()
            }))
        });
        match end.outcome {
            Outcome::Disconnected => {
                tracing::info!(generation, "disconnected");
                self.shared.set_status(generation, ConnState::Disconnected);
                None
            }
            Outcome::Failed(text) => {
                tracing::warn!(generation, reason = %text, "connection failed, no retry");
                self.shared.set_status(generation, ConnState::Failed(text));
                None
            }
            Outcome::Retry(reason) => {
                c.retries = next_retries(c.retries, end.up_for);
                let delay = backoff_delay(c.retries, self.random());
                tracing::warn!(
                    generation,
                    attempt = c.retries,
                    delay_ms = delay.as_millis() as u64,
                    reason = %reason,
                    "connection lost, retry after backoff"
                );
                c.wake = Some(tokio::time::Instant::now() + delay);
                self.shared.set_status(
                    generation,
                    ConnState::Reconnecting {
                        attempt: c.retries,
                        reason,
                    },
                );
                Some(c)
            }
        }
    }

    /// Cancels the attempt or the backoff, and waits (bounded) for the attempt to end.
    async fn stop(&mut self, mut c: Current) {
        let generation = c.generation;
        // From now on the old attempt cannot change the status or queue a message.
        self.shared.active.set(0);
        let Some(mut attempt) = c.attempt.take() else {
            // Backoff: no task runs.
            tracing::info!(generation, "disconnected during backoff");
            self.shared.set_status(generation, ConnState::Disconnected);
            return;
        };
        self.shared.set_status(generation, ConnState::Disconnecting);
        attempt.stop.notify_one();
        let state = match tokio::time::timeout(STOP_TIMEOUT, &mut attempt.handle).await {
            Ok(Err(e)) if e.is_panic() => ConnState::Failed(PANIC_TEXT.into()),
            Ok(_) => ConnState::Disconnected,
            Err(_) => {
                attempt.handle.abort();
                let _ = attempt.handle.await;
                ConnState::Disconnected
            }
        };
        tracing::info!(generation, "disconnected by user");
        self.shared.set_status(generation, state);
    }

    /// Subscribe, Unsubscribe and Publish from the UI.
    fn route(&self, current: &Option<Current>, command: Command) {
        let generation = self.generation;
        let Some(c) = current else {
            reject(&self.shared, generation, command, "Not connected.");
            return;
        };
        let up = c.attempt.as_ref().filter(|a| a.up.get());
        match command {
            Command::Subscribe {
                filter,
                qos,
                options,
                req,
            } => {
                if let Some(e) = filter_error(&filter) {
                    let why = format!("Invalid topic filter: {e}");
                    reject(
                        &self.shared,
                        generation,
                        Command::Subscribe {
                            filter,
                            qos,
                            options,
                            req,
                        },
                        &why,
                    );
                    return;
                }
                let mut desired = c.desired.borrow_mut();
                match desired.iter_mut().find(|(f, ..)| *f == filter) {
                    Some(entry) => (entry.1, entry.2, entry.3) = (qos, options, req),
                    None => desired.push((filter.clone(), qos, options, req)),
                }
                drop(desired);

                // Not up yet: the attempt reads `desired` at the CONNACK. After a loss the
                // next attempt does the same.
                if let Some(a) = up {
                    self.forward(
                        a,
                        Command::Subscribe {
                            filter,
                            qos,
                            options,
                            req,
                        },
                    );
                }
            }
            Command::Unsubscribe { filter } => {
                c.desired.borrow_mut().retain(|(f, ..)| *f != filter);
                match up {
                    Some(a) => self.forward(a, Command::Unsubscribe { filter }),
                    // The connection is down, so there is nothing to send now. The filter stays
                    // in the session set: after a CONNACK that resumes the session, the attempt
                    // removes it from the broker.
                    None => self.shared.event(Event::Unsubscribed {
                        generation,
                        filter,
                        result: Ok(()),
                    }),
                }
            }
            other => match &c.attempt {
                Some(a) => self.forward(a, other),
                None => reject(&self.shared, generation, other, "Not connected."),
            },
        }
    }

    fn forward(&self, attempt: &Attempt, command: Command) {
        if let Err(e) = attempt.commands.try_send(command) {
            reject(
                &self.shared,
                self.generation,
                e.into_inner(),
                "Too many pending requests.",
            );
        }
    }
}

/// Seed for the jitter. Never zero, because xorshift needs a non-zero state.
fn rng_seed() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64)
        | 1
}

async fn supervisor(
    mut commands: mpsc::Receiver<(u64, Command)>,
    mut cancel: watch::Receiver<u64>,
    shared: Rc<Shared>,
) {
    let mut sup = Sup {
        shared,
        sessions: Vec::new(),
        generation: 0,
        next_token: 1,
        rng: rng_seed(),
    };
    let mut current: Option<Current> = None;
    loop {
        let wake = current.as_ref().and_then(|c| c.wake);
        tokio::select! {
            command = commands.recv() => match command {
                None => break,
                // A disconnect came after this command was sent. The command is void.
                Some((epoch, command)) if epoch < *cancel.borrow() => {
                    reject(&sup.shared, sup.generation, command, "Not connected.");
                }
                Some((epoch, Command::Connect(config))) => {
                    // The generation of a Connect must grow. This also stops a Connect that
                    // did not come from `Net::connect`.
                    if config.generation <= sup.generation {
                        tracing::warn!(generation = config.generation, "connect ignored: old generation");
                        continue;
                    }
                    if let Some(old) = current.take() {
                        sup.stop(old).await;
                        // A disconnect can come while we stop the old connect.
                        if epoch < *cancel.borrow() {
                            continue;
                        }
                    }
                    current = Some(sup.begin(*config, epoch));
                }
                Some((_, other)) => sup.route(&current, other),
            },
            changed = cancel.changed() => {
                let value = if changed.is_ok() { *cancel.borrow_and_update() } else { SHUTDOWN };
                // This cancel is older than the connect. A Connect that was sent after the
                // disconnect can be read before the cancel. Do not stop that connect.
                if value != SHUTDOWN && current.as_ref().is_some_and(|c| c.epoch >= value) {
                    continue;
                }
                if let Some(c) = current.take() {
                    sup.stop(c).await;
                } else if matches!(sup.shared.status.borrow().state, ConnState::Failed(_)) {
                    sup.shared.set_status(sup.generation, ConnState::Disconnected);
                }
                if value == SHUTDOWN {
                    break;
                }
            },
            result = async {
                match current.as_mut().and_then(|c| c.attempt.as_mut()) {
                    Some(a) => (&mut a.handle).await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(c) = current.take() {
                    current = sup.attempt_ended(c, result);
                }
            },
            () = async {
                match wake {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(c) = current.as_mut() {
                    sup.start_attempt(c);
                }
            },
        }
    }
    if let Some(c) = current.take() {
        sup.stop(c).await;
    }
}

/// Answers a command that cannot run.
fn reject(shared: &Shared, generation: u64, command: Command, why: &str) {
    match command {
        Command::Publish(o) => shared.event(Event::Published {
            generation,
            op_id: o.op_id,
            result: PublishResult::Rejected(why.into()),
        }),
        Command::Subscribe { filter, req, .. } => shared.event(Event::Subscribed {
            generation,
            filter,
            req,
            result: Err(why.into()),
        }),
        Command::Unsubscribe { filter } => shared.event(Event::Unsubscribed {
            generation,
            result: Err(format!("Cannot unsubscribe from {filter}: {why}")),
            filter,
        }),
        Command::Connect(_) => {}
    }
}

/// One attempt, from connect to disconnect. Runs in a task, so a panic in the MQTT library
/// ends only this attempt.
async fn run_session(
    config: Rc<ConnectConfig>,
    ctx: AttemptCtx,
    shared: Rc<Shared>,
    mut commands: mpsc::Receiver<Command>,
    stop: Rc<Notify>,
) -> AttemptEnd {
    let generation = ctx.generation;
    let end = session(&config, &ctx, &shared, &mut commands, &stop).await;
    // The last changes of the connection (a lost UNSUBACK, for example) are reported too.
    report_filters(&shared, &ctx);
    commands.close();
    while let Ok(command) = commands.try_recv() {
        reject(&shared, generation, command, "Not connected.");
    }
    end
}

/// Why the session loop ended.
enum Ended {
    Driver(Outcome),
    /// An operation timed out and has reported it.
    Expired,
    /// An operation task panicked.
    OpPanic,
}

async fn session(
    config: &ConnectConfig,
    ctx: &AttemptCtx,
    shared: &Rc<Shared>,
    commands: &mut mpsc::Receiver<Command>,
    stop: &Notify,
) -> AttemptEnd {
    let generation = ctx.generation;
    // The files are read and parsed, and the connector is built, off the net thread (the system
    // CA store alone takes some ms). A stop during that returns at once.
    let (ssl, diag) = match tls_setup(config, &ctx.tls, stop).await {
        Ok(Some((ssl, diag))) => (Some(ssl), Some(diag)),
        Ok(None) => (None, None),
        Err(end) => return end,
    };
    let ingest = Rc::new(Ingest {
        shared: shared.clone(),
        generation,
        token: ctx.token,
    });
    let connect = wire::connect(Params {
        config,
        limits: shared.limits,
        ssl,
        diag: diag.as_ref(),
        ingest,
    });
    let result = tokio::select! {
        result = connect => result,
        () = stop.notified() => return AttemptEnd::never_up(Outcome::Disconnected),
    };
    let Connected {
        wire,
        session_present,
        details,
        broker_max_packet,
        mut driver,
    } = match result {
        Ok(connected) => connected,
        Err(outcome) => return AttemptEnd::never_up(outcome),
    };
    shared
        .broker_max_packet
        .store(broker_max_packet.unwrap_or(0), Ordering::Relaxed);
    let up_at = tokio::time::Instant::now();
    // No `await` from here to the end of the subscribe loop: the supervisor sees `up` and
    // `desired` as one step.
    ctx.up.set(true);
    let filters = ctx.desired.borrow().clone();
    // A new session holds nothing. A resumed one can hold filters that are not desired any
    // more (removed offline, or the UNSUBACK was lost): they are removed below.
    let removed: VecDeque<String> = {
        let mut maybe = ctx.maybe.borrow_mut();
        let removed: VecDeque<String> = if session_present {
            let desired: HashSet<&str> = filters.iter().map(|(d, ..)| d.as_str()).collect();
            maybe
                .filters
                .iter()
                .filter(|f| !desired.contains(f.as_str()))
                .cloned()
                .collect()
        } else {
            maybe.clear();
            VecDeque::new()
        };
        // Report the whole set once for each connection, even if it did not change. A new
        // profile can share the session (an imported copy with the same client ID) and has not
        // saved the set yet.
        if !ctx.announced.replace(true) {
            maybe.dirty = true;
        }
        removed
    };
    let mut resub = Resub {
        announce: false,
        pending: filters.into(),
        unsubs: removed,
        blocked: false,
    };
    if shared.set_connected(generation, ctx.token) {
        tracing::info!(generation, reconnect = ctx.reconnect, "connected");
        resub.announce = ctx.reconnect;
        // The app keeps only the details of the latest connection. They wait here, outside
        // the attempt, for room in the event log. A newer connection replaces them.
        shared
            .details
            .replace(Some((generation, ctx.token, Box::new(details))));
    }

    let mut ops = JoinSet::new();
    // An operation that times out reports it here. The session then ends.
    let expired = Rc::new(Notify::new());
    // Subscribe in batches: `dispatch` rejects more than `MAX_PENDING_OPS` operations.
    resub.feed(&wire, ctx, shared, &expired, &mut ops);

    // The user asked to disconnect: the connection closes gracefully, and `grace` forces it if
    // the close takes too long. The timer starts with the request and ends with the close.
    let (mut closing, mut forced) = (false, false);
    let grace = tokio::time::sleep(wire::CLOSE_GRACE);
    tokio::pin!(grace);
    let mut commands_open = true;
    // The timer runs only after a change of the session set. It makes one report for many.
    let report = tokio::time::sleep(REPORT_DELAY);
    tokio::pin!(report);
    let mut report_armed = false;
    let ended = loop {
        // With no room in the event log the timer does not start. The drain signal re-checks.
        let mut report_blocked = false;
        if !report_armed && ctx.report && ctx.maybe.borrow().dirty {
            if shared.has_room() {
                report
                    .as_mut()
                    .reset(tokio::time::Instant::now() + REPORT_DELAY);
                report_armed = true;
            } else {
                report_blocked = true;
            }
        }
        tokio::select! {
            () = &mut report, if report_armed => {
                report_armed = false;
                report_filters(shared, ctx);
            },
            outcome = &mut driver => break Ended::Driver(outcome),
            () = expired.notified() => break Ended::Expired,
            command = commands.recv(), if !closing && commands_open => match command {
                Some(command) => {
                    // A newer command for a filter replaces its queued re-subscribe.
                    match &command {
                        Command::Subscribe { filter, .. } | Command::Unsubscribe { filter } => {
                            resub.pending.retain(|(f, ..)| f != filter);
                            resub.unsubs.retain(|f| f != filter);
                        }
                        _ => {}
                    }
                    dispatch(&wire, command, ctx, shared, &expired, &mut ops);
                }
                None => commands_open = false,
            },
            () = stop.notified(), if !closing => {
                closing = true;
                wire.close();
                grace.as_mut().reset(tokio::time::Instant::now() + wire::CLOSE_GRACE);
            },
            () = &mut grace, if closing && !forced => {
                // The graceful close did not finish. End the I/O now.
                forced = true;
                wire.force_close();
            },
            Some(joined) = ops.join_next(), if !ops.is_empty() => {
                if joined.is_err_and(|e| e.is_panic()) {
                    break Ended::OpPanic;
                }
                resub.feed(&wire, ctx, shared, &expired, &mut ops);
            },
            // The UI took events out of the log. A waiting re-subscribe or report tries again.
            () = shared.drained.notified(), if resub.blocked || report_blocked => {
                resub.feed(&wire, ctx, shared, &expired, &mut ops);
            },
        }
    };
    let up_for = Some(up_at.elapsed());
    // Open operations end with "lost" when the connection ends. Let them report it.
    let outcome = match ended {
        Ended::Expired | Ended::OpPanic => {
            if matches!(ended, Ended::OpPanic) {
                tracing::error!(generation, "an operation task panicked");
            }
            // Drop the connection at once, and let ntex finish its own clean-up.
            wire.force_close();
            let _ = tokio::time::timeout(DRAIN_TIMEOUT, &mut driver).await;
            drain(&mut ops).await;
            let text = if matches!(ended, Ended::Expired) {
                OP_TIMEOUT_END_TEXT
            } else {
                PANIC_TEXT
            };
            return AttemptEnd {
                outcome: Outcome::Failed(text.into()),
                up_for,
            };
        }
        Ended::Driver(_) if closing => Outcome::Disconnected,
        Ended::Driver(outcome) => outcome,
    };
    drop(wire);
    drain(&mut ops).await;
    AttemptEnd { outcome, up_for }
}

/// What the session still has to send after the CONNACK: the `Reconnected` event and the
/// subscribe of every desired filter.
struct Resub {
    announce: bool,
    pending: VecDeque<Want>,
    /// Filters to remove from a resumed session (see [`SessionSubs`]).
    unsubs: VecDeque<String>,
    /// The last `feed` stopped because the event log has no room. It waits for the drain signal.
    blocked: bool,
}

impl Resub {
    /// Sends `Reconnected` and starts queued re-subscribes while the batch limit and the room in
    /// the event log allow. Half of the operation slots stay free for commands from the UI.
    /// With no room, it stops and sets `blocked`: the session loop calls it again when the UI
    /// drains the log.
    fn feed(
        &mut self,
        wire: &Wire,
        ctx: &AttemptCtx,
        shared: &Rc<Shared>,
        expired: &Rc<Notify>,
        ops: &mut JoinSet<()>,
    ) {
        let generation = ctx.generation;
        self.blocked = false;
        if shared.details.borrow().is_some() {
            if !shared.has_room() {
                self.blocked = true;
                return;
            }
            if let Some((generation, attempt, details)) = shared.details.take() {
                shared.event(Event::ConnectionDetails {
                    generation,
                    attempt,
                    details,
                });
            }
        }
        if self.announce {
            if !shared.has_room() {
                self.blocked = true;
                return;
            }
            shared.event(Event::Reconnected {
                generation,
                at: SystemTime::now(),
            });
            self.announce = false;
        }
        // These removals make no event, so they need no room in the log.
        while ops.len() < MAX_PENDING_OPS / 2
            && let Some(filter) = self.unsubs.pop_front()
        {
            ops.spawn_local(stale_unsubscribe_op(
                wire.clone(),
                filter,
                ctx.maybe.clone(),
                ctx.desired.clone(),
                expired.clone(),
                shared.limits.timers.op,
            ));
        }
        while ops.len() < MAX_PENDING_OPS / 2 && !self.pending.is_empty() {
            if !shared.has_room() {
                self.blocked = true;
                return;
            }
            if let Some((filter, qos, options, req)) = self.pending.pop_front() {
                let subscribe = Command::Subscribe {
                    filter,
                    qos,
                    options,
                    req,
                };
                dispatch(wire, subscribe, ctx, shared, expired, ops);
            }
        }
    }
}

async fn drain(ops: &mut JoinSet<()>) {
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, async {
        while ops.join_next().await.is_some() {}
    })
    .await;
}

pub(crate) const IO_CLOSED_TEXT: &str = "The broker closed the connection.";
const CLIENT_CERT_TEXT: &str =
    "The broker needs a client certificate. Add your certificate and key.";
/// Queues incoming publishes for the UI. Never waits for the UI.
struct Ingest {
    shared: Rc<Shared>,
    generation: u64,
    /// Attempt token. After a cancel or a loss, [`Shared::active`] differs and nothing is queued.
    token: u64,
}

impl Ingest {
    /// Queues the message that `make` builds for this generation. It drops the message (and
    /// counts it) when a queue is full.
    fn queue(&self, make: impl FnOnce(u64) -> Incoming) {
        let s = &*self.shared;
        // The attempt was cancelled: queue nothing more for the UI. This is not a drop.
        if s.active.get() != self.token {
            return;
        }
        let message = make(self.generation);
        let cost = queue_cost(&message);
        // Only this thread adds to the counter, so check-then-add is safe.
        let fits =
            s.queued_bytes.load(Ordering::Acquire).saturating_add(cost) <= s.limits.queue_bytes;
        if fits {
            s.queued_bytes.fetch_add(cost, Ordering::AcqRel);
            // The count goes up first: the UI can release the message at once.
            s.stats.queued_msgs.fetch_add(1, Ordering::AcqRel);
            if s.incoming.try_send(message).is_ok() {
                return;
            }
            s.queued_bytes.fetch_sub(cost, Ordering::AcqRel);
            s.stats.queued_msgs.fetch_sub(1, Ordering::AcqRel);
        }
        s.stats.drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// Starts one operation in its own task, so a full send queue never stalls the session loop.
/// At most `MAX_PENDING_OPS` operations run at one time. More are rejected.
fn dispatch(
    wire: &Wire,
    command: Command,
    ctx: &AttemptCtx,
    shared: &Rc<Shared>,
    expired: &Rc<Notify>,
    ops: &mut JoinSet<()>,
) {
    let generation = ctx.generation;
    let (maybe, desired) = (ctx.maybe.clone(), ctx.desired.clone());
    if ops.len() >= MAX_PENDING_OPS {
        reject(shared, generation, command, TOO_MANY_OPS_TEXT);
        return;
    }
    let (wire, shared, expired) = (wire.clone(), shared.clone(), expired.clone());
    match command {
        Command::Subscribe {
            filter,
            qos,
            options,
            req,
        } => {
            ops.spawn_local(subscribe_op(
                wire,
                (filter, qos, options, req),
                generation,
                maybe,
                shared,
                expired,
            ));
        }
        Command::Unsubscribe { filter } => {
            ops.spawn_local(unsubscribe_op(
                wire, filter, generation, maybe, desired, shared, expired,
            ));
        }
        Command::Publish(outgoing) => {
            ops.spawn_local(publish_op(wire, *outgoing, generation, shared, expired));
        }
        Command::Connect(_) => {}
    }
}

async fn subscribe_op(
    wire: Wire,
    (filter, qos, options, req): Want,
    generation: u64,
    maybe: SessionSubs,
    shared: Rc<Shared>,
    expired: Rc<Notify>,
) {
    let result = tokio::time::timeout(shared.limits.timers.op, async {
        if let Some(e) = filter_error(&filter) {
            return Err(format!("Invalid topic filter: {e}"));
        }
        // Only MQTT 5 sends the options, so only MQTT 5 can refuse them.
        if wire.sends_options()
            && let Err(why) = options.check(&filter)
        {
            return Err(why.to_owned());
        }
        // From the moment of the send, the broker may hold the filter.
        maybe.borrow_mut().insert(&filter);
        wire.subscribe(&filter, qos, options).await
    })
    .await
    .unwrap_or_else(|_| {
        expired.notify_one();
        Err(NO_ANSWER_TEXT.to_owned())
    });
    match &result {
        Ok(granted) => tracing::info!(generation, granted_qos = ?granted, "subscribed"),
        Err(why) => tracing::warn!(generation, reason = %why, "subscribe failed"),
    }
    tracing::debug!(generation, %filter, "subscribe result for filter");
    shared.event(Event::Subscribed {
        generation,
        filter,
        req,
        result,
    });
}

async fn unsubscribe_op(
    wire: Wire,
    filter: String,
    generation: u64,
    maybe: SessionSubs,
    desired: Rc<RefCell<Vec<Want>>>,
    shared: Rc<Shared>,
    expired: Rc<Notify>,
) {
    let fail = |why: &str| format!("Cannot unsubscribe from {filter}: {why}");
    let result = tokio::time::timeout(shared.limits.timers.op, async {
        if let Some(e) = filter_error(&filter) {
            return Err(fail(&format!("Invalid topic filter: {e}")));
        }
        wire.unsubscribe(&filter).await.map_err(|e| match e {
            UnsubFail::Lost(why) => fail(&why),
            UnsubFail::Rejected(text) => text,
        })
    })
    .await
    .unwrap_or_else(|_| {
        expired.notify_one();
        Err(fail(NO_ANSWER_TEXT))
    });
    if result.is_ok() {
        forget(&maybe, &desired, &filter);
    }
    shared.event(Event::Unsubscribed {
        generation,
        filter,
        result,
    });
}

/// The broker no longer holds `filter`: drop it from the session set. A filter that is desired
/// again stays (the user subscribed to it after the UNSUBSCRIBE was sent).
fn forget(maybe: &SessionSubs, desired: &Rc<RefCell<Vec<Want>>>, filter: &str) {
    if !desired.borrow().iter().any(|(f, ..)| f == filter) {
        maybe.borrow_mut().remove(filter);
    }
}

/// Removes a filter that is not desired from a resumed session (see [`SessionSubs`]). It makes
/// no event: the user got the result when he removed the filter. A failure keeps the filter in
/// the set, so the next CONNACK that resumes the session tries again.
async fn stale_unsubscribe_op(
    wire: Wire,
    filter: String,
    maybe: SessionSubs,
    desired: Rc<RefCell<Vec<Want>>>,
    expired: Rc<Notify>,
    limit: Duration,
) {
    let sent = tokio::time::timeout(limit, async {
        if filter_error(&filter).is_some() {
            return Err(());
        }
        wire.unsubscribe(&filter).await.map_err(|_| ())
    })
    .await;
    match sent {
        Ok(Ok(())) => {
            forget(&maybe, &desired, &filter);
            tracing::info!("removed a subscription from the resumed session");
        }
        Ok(Err(())) => {}
        Err(_) => expired.notify_one(),
    }
}

async fn publish_op(
    wire: Wire,
    o: Outgoing,
    generation: u64,
    shared: Rc<Shared>,
    expired: Rc<Notify>,
) {
    #[cfg(test)]
    if o.topic == "__panic__" {
        panic!("test: operation task panic");
    }
    let result = match tokio::time::timeout(shared.limits.timers.op, wire.publish(&o)).await {
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

// ---------------------------------------------------------------------------------------------
// TLS set-up and error text
// ---------------------------------------------------------------------------------------------

/// The TLS connector of one attempt, and its diagnosis slot.
type TlsParts = (openssl::ssl::SslConnector, TlsDiag);

/// The TLS configuration of one attempt for a TLS or WSS profile. `Ok(None)` for other
/// transports.
///
/// The first call of a connect reads the PEM files, parses them and decrypts the key. Every call
/// builds the connector (this loads the system CA store). All of it runs in a blocking task, so
/// the net thread keeps running. `stop` ends the wait at once (the task then finishes on its own
/// and its result is dropped). Later attempts of the same connect reuse the parsed material from
/// `slot`. An error is final for the connect, so it is not stored.
async fn tls_setup(
    c: &ConnectConfig,
    slot: &TlsSlot,
    stop: &Notify,
) -> Result<Option<TlsParts>, AttemptEnd> {
    let p = &c.profile;
    if !matches!(p.transport, Transport::Tls | Transport::Wss) {
        return Ok(None);
    }
    let failed = |text: String| AttemptEnd::never_up(Outcome::Failed(text));
    let pair = match (&p.client_cert, &p.client_key) {
        (Some(cert), Some(key)) => Some((cert.clone(), key.clone())),
        (None, None) => None,
        _ => {
            return Err(failed(
                "Set both the client certificate and the client key, or neither.".into(),
            ));
        }
    };
    let cached = slot.borrow().clone();
    let (ca, passphrase, verify) = (p.ca_file.clone(), c.key_passphrase.clone(), p.verify_tls);
    let job = tokio::task::spawn_blocking(move || {
        let material = match cached {
            Some(m) => m,
            None => Arc::new(TlsMaterial::load(
                ca.as_deref(),
                pair.as_ref().map(|(c, k)| (c.as_path(), k.as_path())),
                passphrase.as_deref(),
                verify,
            )?),
        };
        let parts = material.connector()?;
        Ok::<_, String>((material, parts))
    });
    let built = tokio::select! {
        built = job => built,
        () = stop.notified() => return Err(AttemptEnd::never_up(Outcome::Disconnected)),
    };
    match built {
        Ok(Ok((material, parts))) => {
            *slot.borrow_mut() = Some(material);
            Ok(Some(parts))
        }
        Ok(Err(text)) => Err(failed(text)),
        Err(e) => Err(failed(format!("TLS set-up stopped unexpectedly: {e}"))),
    }
}

fn io_outcome(e: &std::io::Error, diag: Option<&TlsDiag>, has_client_cert: bool) -> Outcome {
    use std::io::ErrorKind as K;
    if let Some(text) = diag.and_then(TlsDiag::text) {
        return Outcome::Failed(text);
    }
    let text = e.to_string();
    let lower = text.to_lowercase();
    match e.kind() {
        K::ConnectionRefused => {
            Outcome::Retry("The broker refused the connection. Check the host and the port.".into())
        }
        K::TimedOut => Outcome::Retry("The connection timed out.".into()),
        // The WebSocket upgrade timeout of ntex has no kind: its text is exactly "Timeout".
        K::Other if text == "Timeout" => Outcome::Retry("The connection timed out.".into()),
        K::UnexpectedEof | K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe => {
            Outcome::Retry(IO_CLOSED_TEXT.into())
        }
        K::NetworkUnreachable | K::HostUnreachable | K::NetworkDown => {
            Outcome::Retry("The network is not reachable.".into())
        }
        K::InvalidData if lower.contains("maximum packet size") => {
            Outcome::Failed("The broker sent a packet that is larger than the allowed size.".into())
        }
        // A TLS handshake that the peer cut off (for example a broker restart) is a transport
        // loss, not a certificate problem: no verify failure was recorded above.
        _ if lower.contains("unexpected eof")
            || lower.contains("connection reset")
            || lower.contains("broken pipe") =>
        {
            Outcome::Retry(IO_CLOSED_TEXT.into())
        }
        // No local verify failure was recorded, so a certificate alert comes from the peer. It
        // rejects our side of the handshake.
        _ if lower.contains("certificate required")
            || (lower.contains("alert") && lower.contains("bad certificate"))
            || (lower.contains("handshake failure") && !has_client_cert) =>
        {
            Outcome::Failed(CLIENT_CERT_TEXT.into())
        }
        _ if lower.contains("certificate") => Outcome::Failed(format!(
            "TLS: the server certificate is not valid ({text}). Add its CA file, or turn off verification."
        )),
        _ if lower.contains("temporary failure in name resolution") => {
            Outcome::Retry("The host name cannot be looked up now.".into())
        }
        _ if lower.contains("failed to lookup") || lower.contains("name or service") => {
            Outcome::Failed("The host name was not found.".into())
        }
        _ => Outcome::Failed(format!("Network error: {text}")),
    }
}

#[cfg(test)]
mod tests {
    use super::wire::{
        CONNECTION_CLOSED_TEXT, QOS2_TEXT, connack_is_temporary, disconnect_is_temporary,
        protocol_outcome,
    };
    use super::*;
    use ntex_mqtt::error::{DecodeError, MqttProtocolError};

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
            broker_max_packet: Arc::default(),
            drained: Arc::default(),
            details: RefCell::default(),
            limits: NetLimits::default(),
            active: Cell::new(0),
        });

        let profile = Profile {
            host: "127.0.0.1".into(),
            port,
            transport: Transport::Tcp,
            subscriptions: vec![],
            protocol: Protocol::V5,
            ..Profile::default()
        };
        cmd_tx
            .try_send((
                0,
                Command::Connect(Box::new(ConnectConfig {
                    generation: 1,
                    ..ConnectConfig::from(profile)
                })),
            ))
            .expect("queue connect");
        let subscribe = Command::Subscribe {
            filter: "a/b".into(),
            qos: Qos::AtMostOnce,
            options: Default::default(),
            req: 0,
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

    #[tokio::test]
    async fn invalid_publish_is_answered_with_an_event_and_not_queued() {
        let mut net = Net::start(NetLimits::default());
        assert_eq!(net.broker_max_packet(), None);
        let publish = |op_id, topic: &str| {
            Command::Publish(Box::new(Outgoing {
                op_id,
                topic: topic.into(),
                payload: Bytes::from_static(b"x"),
                qos: Qos::AtMostOnce,
                retain: false,
                props: MsgProps::default(),
            }))
        };
        net.send(publish(7, "a/+")).expect("rejected with an event");
        match net.events.try_recv() {
            Ok(Event::Published {
                op_id: 7,
                result: PublishResult::Rejected(why),
                ..
            }) => assert!(why.contains("wildcard"), "{why}"),
            other => panic!("unexpected {other:?}"),
        }
        // A valid topic is queued: the net thread answers (it is not connected).
        net.send(publish(8, "a/b")).expect("queued");
        let event = tokio::time::timeout(Duration::from_secs(5), net.events.recv())
            .await
            .expect("answer");
        assert!(matches!(
            event,
            Some(Event::Published {
                op_id: 8,
                result: PublishResult::Rejected(_),
                ..
            })
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
            protocol: Protocol::V5,
            ..Profile::default()
        };
        net.send(Command::Connect(Box::new(ConnectConfig::from(profile))))
            .expect("send");
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
            protocol: Protocol::V5,
        };
        let mut heavy = empty.clone();
        heavy.topic = Arc::from("a/long/topic");
        heavy.props.user = vec![("key".into(), "x".repeat(100))];
        assert!(queue_cost(&heavy) >= queue_cost(&empty) + 12 + 103);
    }

    // -----------------------------------------------------------------------------------------
    // Reconnect tests. The fake brokers speak just enough MQTT 5.
    // -----------------------------------------------------------------------------------------

    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::atomic::AtomicBool;

    /// A fake broker. `script(index, socket)` runs for each accepted connection, one after the
    /// other. Dropping the broker stops the accept loop.
    struct Fake {
        port: u16,
        accepts: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
    }

    impl Fake {
        fn start(script: impl Fn(usize, TcpStream) + Send + 'static) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.set_nonblocking(true).expect("nonblocking");
            let port = listener.local_addr().expect("addr").port();
            let accepts = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let (a, s) = (accepts.clone(), stop.clone());
            std::thread::spawn(move || {
                while !s.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((sock, _)) => {
                            sock.set_nonblocking(false).expect("blocking");
                            sock.set_read_timeout(Some(Duration::from_secs(5)))
                                .expect("timeout");
                            let n = a.fetch_add(1, Ordering::SeqCst);
                            script(n, sock);
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            });
            Self {
                port,
                accepts,
                stop,
            }
        }

        fn accepts(&self) -> usize {
            self.accepts.load(Ordering::SeqCst)
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// One packet: first byte and body. `None` on EOF, timeout or error.
    fn read_packet(sock: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
        let mut first = [0u8; 1];
        sock.read_exact(&mut first).ok()?;
        let (mut len, mut shift) = (0usize, 0);
        loop {
            let mut b = [0u8; 1];
            sock.read_exact(&mut b).ok()?;
            len |= usize::from(b[0] & 0x7f) << shift;
            if b[0] & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let mut body = vec![0u8; len];
        sock.read_exact(&mut body).ok()?;
        Some((first[0], body))
    }

    /// Reads the CONNECT and answers with a CONNACK.
    fn accept_connect(sock: &mut TcpStream, reason: u8) -> bool {
        match read_packet(sock) {
            Some((0x10, _)) => {}
            _ => return false,
        }
        sock.write_all(&[0x20, 0x03, 0x00, reason, 0x00]).is_ok()
    }

    /// Filters in a SUBSCRIBE body, with their requested QoS.
    fn subscribe_filters(body: &[u8]) -> Vec<(String, u8)> {
        let mut i = 2; // packet id
        i += 1; // property length (always 0 from this client)
        let mut out = vec![];
        while i + 2 <= body.len() {
            let n = usize::from(u16::from_be_bytes([body[i], body[i + 1]]));
            let end = i + 2 + n;
            out.push((
                String::from_utf8_lossy(&body[i + 2..end]).into_owned(),
                body[end] & 3,
            ));
            i = end + 1;
        }
        out
    }

    /// Answers a SUBSCRIBE with a SUBACK that grants the requested QoS.
    fn answer_subscribe(sock: &mut TcpStream, body: &[u8]) -> Vec<String> {
        let filters = subscribe_filters(body);
        let mut ack = vec![0x90, (3 + filters.len()) as u8, body[0], body[1], 0x00];
        ack.extend(filters.iter().map(|(_, qos)| *qos));
        let _ = sock.write_all(&ack);
        filters.into_iter().map(|(f, _)| f).collect()
    }

    fn tcp_profile(port: u16, subs: &[&str]) -> Profile {
        Profile {
            host: "127.0.0.1".into(),
            port,
            transport: Transport::Tcp,
            subscriptions: subs
                .iter()
                .map(|f| ((*f).into(), Qos::AtMostOnce))
                .collect(),
            protocol: Protocol::V5,
            ..Profile::default()
        }
    }

    async fn wait_status(net: &mut Net, f: impl Fn(&Status) -> bool) -> Status {
        tokio::time::timeout(Duration::from_secs(5), net.status.wait_for(f))
            .await
            .expect("status timeout")
            .expect("status closed")
            .clone()
    }

    /// The next event, with `ConnectionDetails` (new in S2d, one for each connect) left out:
    /// the older tests do not look at it. `next_event_any` keeps it.
    async fn next_event(net: &mut Net) -> Event {
        loop {
            match next_event_any(net).await {
                Event::ConnectionDetails { .. } => {}
                other => return other,
            }
        }
    }

    async fn next_event_any(net: &mut Net) -> Event {
        tokio::time::timeout(Duration::from_secs(5), net.events.recv())
            .await
            .expect("event timeout")
            .expect("events closed")
    }

    /// A broker that sends QoS 2 breaks the protocol. The session ends with a plain text and no
    /// retry. The net thread lives on and can connect again.
    #[tokio::test(flavor = "current_thread")]
    async fn qos2_publish_fails_the_session_without_retry_or_panic() {
        let (got_tx, got_rx) = std::sync::mpsc::channel();
        let got_tx = std::sync::Mutex::new(got_tx);
        let broker = Fake::start(move |n, mut sock| {
            if !accept_connect(&mut sock, 0) {
                return;
            }
            if n == 0 {
                let Some((0x82, body)) = read_packet(&mut sock) else {
                    return;
                };
                answer_subscribe(&mut sock, &body);
                // PUBLISH QoS 2, topic "t", packet ID 1, no properties, payload "x".
                let _ = sock.write_all(&[0x34, 0x07, 0x00, 0x01, b't', 0x00, 0x01, 0x00, b'x']);
            }
            // The client sends a DISCONNECT with reason 0x9B (QoS not supported) and no
            // properties. Then the connection ends.
            if n == 0 {
                let got = read_packet(&mut sock);
                got_tx.lock().expect("lock").send(got).expect("send");
            } else {
                while read_packet(&mut sock).is_some() {}
            }
        });
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(broker.port, &["#"]),
        ))))
        .expect("send");
        let s = wait_status(&mut net, |s| matches!(s.state, ConnState::Failed(_))).await;
        assert!(
            matches!(&s.state, ConnState::Failed(t) if t == QOS2_TEXT),
            "{s:?}"
        );
        assert_eq!(
            got_rx.recv_timeout(Duration::from_secs(2)).expect("packet"),
            Some((0xe0, vec![0x9b, 0x00])),
            "DISCONNECT 0x9B"
        );
        tokio::time::sleep(BACKOFF_MAX).await;
        assert!(matches!(net.status.borrow().state, ConnState::Failed(_)));
        assert_eq!(broker.accepts(), 1, "no retry");
        // The thread is alive: a new connect works.
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(broker.port, &[]),
        ))))
        .expect("send");
        wait_status(&mut net, |s| s.state == ConnState::Connected).await;
        assert_eq!(broker.accepts(), 2);
        net.disconnect();
        net.shutdown().await;
    }

    /// A panic in an operation task ends the session as Failed. The net thread lives on.
    #[tokio::test(flavor = "current_thread")]
    async fn operation_panic_fails_the_session_not_the_thread() {
        let (port, server) = silent_broker();
        let mut net = Net::start(NetLimits::default());
        connect_to(&mut net, port).await;
        let mut panic_publish = publish_cmd(1);
        if let Command::Publish(o) = &mut panic_publish {
            o.topic = "__panic__".into();
        }
        net.send(panic_publish).expect("send");
        wait_status(
            &mut net,
            |s| matches!(&s.state, ConnState::Failed(t) if t == PANIC_TEXT),
        )
        .await;
        server.join().expect("server");
        let (port, server) = silent_broker();
        connect_to(&mut net, port).await;
        net.shutdown().await;
        server.join().expect("server");
    }

    /// The TLS files are read off the net thread. A CA "file" that blocks for ever (a FIFO with no
    /// writer) must not stop a Disconnect.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn disconnect_during_the_tls_build_returns_at_once() {
        let dir = tempfile::tempdir().expect("dir");
        let fifo = dir.path().join("ca.pem");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !made.is_ok_and(|s| s.success()) {
            return; // no mkfifo on this machine
        }
        let mut net = Net::start(NetLimits::default());
        let profile = Profile {
            host: "localhost".into(),
            port: 1,
            transport: Transport::Tls,
            ca_file: Some(fifo.clone()),
            subscriptions: vec![],
            protocol: Protocol::V5,
            ..Profile::default()
        };
        net.connect(ConnectConfig::from(profile)).expect("connect");
        wait_status(&mut net, |s| matches!(s.state, ConnState::Connecting)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let t0 = tokio::time::Instant::now();
        net.disconnect();
        wait_status(&mut net, |s| matches!(s.state, ConnState::Disconnected)).await;
        assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
        // Let the blocked read end, so that the thread can finish.
        drop(std::fs::OpenOptions::new().write(true).open(&fifo));
        net.shutdown().await;
    }

    /// An attempt that is not the active one cannot change the status or queue a message.
    #[test]
    fn stale_attempt_results_are_dropped() {
        let (incoming, mut incoming_rx) = mpsc::channel(4);
        let (events, _events_rx) = mpsc::channel(4);
        let (status, status_rx) = watch::channel(Status::default());
        let shared = Rc::new(Shared {
            incoming,
            events,
            status,
            stats: Arc::default(),
            queued_bytes: Arc::default(),
            broker_max_packet: Arc::default(),
            drained: Arc::default(),
            details: RefCell::default(),
            limits: NetLimits::default(),
            active: Cell::new(0),
        });
        shared.set_status(1, ConnState::Connecting);
        shared.active.set(2); // attempt 2 is the active one
        assert!(!shared.set_connected(1, 1), "attempt 1 is stale");
        assert_eq!(status_rx.borrow().state, ConnState::Connecting);

        let publish = |g: u64| Incoming {
            generation: g,
            topic: Arc::from("t"),
            payload: Bytes::from_static(b"x"),
            qos: Qos::AtMostOnce,
            retain: false,
            props: MsgProps::default(),
            received_at: SystemTime::now(),
            protocol: Protocol::V5,
        };
        let stale = Ingest {
            shared: shared.clone(),
            generation: 1,
            token: 1,
        };
        stale.queue(publish);
        assert!(incoming_rx.try_recv().is_err(), "stale message was queued");
        assert_eq!(shared.queued_bytes.load(Ordering::Acquire), 0);
        assert_eq!(shared.stats.drops.load(Ordering::Relaxed), 0);
        let live = Ingest { token: 2, ..stale };
        live.queue(publish);
        assert!(incoming_rx.try_recv().is_ok());

        assert!(shared.set_connected(1, 2));
        assert_eq!(status_rx.borrow().state, ConnState::Connected);
        // A cancel (active = 0) also blocks the attempt.
        shared.set_status(1, ConnState::Connecting);
        shared.active.set(0);
        assert!(!shared.set_connected(1, 2));
    }

    /// The queue length counter goes up on a queued message and never counts a dropped one.
    #[test]
    fn queue_length_counts_queued_messages_only() {
        let (incoming, mut incoming_rx) = mpsc::channel(1);
        let (events, _events_rx) = mpsc::channel(4);
        let (status, _status_rx) = watch::channel(Status::default());
        let shared = Rc::new(Shared {
            incoming,
            events,
            status,
            stats: Arc::default(),
            queued_bytes: Arc::default(),
            broker_max_packet: Arc::default(),
            drained: Arc::default(),
            details: RefCell::default(),
            limits: NetLimits::default(),
            active: Cell::new(1),
        });
        let ingest = Ingest {
            shared: shared.clone(),
            generation: 1,
            token: 1,
        };
        let publish = |g: u64| Incoming {
            generation: g,
            topic: Arc::from("t"),
            payload: Bytes::from_static(b"x"),
            qos: Qos::AtMostOnce,
            retain: false,
            props: MsgProps::default(),
            received_at: SystemTime::now(),
            protocol: Protocol::V5,
        };
        ingest.queue(publish);
        ingest.queue(publish); // the channel holds one message: this is a drop
        assert_eq!(shared.stats.queued_msgs.load(Ordering::Acquire), 1);
        assert_eq!(shared.stats.drops.load(Ordering::Relaxed), 1);
        assert!(incoming_rx.try_recv().is_ok());
    }

    #[test]
    fn backoff_doubles_is_capped_and_jittered() {
        // `random` = u64::MAX / 2 gives a factor of 1.0.
        let mid = u64::MAX / 2;
        let at = |n, r| backoff_delay(n, r).as_secs_f64();
        let base = BACKOFF_BASE.as_secs_f64();
        let max = BACKOFF_MAX.as_secs_f64();
        assert!((at(1, mid) - base).abs() < 1e-3);
        assert!((at(2, mid) - 2.0 * base).abs() < 1e-3);
        assert!((at(3, mid) - 4.0 * base).abs() < 1e-3);
        assert!((at(40, mid) - max).abs() < 1e-3, "capped");
        assert!((at(u32::MAX, mid) - max).abs() < 1e-3, "no overflow");
        assert!((at(1, 0) - 0.8 * base).abs() < 1e-3, "-20 %");
        assert!((at(1, u64::MAX) - 1.2 * base).abs() < 1e-3, "+20 %");
    }

    #[test]
    fn retry_counter_resets_after_a_stable_connection() {
        assert_eq!(next_retries(0, None), 1);
        assert_eq!(next_retries(3, None), 4);
        assert_eq!(
            next_retries(3, Some(STABLE_AFTER - Duration::from_millis(1))),
            4
        );
        assert_eq!(next_retries(3, Some(STABLE_AFTER)), 1);
    }

    #[test]
    fn temporary_and_final_reasons_are_sorted() {
        use ntex_mqtt::v5::codec::ConnectAckReason as C;
        for r in [
            C::ServerUnavailable,
            C::ServerBusy,
            C::ConnectionRateExceeded,
            C::QuotaExceeded,
        ] {
            assert!(connack_is_temporary(r), "{r:?}");
        }
        for r in [
            C::NotAuthorized,
            C::BadUserNameOrPassword,
            C::Banned,
            C::BadAuthenticationMethod,
            C::UnsupportedProtocolVersion,
            C::ClientIdentifierNotValid,
            C::PacketTooLarge,
            C::ProtocolError,
            C::UnspecifiedError,
            C::UseAnotherServer,
        ] {
            assert!(!connack_is_temporary(r), "{r:?}");
        }
        use ntex_mqtt::v5::codec::DisconnectReasonCode as D;
        for r in [
            D::ServerBusy,
            D::ServerShuttingDown,
            D::ConnectionRateExceeded,
            D::MaximumConnectTime,
        ] {
            assert!(disconnect_is_temporary(r), "{r:?}");
        }
        for r in [
            D::NotAuthorized,
            D::ProtocolError,
            D::SessionTakenOver,
            D::PacketTooLarge,
            D::UnspecifiedError,
        ] {
            assert!(!disconnect_is_temporary(r), "{r:?}");
        }
    }

    #[test]
    fn io_errors_are_sorted() {
        use std::io::{Error, ErrorKind as K};
        let retry = |e: Error| matches!(io_outcome(&e, None, false), Outcome::Retry(_));
        for kind in [
            K::ConnectionRefused,
            K::TimedOut,
            K::UnexpectedEof,
            K::ConnectionReset,
            K::BrokenPipe,
            K::NetworkUnreachable,
        ] {
            assert!(retry(Error::from(kind)), "{kind:?}");
        }
        assert!(retry(Error::other(
            "failed to lookup address information: Temporary failure in name resolution"
        )));
        // The WebSocket upgrade timeout of ntex: kind Other, text "Timeout".
        assert!(retry(Error::other("Timeout")));
        for e in [
            Error::other("failed to lookup address information: Name or service not known"),
            Error::new(K::InvalidData, "packet exceeds maximum packet size"),
            Error::other("something new"),
            Error::other("Timeout while waiting for something else"),
            Error::other("certificate verify failed"),
        ] {
            assert!(!retry(Error::new(e.kind(), e.to_string())), "{e}");
        }
        // A recorded certificate problem always stops, whatever the I/O error says.
        let diag = TlsDiag::default();
        diag.record(crate::tls::VerifyFailure {
            code: 20,
            text: "unable to get local issuer certificate".into(),
            not_after: None,
        });
        assert!(matches!(
            io_outcome(&Error::from(K::UnexpectedEof), Some(&diag), false),
            Outcome::Failed(t) if t.contains("not trusted")
        ));
    }

    #[test]
    fn tls_handshake_cut_off_is_retried() {
        let e = std::io::Error::other("error:0A000126:SSL routines::unexpected eof while reading");
        assert!(matches!(
            io_outcome(&e, Some(&TlsDiag::default()), false),
            Outcome::Retry(_)
        ));
    }

    #[test]
    fn missing_client_certificate_is_not_blamed_on_the_server() {
        let text = |msg: &str, has_cert: bool| match io_outcome(
            &std::io::Error::other(msg.to_owned()),
            Some(&TlsDiag::default()),
            has_cert,
        ) {
            Outcome::Failed(t) => t,
            other => panic!("expected Failed, got {other:?}"),
        };
        for msg in [
            "error:0A00045C:SSL routines::tlsv13 alert certificate required",
            "error:0A000412:SSL routines::sslv3 alert bad certificate",
        ] {
            assert_eq!(text(msg, false), CLIENT_CERT_TEXT, "{msg}");
        }
        let hs = "error:0A000410:SSL routines::sslv3 alert handshake failure";
        assert_eq!(text(hs, false), CLIENT_CERT_TEXT);
        assert!(!text(hs, true).contains("client certificate"));
        // Other certificate errors still blame the server.
        assert!(text("certificate verify failed", false).contains("server certificate"));
    }

    /// More than `MAX_PENDING_OPS` desired filters: all get subscribed, and never more than
    /// the cap runs at one time.
    #[tokio::test(flavor = "current_thread")]
    async fn many_desired_filters_are_all_subscribed_within_the_cap() {
        const TOTAL: usize = MAX_PENDING_OPS + 100;
        let max_open = Arc::new(AtomicUsize::new(0));
        let seen_max = max_open.clone();
        let broker = Fake::start(move |_, mut sock| {
            if !accept_connect(&mut sock, 0) {
                return;
            }
            sock.set_read_timeout(Some(Duration::from_millis(100)))
                .expect("timeout");
            // Hold the answers until the client is idle, so the open count shows the batch size.
            let (mut open, mut total) = (vec![], 0);
            loop {
                match read_packet(&mut sock) {
                    Some((0x82, body)) => {
                        open.push(body);
                        total += 1;
                        seen_max.fetch_max(open.len(), Ordering::SeqCst);
                    }
                    Some(_) => {}
                    None if open.is_empty() && total >= TOTAL => return,
                    None => {
                        for body in open.drain(..) {
                            answer_subscribe(&mut sock, &body);
                        }
                    }
                }
            }
        });
        let names: Vec<String> = (0..TOTAL).map(|i| format!("f/{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(broker.port, &refs),
        ))))
        .expect("send");
        let mut subscribed = std::collections::HashSet::new();
        while subscribed.len() < TOTAL {
            match next_event(&mut net).await {
                Event::Subscribed {
                    filter,
                    result: Ok(_),
                    ..
                } => {
                    subscribed.insert(filter);
                }
                Event::Subscribed { filter, result, .. } => {
                    panic!("{filter} was not subscribed: {result:?}")
                }
                _ => {}
            }
        }
        assert_eq!(subscribed.len(), TOTAL);
        let max = max_open.load(Ordering::SeqCst);
        assert!(max > 1 && max <= MAX_PENDING_OPS, "{max} open at once");
        net.shutdown().await;
    }

    // -----------------------------------------------------------------------------------------
    // Tests of the ntex port (S2)
    // -----------------------------------------------------------------------------------------

    /// Each protocol error of ntex has its own text. Keep alive, write timeout and read timeout
    /// are temporary; a packet over the limit and malformed data are final.
    #[test]
    fn protocol_errors_have_their_own_outcome() {
        assert_eq!(
            protocol_outcome(&MqttProtocolError::KeepAliveTimeout),
            Outcome::Retry(CONNECTION_CLOSED_TEXT.into())
        );
        assert_eq!(
            protocol_outcome(&MqttProtocolError::WriteTimeout),
            Outcome::Retry("The broker did not read our data for 10 s.".into())
        );
        assert_eq!(
            protocol_outcome(&MqttProtocolError::ReadTimeout),
            Outcome::Retry("The broker stopped in the middle of a packet.".into())
        );
        assert_eq!(
            protocol_outcome(&MqttProtocolError::Decode(DecodeError::MaxSizeExceeded {
                size: 2,
                max_size: 1
            })),
            Outcome::Failed(
                "The broker sent a packet that is larger than the allowed size.".into()
            )
        );
        assert_eq!(
            protocol_outcome(&MqttProtocolError::Decode(DecodeError::MalformedPacket)),
            Outcome::Failed("The broker sent data that is not valid MQTT 5.".into())
        );
    }

    /// The first protocol call never ends. The second one panics after an await. ntex runs it
    /// in a detached task, so only our `catch_unwind` can end the attempt: it must end with
    /// `Failed`, not hang. The net thread then connects again.
    #[tokio::test(flavor = "current_thread")]
    async fn a_handler_panic_after_an_await_fails_the_attempt() {
        let broker = Fake::start(|n, mut sock| {
            if !accept_connect(&mut sock, 0) {
                return;
            }
            if n == 0 {
                let Some((0x82, body)) = read_packet(&mut sock) else {
                    return;
                };
                answer_subscribe(&mut sock, &body);
                // Two PUBLISH packets, QoS 0, no properties, payload "x".
                for topic in [&b"__handler_hold__"[..], &b"__handler_panic__"[..]] {
                    let mut packet =
                        vec![0x30, (2 + topic.len() + 2) as u8, 0x00, topic.len() as u8];
                    packet.extend_from_slice(topic);
                    packet.extend_from_slice(&[0x00, b'x']);
                    let _ = sock.write_all(&packet);
                }
            }
            while read_packet(&mut sock).is_some() {}
        });
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(broker.port, &["#"]),
        ))))
        .expect("send");
        let s = wait_status(&mut net, |s| matches!(s.state, ConnState::Failed(_))).await;
        assert!(
            matches!(&s.state, ConnState::Failed(t) if t == PANIC_TEXT),
            "{s:?}"
        );
        assert_eq!(broker.accepts(), 1, "no retry after a panic");
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(broker.port, &[]),
        ))))
        .expect("send");
        wait_status(&mut net, |s| s.state == ConnState::Connected).await;
        net.disconnect();
        net.shutdown().await;
    }

    /// The peer closes the TCP connection before it answers the WebSocket upgrade. That is a
    /// temporary close: the attempt reconnects. Without the mapping of `Disconnected(None)` it
    /// would be `Failed("Network error: ...")`.
    #[tokio::test(flavor = "current_thread")]
    async fn a_close_before_the_upgrade_answer_is_retried() {
        let broker = Fake::start(|_, mut sock| {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf); // the upgrade request
            drop(sock);
        });
        let mut net = Net::start(NetLimits::default());
        let profile = Profile {
            transport: Transport::Ws,
            ws_path: "/mqtt".into(),
            ..tcp_profile(broker.port, &[])
        };
        net.send(Command::Connect(Box::new(ConnectConfig::from(profile))))
            .expect("send");
        let s = wait_status(&mut net, |s| {
            matches!(
                s.state,
                ConnState::Reconnecting { .. } | ConnState::Failed(_)
            )
        })
        .await;
        assert!(
            matches!(&s.state, ConnState::Reconnecting { attempt: 1, reason } if reason == "The broker closed the connection."),
            "{s:?}"
        );
        net.disconnect();
        net.shutdown().await;
    }

    /// The reasons for a bad topic name are fixed texts (taken from the old client in S2 round 2):
    /// controls, NUL, non-characters, unassigned code points, wildcards, empty, too long.
    #[test]
    fn topic_errors_have_fixed_texts() {
        let long = "a".repeat(70_000);
        let cases: [(&str, Option<&str>); 19] = [
            ("", Some("empty topic")),
            ("a/b", None),
            ("a/+", Some("bad topic a/+")),
            ("a/#", Some("bad topic a/#")),
            ("a#", Some("bad topic a#")),
            ("a\0b", Some("bad string: contains U+0000")),
            ("a\u{7}b", Some("bad string: contains control codepoint")),
            ("a\u{85}b", Some("bad string: contains control codepoint")),
            ("a\u{7f}b", Some("bad string: contains control codepoint")),
            (
                "a\u{fdd0}b",
                Some("bad string: contains non-character codepoint"),
            ),
            (
                "a\u{ffff}b",
                Some("bad string: contains non-character codepoint"),
            ),
            (
                "a\u{10fffe}b",
                Some("bad string: contains non-character codepoint"),
            ),
            (
                "a\u{378}b",
                Some("bad string: contains non-character codepoint"),
            ),
            (
                "a\u{e0080}b",
                Some("bad string: contains non-character codepoint"),
            ),
            ("a\u{10fffd}b", None),
            ("a\u{e000}b", None),
            ("a\u{200b}b", None),
            ("a/\u{e9}/\u{1f600}", None),
            (&long, Some("bad string: longer than 65,535 bytes")),
        ];
        for (t, want) in cases {
            assert_eq!(
                crate::topic::topic_error(t).as_deref(),
                want,
                "{:?}",
                t.chars().take(20).collect::<String>()
            );
        }
    }

    /// A TLS or WSS server on the IPv6 loopback, with a certificate for the IP address `::1`
    /// (an IP entry). The URL of WSS has brackets around the address, but the name for
    /// verification must not have them. After the handshake the server closes, so the attempt
    /// is temporary (`Reconnecting`). With "[::1]" as the name, verification fails and the
    /// attempt is final (`Failed`).
    #[tokio::test(flavor = "current_thread")]
    async fn tls_and_wss_verify_an_ipv6_address_without_brackets() {
        use crate::tls::testutil::{TempDir, cert, key};
        use openssl::ssl::{SslAcceptor, SslMethod};
        let (ca_key, srv_key) = (key(), key());
        let ca = cert("Test CA", &ca_key, None, true, -1, 30);
        let leaf = cert("::1", &srv_key, Some((&ca, &ca_key)), false, -1, 30);
        let dir = TempDir::new();
        let ca_path = dir.write("ca.pem", &ca.to_pem().expect("pem"));
        for transport in [Transport::Wss, Transport::Tls] {
            let Ok(listener) = std::net::TcpListener::bind("[::1]:0") else {
                panic!("the IPv6 loopback is needed for this test");
            };
            let port = listener.local_addr().expect("addr").port();
            let mut acceptor =
                SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).expect("acceptor");
            acceptor.set_certificate(&leaf).expect("cert");
            acceptor.set_private_key(&srv_key).expect("key");
            let acceptor = acceptor.build();
            let (ok_tx, ok_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                while let Ok((sock, _)) = listener.accept() {
                    // The handshake ends well only if the client accepted our certificate.
                    let _ = ok_tx.send(acceptor.accept(sock).is_ok());
                }
            });
            let mut net = Net::start(NetLimits::default());
            let profile = Profile {
                host: "::1".into(),
                port,
                transport,
                ca_file: Some(ca_path.clone()),
                verify_tls: true,
                subscriptions: vec![],
                protocol: Protocol::V5,
                ..Profile::default()
            };
            net.connect(ConnectConfig::from(profile)).expect("connect");
            let s = wait_status(&mut net, |s| {
                matches!(
                    s.state,
                    ConnState::Reconnecting { .. } | ConnState::Failed(_)
                )
            })
            .await;
            assert!(
                matches!(s.state, ConnState::Reconnecting { .. }),
                "{transport:?}: {s:?}"
            );
            assert_eq!(ok_rx.recv_timeout(Duration::from_secs(2)), Ok(true));
            net.disconnect();
            net.shutdown().await;
        }
    }

    /// The reasons for a bad filter are fixed texts (taken from the old client): each kind of fault.
    #[test]
    fn filter_errors_have_fixed_texts() {
        let long = "a".repeat(70_000);
        let cases: [(&str, Option<&str>); 34] = [
            ("", Some("empty filter")),
            ("a/b", None),
            ("a/+/c", None),
            ("#", None),
            ("+", None),
            ("a/#", None),
            ("$SYS/#", None),
            ("$share/g/a/#", None),
            ("$share/g/", Some("empty filter")),
            ("$share/g", Some("bad filter $share/g")),
            ("$share//a", Some("bad filter $share//a")),
            ("$share/+/a", Some("bad filter $share/+/a")),
            ("$share/g/a#", Some("bad filter a#")),
            ("foo/#baz", Some("bad filter foo/#baz")),
            ("foo/baz#", Some("bad filter foo/baz#")),
            ("foo/#/baz", Some("bad filter foo/#/baz")),
            ("a+", Some("bad filter a+")),
            ("+a", Some("bad filter +a")),
            ("a/b+", Some("bad filter a/b+")),
            (
                "a\u{378}b",
                Some("bad string: contains non-character codepoint"),
            ),
            (
                "a\u{e0080}b",
                Some("bad string: contains non-character codepoint"),
            ),
            ("a\u{10fffd}b", None),
            ("a\u{e000}b", None),
            ("a\u{200b}b", None),
            (
                "+/\u{378}",
                Some("bad string: contains non-character codepoint"),
            ),
            ("$share/g/\u{e000}/#", None),
            ("a\0b", Some("bad string: contains U+0000")),
            ("a\u{7}b", Some("bad string: contains control codepoint")),
            ("a\u{85}b", Some("bad string: contains control codepoint")),
            (
                "a\u{fdd0}b",
                Some("bad string: contains non-character codepoint"),
            ),
            (
                "a\u{ffff}b",
                Some("bad string: contains non-character codepoint"),
            ),
            (
                "a\u{10fffe}b",
                Some("bad string: contains non-character codepoint"),
            ),
            ("a/\u{e9}/\u{1f600}", None),
            (&long, Some("bad string: longer than 65,535 bytes")),
        ];
        for (t, want) in cases {
            assert_eq!(
                filter_error(t).as_deref(),
                want,
                "{:?}",
                t.chars().take(20).collect::<String>()
            );
        }
    }

    // -----------------------------------------------------------------------------------------
    // Tests of the MQTT 3.1.1 adapter (S2b)
    // -----------------------------------------------------------------------------------------

    use super::wire::v3_max_remaining;

    fn v3_config(p: Profile) -> ConnectConfig {
        ConnectConfig {
            protocol: Protocol::V311,
            ..ConnectConfig::from(p)
        }
    }

    async fn connect_v3_to(net: &mut Net, p: Profile) {
        net.send(Command::Connect(Box::new(v3_config(p))))
            .expect("send");
    }

    /// Reads the CONNECT, answers with a 3.1.1 CONNACK (`rc`, no session) and returns the CONNECT
    /// body.
    fn accept_v3(sock: &mut TcpStream, rc: u8) -> Option<Vec<u8>> {
        let (kind, body) = read_packet(sock)?;
        if kind != 0x10 {
            return None;
        }
        sock.write_all(&[0x20, 0x02, 0x00, rc]).ok()?;
        Some(body)
    }

    /// The whole PUBLISH (QoS 0, topic "t") with a given Remaining Length. `pad`: the length
    /// uses 4 bytes (a non-minimal encoding). MQTT 5 adds the property length byte.
    fn publish_with_remaining(r: usize, v5: bool, pad: bool) -> Vec<u8> {
        let mut p = vec![0x30];
        if pad {
            p.extend([
                (r & 0x7f) as u8 | 0x80,
                ((r >> 7) & 0x7f) as u8 | 0x80,
                ((r >> 14) & 0x7f) as u8 | 0x80,
                ((r >> 21) & 0x7f) as u8,
            ]);
        } else {
            let mut n = r;
            loop {
                let b = (n & 0x7f) as u8;
                n >>= 7;
                p.push(if n > 0 { b | 0x80 } else { b });
                if n == 0 {
                    break;
                }
            }
        }
        // The Remaining Length counts everything after the length bytes.
        let start = p.len();
        p.extend([0x00, 0x01, b't']);
        if v5 {
            p.push(0x00);
        }
        p.resize(start + r, b'x');
        p
    }

    #[test]
    fn the_largest_remaining_length_is_exact_at_the_varint_boundaries() {
        // 1 + varint_len(r) + r <= max_packet, with the largest r. Values by hand.
        for (max_packet, r) in [
            (0, 0),
            (129, 127),
            (130, 127),
            (131, 128),
            (16_386, 16_383),
            (16_387, 16_383),
            (16_388, 16_384),
            (1024, 1021),
            (1_048_576, 1_048_572),
            (2_097_155, 2_097_151),
            (2_097_156, 2_097_151),
            (2_097_157, 2_097_152),
        ] {
            assert_eq!(v3_max_remaining(max_packet), r, "max_packet {max_packet}");
        }
        // The brute force rule agrees around every boundary.
        let varint = |r: u64| match r {
            0..=127 => 1,
            128..=16_383 => 2,
            16_384..=2_097_151 => 3,
            _ => 4,
        };
        for boundary in [127u64, 16_383, 2_097_151] {
            for max in boundary..boundary + 12 {
                let brute = (0..=max)
                    .rev()
                    .find(|&r| 1 + varint(r) + r <= max)
                    .unwrap_or(0);
                assert_eq!(u64::from(v3_max_remaining(max as usize)), brute, "{max}");
            }
        }
    }

    #[test]
    fn a_session_persists_by_version() {
        let mut c = ConnectConfig::from(Profile::default());
        for (protocol, clean, expiry, persists) in [
            (Protocol::V5, true, 0, false),
            (Protocol::V5, true, 60, true),
            (Protocol::V5, false, 0, false),
            (Protocol::V311, true, 0, false),
            (Protocol::V311, true, 60, false),
            (Protocol::V311, false, 0, true),
        ] {
            c.protocol = protocol;
            c.profile.clean_start = clean;
            c.profile.session_expiry_secs = expiry;
            assert_eq!(
                session_persists(&c),
                persists,
                "{protocol:?} clean {clean} expiry {expiry}"
            );
        }
    }

    /// The CONNECT is MQTT 3.1.1 (level 4). The Clean Session flag follows the profile: off means
    /// the broker keeps the session (D3).
    #[tokio::test(flavor = "current_thread")]
    async fn the_v3_connect_packet_has_level_4_and_the_clean_session_flag() {
        for clean in [true, false] {
            let (tx, rx) = std::sync::mpsc::channel();
            let tx = std::sync::Mutex::new(tx);
            let broker = Fake::start(move |_, mut sock| {
                if let Some(body) = accept_v3(&mut sock, 0) {
                    let _ = tx.lock().expect("lock").send(body);
                }
                while read_packet(&mut sock).is_some() {}
            });
            let mut net = Net::start(NetLimits::default());
            let mut p = tcp_profile(broker.port, &[]);
            p.clean_start = clean;
            p.client_id = "cid".into();
            p.keepalive_secs = 30;
            connect_v3_to(&mut net, p).await;
            wait_status(&mut net, |s| s.state == ConnState::Connected).await;
            let body = rx.recv_timeout(Duration::from_secs(2)).expect("CONNECT");
            assert_eq!(&body[..7], [0, 4, b'M', b'Q', b'T', b'T', 4], "level 4");
            assert_eq!(body[7] & 0x02 != 0, clean, "Clean Session flag");
            assert_eq!(&body[8..10], [0, 30], "keep alive");
            net.disconnect();
            net.shutdown().await;
        }
    }

    /// Each version error of plan "Version errors", sent as raw bytes.
    async fn version_error(v3: bool, reply: &'static [u8]) -> ConnState {
        let broker = Fake::start(move |_, mut sock| {
            if read_packet(&mut sock).is_some() {
                let _ = sock.write_all(reply);
            }
            // An empty reply closes the connection.
        });
        let mut net = Net::start(NetLimits::default());
        let p = tcp_profile(broker.port, &[]);
        if v3 {
            connect_v3_to(&mut net, p).await;
        } else {
            net.send(Command::Connect(Box::new(ConnectConfig::from(p))))
                .expect("send");
        }
        let s = wait_status(&mut net, |s| {
            matches!(
                s.state,
                ConnState::Reconnecting { .. } | ConnState::Failed(_)
            )
        })
        .await;
        net.disconnect();
        net.shutdown().await;
        s.state
    }

    /// A 3.1.1-only broker answers an MQTT 5 CONNECT with a 3.1.1 refusal (return code 1).
    #[tokio::test(flavor = "current_thread")]
    async fn a_connack_that_v5_cannot_decode_says_the_reply_is_not_mqtt_5() {
        assert_eq!(
            version_error(false, &[0x20, 0x02, 0x00, 0x01]).await,
            ConnState::Failed(
                "The broker sent a reply that is not MQTT 5. If the broker supports only MQTT 3.1.1, set Protocol to MQTT 3.1.1."
                    .into()
            )
        );
    }

    /// A clean close and a reset before the CONNACK have the same text, on both versions.
    #[tokio::test(flavor = "current_thread")]
    async fn a_close_before_the_connack_does_not_claim_a_version() {
        let text = "The broker closed the connection before it accepted it. Check the address, the TLS settings and the Protocol.";
        for v3 in [false, true] {
            for reset in [false, true] {
                let broker = Fake::start(move |_, mut sock| {
                    if reset {
                        // Read one byte only. The close with unread data sends a RST.
                        let mut one = [0u8; 1];
                        let _ = sock.read_exact(&mut one);
                    } else {
                        let _ = read_packet(&mut sock);
                    }
                });
                let mut net = Net::start(NetLimits::default());
                let p = tcp_profile(broker.port, &[]);
                if v3 {
                    connect_v3_to(&mut net, p).await;
                } else {
                    net.send(Command::Connect(Box::new(ConnectConfig::from(p))))
                        .expect("send");
                }
                let s = wait_status(&mut net, |s| {
                    matches!(
                        s.state,
                        ConnState::Reconnecting { .. } | ConnState::Failed(_)
                    )
                })
                .await;
                assert_eq!(
                    s.state,
                    ConnState::Reconnecting {
                        attempt: 1,
                        reason: text.into()
                    },
                    "v3 {v3} reset {reset}"
                );
                net.disconnect();
                net.shutdown().await;
            }
        }
    }

    fn suback_v3(sock: &mut TcpStream, code: u8) {
        let Some((0x82, body)) = read_packet(sock) else {
            return;
        };
        let _ = sock.write_all(&[0x90, 0x03, body[0], body[1], code]);
    }

    /// SUBACK: granted 0 and 1 are granted QoS; 0x80 is "Refused (0x80)"; granted 2 is an error.
    #[tokio::test(flavor = "current_thread")]
    async fn v3_suback_codes_become_results() {
        for (code, want) in [
            (0u8, Ok(Qos::AtMostOnce)),
            (1, Ok(Qos::AtLeastOnce)),
            (0x80, Err("Refused (0x80)".to_owned())),
            (
                2,
                Err("The broker granted QoS 2. This app supports QoS 0 and 1.".to_owned()),
            ),
        ] {
            let broker = Fake::start(move |_, mut sock| {
                if accept_v3(&mut sock, 0).is_some() {
                    suback_v3(&mut sock, code);
                    while read_packet(&mut sock).is_some() {}
                }
            });
            let mut net = Net::start(NetLimits::default());
            connect_v3_to(&mut net, tcp_profile(broker.port, &["a/b"])).await;
            match next_event(&mut net).await {
                Event::Subscribed { filter, result, .. } => {
                    assert_eq!((filter.as_str(), result), ("a/b", want), "code {code}");
                }
                other => panic!("{other:?}"),
            }
            net.disconnect();
            net.shutdown().await;
        }
    }

    fn v3_publish_cmd(op_id: u64, qos: Qos, payload: Vec<u8>) -> Command {
        Command::Publish(Box::new(Outgoing {
            op_id,
            topic: "t".into(),
            payload: Bytes::from(payload),
            qos,
            retain: false,
            props: MsgProps::default(),
        }))
    }

    async fn published(net: &mut Net, op_id: u64) -> PublishResult {
        loop {
            if let Event::Published {
                op_id: id, result, ..
            } = next_event(net).await
                && id == op_id
            {
                return result;
            }
        }
    }

    /// QoS 0 is Sent, QoS 1 is Acknowledged by the PUBACK. A close with a QoS 1 publish open is
    /// "Lost on disconnect". No PUBACK in 10 s (here 300 ms) is "Timed out".
    #[tokio::test(flavor = "current_thread")]
    async fn v3_publish_results() {
        let broker = Fake::start(|n, mut sock| {
            if accept_v3(&mut sock, 0).is_none() {
                return;
            }
            while let Some((kind, body)) = read_packet(&mut sock) {
                if kind & 0xf0 != 0x30 {
                    continue;
                }
                let payload_start = 2 + 1 + usize::from(kind & 0x06 != 0) * 2;
                match (n, &body[payload_start..]) {
                    (0, b"ack") => {
                        let _ = sock.write_all(&[0x40, 0x02, body[3], body[4]]);
                    }
                    // The connection closes with the publish open.
                    (0, b"lose") => return,
                    _ => {}
                }
            }
        });
        let mut net = Net::start(NetLimits::default());
        connect_v3_to(&mut net, tcp_profile(broker.port, &[])).await;
        wait_status(&mut net, |s| s.state == ConnState::Connected).await;
        net.send(v3_publish_cmd(1, Qos::AtMostOnce, b"zero".to_vec()))
            .expect("send");
        assert_eq!(published(&mut net, 1).await, PublishResult::Sent);
        net.send(v3_publish_cmd(2, Qos::AtLeastOnce, b"ack".to_vec()))
            .expect("send");
        assert_eq!(published(&mut net, 2).await, PublishResult::Acknowledged);
        net.send(v3_publish_cmd(3, Qos::AtLeastOnce, b"lose".to_vec()))
            .expect("send");
        assert_eq!(
            published(&mut net, 3).await,
            PublishResult::LostOnDisconnect
        );
        // The next connection never answers a QoS 1 publish. The status moves to `Reconnecting`
        // after the lost result, so wait for it before the wait for `Connected`.
        wait_status(&mut net, |s| {
            matches!(s.state, ConnState::Reconnecting { .. })
        })
        .await;
        wait_status(&mut net, |s| s.state == ConnState::Connected).await;
        net.send(v3_publish_cmd(4, Qos::AtLeastOnce, b"silent".to_vec()))
            .expect("send");
        assert_eq!(published(&mut net, 4).await, PublishResult::TimedOut);
        net.disconnect();
        net.shutdown().await;
    }

    /// v3: a broker that sends QoS 2 gets no PUBREC. The connection closes and the session fails
    /// with the QoS 2 text, as on MQTT 5.
    #[tokio::test(flavor = "current_thread")]
    async fn v3_qos2_closes_without_pubrec() {
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let broker = Fake::start(move |_, mut sock| {
            if accept_v3(&mut sock, 0).is_none() {
                return;
            }
            // PUBLISH QoS 2, topic "t", packet ID 1, payload "x".
            let _ = sock.write_all(&[0x34, 0x06, 0x00, 0x01, b't', 0x00, 0x01, b'x']);
            // Everything the client sends until it closes.
            let mut kinds = vec![];
            while let Some((kind, _)) = read_packet(&mut sock) {
                kinds.push(kind);
            }
            let _ = tx.lock().expect("lock").send(kinds);
        });
        let mut net = Net::start(NetLimits::default());
        connect_v3_to(&mut net, tcp_profile(broker.port, &[])).await;
        let s = wait_status(&mut net, |s| matches!(s.state, ConnState::Failed(_))).await;
        assert_eq!(s.state, ConnState::Failed(QOS2_TEXT.into()));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(3)).expect("closed"),
            vec![0xe0],
            "no PUBREC (0x50): the only packet is the DISCONNECT of the close"
        );
        assert_eq!(broker.accepts(), 1, "no retry");
        net.shutdown().await;
    }

    /// The panic test of V5, on MQTT 3.1.1. The first call never ends. The second one panics
    /// after an await. ntex runs it in a detached task, so only `contained()` can end the
    /// attempt: it must end with `Failed` in a bounded time, not hang. A new connect works.
    #[tokio::test(flavor = "current_thread")]
    async fn a_v3_handler_panic_after_an_await_fails_the_attempt() {
        let broker = Fake::start(|n, mut sock| {
            if accept_v3(&mut sock, 0).is_none() {
                return;
            }
            if n == 0 {
                for topic in [&b"__handler_hold__"[..], &b"__handler_panic__"[..]] {
                    let mut packet =
                        vec![0x30, (2 + topic.len() + 1) as u8, 0x00, topic.len() as u8];
                    packet.extend_from_slice(topic);
                    packet.push(b'x');
                    let _ = sock.write_all(&packet);
                }
            }
            while read_packet(&mut sock).is_some() {}
        });
        let mut net = Net::start(NetLimits::default());
        connect_v3_to(&mut net, tcp_profile(broker.port, &[])).await;
        let s = wait_status(&mut net, |s| matches!(s.state, ConnState::Failed(_))).await;
        assert_eq!(s.state, ConnState::Failed(PANIC_TEXT.into()));
        assert_eq!(broker.accepts(), 1, "no retry after a panic");
        connect_v3_to(&mut net, tcp_profile(broker.port, &[])).await;
        wait_status(&mut net, |s| s.state == ConnState::Connected).await;
        net.disconnect();
        net.shutdown().await;
    }

    /// An empty client ID with Clean Session off is refused before the dial, with a text about
    /// the client ID (not "the broker sent invalid data"). No connection is made.
    #[tokio::test(flavor = "current_thread")]
    async fn v3_empty_client_id_needs_clean_session() {
        let broker = Fake::start(|_, mut sock| {
            let _ = accept_v3(&mut sock, 0);
        });
        let mut net = Net::start(NetLimits::default());
        let mut p = tcp_profile(broker.port, &[]);
        p.client_id = String::new();
        p.clean_start = false;
        connect_v3_to(&mut net, p).await;
        let s = wait_status(&mut net, |s| matches!(s.state, ConnState::Failed(_))).await;
        assert_eq!(
            s.state,
            ConnState::Failed("MQTT 3.1.1 needs a client ID when Clean session is off. Enter a client ID, or turn on Clean session.".into())
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(broker.accepts(), 0, "nothing was dialed");
        net.shutdown().await;
    }

    /// v3 checks the Remaining Length only. A non-minimal length encoding of the largest allowed
    /// value passes (the packet is 3 bytes over `max_packet`). One more fails.
    #[tokio::test(flavor = "current_thread")]
    async fn v3_non_minimal_length_encoding_is_checked_by_remaining_length() {
        const MAX: usize = 1024;
        let r_max = v3_max_remaining(MAX) as usize;
        for (r, ok) in [(r_max, true), (r_max + 1, false)] {
            let packet = publish_with_remaining(r, false, true);
            let broker = Fake::start(move |_, mut sock| {
                if accept_v3(&mut sock, 0).is_some() {
                    let _ = sock.write_all(&packet);
                    while read_packet(&mut sock).is_some() {}
                }
            });
            let mut net = Net::start(NetLimits {
                max_packet: MAX,
                ..NetLimits::default()
            });
            connect_v3_to(&mut net, tcp_profile(broker.port, &[])).await;
            if ok {
                let m = tokio::time::timeout(Duration::from_secs(5), net.incoming.recv())
                    .await
                    .expect("timeout")
                    .expect("message");
                assert_eq!(m.payload.len(), r - 3);
            } else {
                let s = wait_status(&mut net, |s| matches!(s.state, ConnState::Failed(_))).await;
                assert_eq!(
                    s.state,
                    ConnState::Failed(
                        "The broker sent a packet that is larger than the allowed size.".into()
                    )
                );
            }
            net.disconnect();
            net.shutdown().await;
        }
    }

    // -----------------------------------------------------------------------------------------
    // S2d: subscription options, connection details, server reference.
    // -----------------------------------------------------------------------------------------

    /// A v5 string property: id, 2-byte length, bytes.
    fn prop_str(id: u8, s: &str) -> Vec<u8> {
        let mut p = vec![id];
        p.extend((s.len() as u16).to_be_bytes());
        p.extend(s.as_bytes());
        p
    }

    /// Reads the CONNECT and answers with a v5 CONNACK that has the properties `props`.
    fn accept_v5_with(sock: &mut TcpStream, flags: u8, reason: u8, props: &[u8]) -> bool {
        match read_packet(sock) {
            Some((0x10, _)) => {}
            _ => return false,
        }
        let mut body = vec![flags, reason, props.len() as u8];
        body.extend(props);
        let mut packet = vec![0x20, body.len() as u8];
        packet.extend(body);
        sock.write_all(&packet).is_ok()
    }

    /// The filters of a v5 SUBSCRIBE body with the whole options byte of each.
    fn subscribe_option_bytes(body: &[u8]) -> Vec<(String, u8)> {
        let mut i = 3; // packet ID and the property length (0)
        let mut out = vec![];
        while i + 2 <= body.len() {
            let n = usize::from(u16::from_be_bytes([body[i], body[i + 1]]));
            let end = i + 2 + n;
            out.push((
                String::from_utf8_lossy(&body[i + 2..end]).into_owned(),
                body[end],
            ));
            i = end + 1;
        }
        out
    }

    fn all_options() -> SubOptions {
        SubOptions {
            no_local: true,
            retain_as_published: true,
            retain_handling: RetainHandling::Never,
        }
    }

    /// v3: the SUBSCRIBE has the filter and one QoS byte. No option bit is set, whatever the
    /// profile holds, and the SUBSCRIBE has no property length.
    #[tokio::test(flavor = "current_thread")]
    async fn v3_subscribe_never_sends_options() {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let tx = std::sync::Mutex::new(tx);
        let broker = Fake::start(move |_, mut sock| {
            if accept_v3(&mut sock, 0).is_none() {
                return;
            }
            while let Some((kind, body)) = read_packet(&mut sock) {
                if kind == 0x82 {
                    tx.lock().expect("lock").send(body.clone()).expect("send");
                    let _ = sock.write_all(&[0x90, 0x03, body[0], body[1], 0x00]);
                }
            }
        });
        let mut profile = tcp_profile(broker.port, &["a/b"]);
        profile
            .subscription_options
            .insert("a/b".into(), all_options());
        let mut net = Net::start(NetLimits::default());
        connect_v3_to(&mut net, profile).await;
        let first = rx.recv_timeout(Duration::from_secs(5)).expect("SUBSCRIBE");
        // Packet ID (2 bytes), filter length (2), "a/b", options byte.
        assert_eq!(&first[2..], [0, 3, b'a', b'/', b'b', 0x00]);
        // A shared filter with No Local is sent as a plain filter: v3 has no No Local.
        net.send(Command::Subscribe {
            filter: "$share/g/t".into(),
            qos: Qos::AtLeastOnce,
            options: all_options(),
            req: 0,
        })
        .expect("send");
        let second = rx.recv_timeout(Duration::from_secs(5)).expect("SUBSCRIBE");
        assert_eq!(
            &second[2..],
            [&[0, 10][..], b"$share/g/t", &[0x01]].concat()
        );
        net.disconnect();
        net.shutdown().await;
    }

    /// The subscribe after a reconnect keeps the options: the ones from the profile and the
    /// ones of a live subscribe.
    #[tokio::test(flavor = "current_thread")]
    async fn resubscribe_keeps_the_options() {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<(String, u8)>>();
        let tx = std::sync::Mutex::new(tx);
        let broker = Fake::start(move |n, mut sock| {
            if !accept_connect(&mut sock, 0) {
                return;
            }
            while let Some((kind, body)) = read_packet(&mut sock) {
                if kind != 0x82 {
                    continue;
                }
                let seen = subscribe_option_bytes(&body);
                answer_subscribe(&mut sock, &body);
                tx.lock().expect("lock").send(seen.clone()).expect("send");
                if n == 0 && seen[0].0 == "live/x" {
                    std::thread::sleep(Duration::from_millis(50));
                    return;
                }
            }
        });
        let mut profile = tcp_profile(broker.port, &["base/#"]);
        profile
            .subscription_options
            .insert("base/#".into(), all_options());
        let net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(profile))))
            .expect("send");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).expect("first"),
            [("base/#".to_owned(), 0x2C)]
        );
        net.send(Command::Subscribe {
            filter: "live/x".into(),
            qos: Qos::AtLeastOnce,
            options: SubOptions {
                no_local: true,
                ..SubOptions::default()
            },
            req: 0,
        })
        .expect("send");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).expect("live"),
            [("live/x".to_owned(), 0x05)]
        );
        // After the drop the same two filters come again with the same bytes.
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)).expect("again 1"),
            [("base/#".to_owned(), 0x2C)]
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)).expect("again 2"),
            [("live/x".to_owned(), 0x05)]
        );
        net.disconnect();
        net.shutdown().await;
    }

    /// MQTT 5 forbids No Local on a shared subscription. The net thread refuses it with a
    /// text, and nothing goes to the broker.
    #[tokio::test(flavor = "current_thread")]
    async fn no_local_on_a_shared_subscription_is_refused_locally() {
        let (tx, rx) = std::sync::mpsc::channel::<u8>();
        let tx = std::sync::Mutex::new(tx);
        let broker = Fake::start(move |_, mut sock| {
            if !accept_connect(&mut sock, 0) {
                return;
            }
            while let Some((kind, _)) = read_packet(&mut sock) {
                tx.lock().expect("lock").send(kind).expect("send");
            }
        });
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(broker.port, &[]),
        ))))
        .expect("send");
        wait_status(&mut net, |s| s.state == ConnState::Connected).await;
        net.send(Command::Subscribe {
            filter: "$share/g/t".into(),
            qos: Qos::AtMostOnce,
            options: SubOptions {
                no_local: true,
                ..SubOptions::default()
            },
            req: 0,
        })
        .expect("send");
        match next_event(&mut net).await {
            Event::Subscribed { filter, result, .. } => assert_eq!(
                (filter.as_str(), result),
                (
                    "$share/g/t",
                    Err(
                        "No Local is not allowed on a shared subscription ($share/...).".to_owned()
                    )
                )
            ),
            other => panic!("{other:?}"),
        }
        net.disconnect();
        net.shutdown().await;
        // The broker saw only the DISCONNECT (0xE0).
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).expect("DISCONNECT"),
            0xE0
        );
        assert!(rx.try_recv().is_err(), "no SUBSCRIBE");
    }

    /// A v5 CONNACK with properties gives `ConnectionDetails` with exactly those values. The
    /// Server Keep Alive 0 wins over the 60 s of the profile.
    #[tokio::test(flavor = "current_thread")]
    async fn connection_details_come_from_the_connack() {
        let broker = Fake::start(|_, mut sock| {
            let mut props = vec![0x13, 0x00, 0x00]; // Server Keep Alive 0
            props.extend([0x21, 0x00, 0x0A]); // Receive Maximum 10
            props.extend([0x24, 0x01]); // Maximum QoS 1
            props.extend([0x25, 0x00]); // Retain Available 0
            props.extend([0x27, 0x00, 0x00, 0x10, 0x00]); // Maximum Packet Size 4096
            props.extend([0x22, 0x00, 0x05]); // Topic Alias Maximum 5
            props.extend([0x28, 0x00, 0x29, 0x00, 0x2A, 0x00]); // wildcard, sub IDs, shared: off
            props.extend(prop_str(0x12, "srv-id"));
            props.extend(prop_str(0x1A, "resp/"));
            props.extend(prop_str(0x1C, "alt.example:1883"));
            props.extend(prop_str(0x1F, "hello"));
            if accept_v5_with(&mut sock, 0x01, 0x00, &props) {
                while read_packet(&mut sock).is_some() {}
            }
        });
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(broker.port, &[]),
        ))))
        .expect("send");
        let connected = wait_status(&mut net, |s| s.state == ConnState::Connected).await;
        let generation = connected.generation;
        assert_ne!(connected.attempt, 0);
        match next_event_any(&mut net).await {
            Event::ConnectionDetails {
                generation: g,
                attempt,
                details,
            } => {
                assert_eq!(g, generation);
                assert_eq!(
                    attempt, connected.attempt,
                    "the attempt of the Connected status"
                );
                assert_eq!(
                    *details,
                    ConnectionDetails {
                        protocol: Protocol::V5,
                        session_present: true,
                        v5: Some(V5Details {
                            assigned_client_id: Some("srv-id".into()),
                            keep_alive_secs: 0,
                            server_keep_alive_secs: Some(0),
                            receive_max: 10,
                            max_packet_size: Some(4096),
                            max_qos: 1,
                            retain_available: false,
                            wildcard_subscriptions: false,
                            shared_subscriptions: false,
                            subscription_ids: false,
                            topic_alias_max: 5,
                            server_reference: Some("alt.example:1883".into()),
                            response_info: Some("resp/".into()),
                            reason_string: Some("hello".into()),
                        }),
                    }
                );
            }
            other => panic!("{other:?}"),
        }
        net.disconnect();
        net.shutdown().await;
    }

    /// A refused v5 CONNACK with a server reference: the old text stays, and the reference
    /// follows it (plan, "Server reference", D8).
    #[tokio::test(flavor = "current_thread")]
    async fn a_refused_connack_text_ends_with_the_server_reference() {
        let broker = Fake::start(|_, mut sock| {
            let mut props = prop_str(0x1C, "alt.example:1883");
            props.extend(prop_str(0x1F, "moved"));
            let _ = accept_v5_with(&mut sock, 0x00, 0x9C, &props);
        });
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(broker.port, &[]),
        ))))
        .expect("send");
        let s = wait_status(&mut net, |s| {
            matches!(
                s.state,
                ConnState::Reconnecting { .. } | ConnState::Failed(_)
            )
        })
        .await;
        assert_eq!(
            s.state,
            ConnState::Failed(
                "The broker rejected the connection: use another server (moved). Server reference: alt.example:1883."
                    .into()
            )
        );
        net.disconnect();
        net.shutdown().await;
    }

    /// A broker DISCONNECT with a server reference: the reference follows the old text.
    #[tokio::test(flavor = "current_thread")]
    async fn a_broker_disconnect_text_ends_with_the_server_reference() {
        let broker = Fake::start(|_, mut sock| {
            if !accept_connect(&mut sock, 0) {
                return;
            }
            // DISCONNECT: reason 0x9D (server moved), properties: Server Reference.
            let props = prop_str(0x1C, "alt.example:1883");
            let mut body = vec![0x9D, props.len() as u8];
            body.extend(props);
            let mut packet = vec![0xE0, body.len() as u8];
            packet.extend(body);
            let _ = sock.write_all(&packet);
            while read_packet(&mut sock).is_some() {}
        });
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(broker.port, &[]),
        ))))
        .expect("send");
        let s = wait_status(&mut net, |s| {
            matches!(
                s.state,
                ConnState::Reconnecting { .. } | ConnState::Failed(_)
            )
        })
        .await;
        assert_eq!(
            s.state,
            ConnState::Failed(
                "The broker closed the connection: ServerMoved. Server reference: alt.example:1883."
                    .into()
            )
        );
        net.disconnect();
        net.shutdown().await;
    }

    /// Fills the event log up to its reserve: each invalid filter makes one `Subscribed` error
    /// event, and `send` answers `Full` also after a pause when the log is at its reserve.
    async fn fill_the_log(net: &mut Net) {
        let bad = || Command::Subscribe {
            filter: "a/#/b".into(),
            qos: Qos::AtMostOnce,
            options: Default::default(),
            req: 0,
        };
        loop {
            match net.send(bad()) {
                Ok(()) => {}
                Err(SendError::Full) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    if matches!(net.send(bad()), Err(SendError::Full)) {
                        break;
                    }
                }
                Err(e) => panic!("{e}"),
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// Reads the log until nothing comes for 400 ms. Returns the details and the `Reconnected`
    /// events as text: "details <generation> <receive maximum> <attempt>", "reconnected <gen>".
    async fn drain_details(net: &mut Net) -> Vec<String> {
        let mut order = vec![];
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(400), net.recv_event()).await
        {
            match event {
                Event::ConnectionDetails {
                    generation,
                    attempt,
                    details,
                } => {
                    let receive_max = details.v5.expect("v5").receive_max;
                    order.push(format!("details {generation} {receive_max} {attempt}"));
                }
                Event::Reconnected { generation, .. } => {
                    order.push(format!("reconnected {generation}"));
                }
                // The filler of the log.
                Event::Subscribed { result: Err(_), .. } => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        order
    }

    /// The details of a connection with a nearly full event log (the UI does not read). The
    /// connections have the Receive Maximum 10, 11, 12, 13, 14 (one for each accept).
    /// 1. The connection drops. The details of the new connection wait for room like
    ///    `Reconnected`, and they come after the drain, once for each connect and with the
    ///    generation of the connection. No event is added while the log is full.
    /// 2. The app keeps only the details of the latest connection. A second Connect replaces
    ///    the first one, and its connection drops twice. After the drain the UI has the details
    ///    of the last connection once, with the newest generation, and none of the dropped ones.
    #[tokio::test(flavor = "current_thread")]
    async fn a_full_log_keeps_the_details_of_the_latest_connection_and_loses_none() {
        // The test tells the first connection when to drop (no fixed wait).
        let (drop_tx, drop_rx) = std::sync::mpsc::channel::<()>();
        let drop_rx = std::sync::Mutex::new(drop_rx);
        let broker = Fake::start(move |n, mut sock| {
            let props = [0x21, 0x00, 10 + n as u8];
            if !accept_v5_with(&mut sock, 0x00, 0x00, &props) {
                return;
            }
            match n {
                0 => {
                    let _ = drop_rx
                        .lock()
                        .expect("lock")
                        .recv_timeout(Duration::from_secs(10));
                }
                2 | 3 => std::thread::sleep(Duration::from_millis(300)),
                _ => while read_packet(&mut sock).is_some() {},
            }
        });
        let mut net = Net::start(NetLimits::default());
        let config = || ConnectConfig::from(tcp_profile(broker.port, &[]));

        // 1. One drop.
        let generation = net.connect(config()).expect("connect");
        let first = wait_status(&mut net, |s| s.state == ConnState::Connected)
            .await
            .attempt;
        fill_the_log(&mut net).await;
        let filled = net.events.len();
        drop_tx.send(()).expect("broker");
        wait_status(&mut net, |s| {
            matches!(s.state, ConnState::Reconnecting { .. })
        })
        .await;
        let back = wait_status(&mut net, |s| s.state == ConnState::Connected).await;
        assert_eq!(back.generation, generation);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            net.events.len(),
            filled,
            "no event is added while the log is full"
        );
        // The first connection's details were in the log before the fill (an older attempt).
        // After them: the details of the new attempt, then `Reconnected`.
        assert_eq!(
            drain_details(&mut net).await,
            [
                format!("details {generation} 10 {first}"),
                format!("details {generation} 11 {}", back.attempt),
                format!("reconnected {generation}"),
            ]
        );

        // 2. A second Connect: connection 2 drops, connection 3 drops, connection 4 stays.
        fill_the_log(&mut net).await;
        let g2 = net.connect(config()).expect("connect");
        assert!(g2 > generation);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
        let last = loop {
            let s = net.status.borrow().clone();
            if broker.accepts() == 5 && s.generation == g2 && s.state == ConnState::Connected {
                break s;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no third connection: {s:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        // g2: one set of details, with the values of the last attempt (14) and the attempt of
        // the final Connected status, then `Reconnected`. The dropped attempts (12, 13) left
        // nothing.
        assert_eq!(
            drain_details(&mut net).await,
            [
                format!("details {g2} 14 {}", last.attempt),
                format!("reconnected {g2}"),
            ]
        );
        assert_eq!(net.stats.event_drops.load(Ordering::Relaxed), 0);
        net.disconnect();
        net.shutdown().await;
    }

    /// One CONNACK refusal: the plan texts and the retry rule. The rows are a MQTT 3.1.1 return
    /// code (1 to 5) or a MQTT 5 reason code, sent as raw bytes. Only "server unavailable" is
    /// retried.
    #[tokio::test(flavor = "current_thread")]
    async fn connack_refusals_have_plain_text_and_the_right_retry_rule() {
        /// The extra check of a row.
        enum Then {
            Nothing,
            /// Wait for `BACKOFF_MAX`: no second connection.
            NoRetry,
            /// Attempt 2 comes, with the same generation.
            Retry,
        }
        // (v3, CONNACK bytes, retried, text, the text must be exact, extra check)
        type Row = (bool, &'static [u8], bool, &'static str, bool, Then);
        let rows: [Row; 8] = [
            (
                true,
                &[0x20, 0x02, 0x00, 1],
                false,
                "This broker does not support MQTT 3.1.1. Set Protocol to MQTT 5.",
                true,
                Then::Nothing,
            ),
            (
                true,
                &[0x20, 0x02, 0x00, 2],
                false,
                "The broker rejected the connection: the client ID is not valid.",
                true,
                Then::Nothing,
            ),
            (
                true,
                &[0x20, 0x02, 0x00, 3],
                true,
                "The broker rejected the connection: the server is not available.",
                true,
                Then::Nothing,
            ),
            (
                true,
                &[0x20, 0x02, 0x00, 4],
                false,
                "The broker rejected the connection: the user name or password is wrong.",
                true,
                Then::Nothing,
            ),
            (
                true,
                &[0x20, 0x02, 0x00, 5],
                false,
                "The broker rejected the connection: this client is not authorized.",
                true,
                Then::Nothing,
            ),
            // v5 0x84: the MQTT 5 name of "unsupported protocol version".
            (
                false,
                &[0x20, 0x03, 0x00, 0x84, 0x00],
                false,
                "This broker does not support MQTT 5. Set Protocol to MQTT 3.1.1.",
                true,
                Then::Nothing,
            ),
            (
                false,
                &[0x20, 0x03, 0x00, 0x87, 0x00],
                false,
                "not authorized",
                false,
                Then::NoRetry,
            ),
            (
                false,
                &[0x20, 0x03, 0x00, 0x88, 0x00],
                true,
                "not available",
                false,
                Then::Retry,
            ),
        ];
        for (i, (v3, reply, retry, text, exact, then)) in rows.into_iter().enumerate() {
            let broker = Fake::start(move |_, mut sock| {
                if read_packet(&mut sock).is_some() {
                    let _ = sock.write_all(reply);
                }
            });
            let mut net = Net::start(NetLimits::default());
            let p = tcp_profile(broker.port, &[]);
            if v3 {
                connect_v3_to(&mut net, p).await;
            } else {
                net.send(Command::Connect(Box::new(ConnectConfig::from(p))))
                    .expect("send");
            }
            let s = wait_status(&mut net, |s| {
                matches!(
                    s.state,
                    ConnState::Reconnecting { .. } | ConnState::Failed(_)
                )
            })
            .await;
            let (retried, got) = match &s.state {
                ConnState::Reconnecting { attempt: 1, reason } => (true, reason.as_str()),
                ConnState::Failed(t) => (false, t.as_str()),
                other => panic!("row {i}: unexpected {other:?}"),
            };
            assert_eq!(retried, retry, "row {i}: {s:?}");
            assert!(
                if exact {
                    got == text
                } else {
                    got.contains(text)
                },
                "row {i}: {got:?} is not {text:?}"
            );
            match then {
                Then::Nothing => {}
                Then::NoRetry => {
                    tokio::time::sleep(BACKOFF_MAX).await;
                    assert!(matches!(net.status.borrow().state, ConnState::Failed(_)));
                    assert_eq!(broker.accepts(), 1, "no retry");
                }
                Then::Retry => {
                    let s = wait_status(&mut net, |s| {
                        matches!(s.state, ConnState::Reconnecting { attempt: 2, .. })
                    })
                    .await;
                    assert_eq!(s.generation, 1);
                    assert!(broker.accepts() >= 2);
                }
            }
            net.disconnect();
            net.shutdown().await;
        }
    }
}
