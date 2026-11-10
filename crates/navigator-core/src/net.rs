//! The "net" thread: one tokio `current_thread` runtime that owns the MQTT client.
//!
//! The UI talks to it only through [`Net`]. The net thread never waits for the UI. See the
//! "Threads and data flow" section of the plan, ADR 0002 and ARCHITECTURE.md.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, VecDeque};
use std::future::Future;
use std::num::{NonZeroU16, NonZeroU32};
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
    ConnAckReason, ConnectProperties, DeliveryQoS, DisconnectProperties, DisconnectReason,
    PayloadFormatIndicator, Publish, PublishProperties, QoS as WireQos, RetainOptions,
    SubAckReason, SubscribeProperties, UnsubscribeProperties,
};
use ms_mqtt_client::topic::{TopicFilter, TopicName};
use ms_mqtt_client::transport::{
    ConnectionTransportConfig, ConnectionTransportType, IntoWsRequest as _,
};
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::{JoinSet, LocalSet};

use crate::model::{Incoming, MsgProps, Qos};
use crate::tls::{self, TlsDiag};

pub use crate::profile::{Profile, Transport, Will};

/// One connect: the saved profile plus the secrets. The secrets live in memory only.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectConfig {
    pub profile: Profile,
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
    Subscribe { filter: String, qos: Qos },
    Unsubscribe { filter: String },
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
    /// An error the user must see, for example an invalid filter.
    Error { generation: u64, message: String },
}

/// Counters shared with the UI.
#[derive(Debug, Default)]
pub struct NetStats {
    /// Messages dropped because a queue was full (both the client queue and ours).
    pub drops: AtomicU64,
    /// Events dropped because the event log was full. This stays 0 while the UI drains the log
    /// (see [`EVENTS_CAP`]). It can grow only if the UI stops draining during many reconnects.
    pub event_drops: AtomicU64,
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
///   asks before it sends `Reconnected`. If the log has no room, it waits and tries again
///   (see [`FEED_RETRY`]). So a profile with thousands of filters needs a UI that drains.
const EVENTS_CAP: usize = 2048;
/// Free room in the event log that a producer needs. Work in flight: the command queue (64),
/// the session queue (256), the operations (`MAX_PENDING_OPS`), one in the supervisor and
/// `Reconnected`. The rest is slack.
const EVENTS_RESERVE: usize = 64 + 256 + MAX_PENDING_OPS + 1 + 64;
/// Wait before the re-subscribe asks again for room in the full event log. The timer runs only
/// while the re-subscribe is blocked.
const FEED_RETRY: Duration = Duration::from_millis(50);
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
    /// Becomes true when the net thread has ended.
    done: watch::Receiver<bool>,
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
        let (done_tx, done) = watch::channel(false);

        let events_weak = events_tx.downgrade();
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
            events_tx: events_weak,
            incoming,
            status,
            events,
            stats,
            last_generation: AtomicU64::new(0),
            cancel,
            queued_bytes,
            done,
        }
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
    pub fn send(&self, cmd: Command) -> Result<(), SendError> {
        if let Command::Connect(config) = cmd {
            return self.connect(*config).map(|_| ());
        }
        match self.events_tx.upgrade() {
            Some(tx) if tx.capacity() < EVENTS_RESERVE => return Err(SendError::Full),
            Some(_) => {}
            None => return Err(SendError::Closed),
        }
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
    /// Token of the one attempt that may change the status and queue messages. 0: none. The
    /// supervisor sets it when it starts an attempt, and clears it when it cancels or ends the
    /// attempt. Results of any other attempt are dropped.
    active: Cell<u64>,
}

impl Shared {
    fn set_status(&self, generation: u64, state: ConnState) {
        self.status.send_replace(Status { generation, state });
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
    desired: Rc<RefCell<Vec<(String, Qos)>>>,
    /// The attempt sets this at the CONNACK. From then on, the supervisor sends it every
    /// subscribe and unsubscribe. Before, the attempt takes them from `desired`.
    up: Rc<Cell<bool>>,
    /// Filters that the broker may hold for this session (see [`SessionSubs`]).
    maybe: SessionSubs,
    /// The profile has a session expiry, so the set is reported (see [`report_filters`]).
    report: bool,
    /// The set was reported once for this connection (after its first CONNACK).
    announced: Rc<Cell<bool>>,
}

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
    desired: Rc<RefCell<Vec<(String, Qos)>>>,
    /// The filters of the broker session of this connect. It outlives the connect.
    maybe: SessionSubs,
    /// See [`AttemptCtx::announced`].
    announced: Rc<Cell<bool>>,
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
            desired: Rc::new(RefCell::new(config.profile.subscriptions.clone())),
            maybe: self.session_subs(&config),
            announced: Rc::default(),
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
            report: c.config.profile.session_expiry_secs > 0,
            announced: c.announced.clone(),
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
            Command::Subscribe { filter, qos } => {
                if let Err(e) = TopicFilter::new(&filter) {
                    let why = format!("Invalid topic filter: {e}");
                    reject(
                        &self.shared,
                        generation,
                        Command::Subscribe { filter, qos },
                        &why,
                    );
                    return;
                }
                let mut desired = c.desired.borrow_mut();
                match desired.iter_mut().find(|(f, _)| *f == filter) {
                    Some(entry) => entry.1 = qos,
                    None => desired.push((filter.clone(), qos)),
                }
                drop(desired);

                // Not up yet: the attempt reads `desired` at the CONNACK. After a loss the
                // next attempt does the same.
                if let Some(a) = up {
                    self.forward(a, Command::Subscribe { filter, qos });
                }
            }
            Command::Unsubscribe { filter } => {
                c.desired.borrow_mut().retain(|(f, _)| *f != filter);
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
        Command::Subscribe { filter, .. } => shared.event(Event::Subscribed {
            generation,
            filter,
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
    Driver(DisconnectedEvent),
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
    let profile = &config.profile;
    let generation = ctx.generation;
    local_limits::set_limits(
        shared.limits.max_packet,
        shared.limits.queue_len,
        shared.limits.queue_bytes,
    );
    let (transport, diag) = match transport(config) {
        Ok(t) => t,
        Err(text) => return AttemptEnd::never_up(Outcome::Failed(text)),
    };
    let will = match profile.will.as_ref().map(wire_will) {
        Some(None) => {
            return AttemptEnd::never_up(Outcome::Failed("The Will topic is not valid.".into()));
        }
        Some(Some(w)) => Some(w),
        None => None,
    };
    let (client, connect_handle, mut receiver) = new_client(ClientOptions {
        client_id: Some(profile.client_id.clone()).filter(|id| !id.is_empty()),
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
        profile.clean_start,
        keep_alive,
        will,
        profile.username.clone(),
        config
            .password
            .as_ref()
            .map(|p| Bytes::copy_from_slice(p.as_bytes())),
        ConnectProperties {
            maximum_packet_size: NonZeroU32::new(max_packet).unwrap_or(NonZeroU32::MAX),
            session_expiry_interval: profile.session_expiry_secs.into(),
            ..Default::default()
        },
        Some(CONNECT_TIMEOUT),
    );
    let result = tokio::select! {
        result = connect => result,
        () = stop.notified() => return AttemptEnd::never_up(Outcome::Disconnected),
    };
    let (connection, disconnect_handle, session_present) = match result {
        ConnectResult::Success(connection, connack, handle) => {
            (connection, handle, connack.session_present)
        }
        ConnectResult::Failure(_, error) => {
            return AttemptEnd::never_up(connect_outcome(
                &error,
                diag.as_ref(),
                profile.client_cert.is_some(),
            ));
        }
    };
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
            maybe
                .filters
                .iter()
                .filter(|f| !filters.iter().any(|(d, _)| d == *f))
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
    }

    let mut ops = JoinSet::new();
    // An operation that times out reports it here. The session then ends.
    let expired = Rc::new(Notify::new());
    // Subscribe in batches: `dispatch` rejects more than `MAX_PENDING_OPS` operations.
    let retry = tokio::time::sleep(FEED_RETRY);
    tokio::pin!(retry);
    resub.feed(&client, ctx, shared, &expired, &mut ops, retry.as_mut());

    let mut ingest = Ingest {
        shared: shared.clone(),
        generation,
        token: ctx.token,
        last_drops: Cell::new(local_limits::dropped()),
    };
    let mut driver = Box::pin(connection.run_until_disconnect());
    let mut disconnect_handle = Some(disconnect_handle);
    let (mut receiver_open, mut commands_open) = (true, true);
    // The timer runs only after a change of the session set. It makes one report for many.
    let report = tokio::time::sleep(REPORT_DELAY);
    tokio::pin!(report);
    let mut report_armed = false;
    let ended = loop {
        if !report_armed && ctx.report && ctx.maybe.borrow().dirty {
            report
                .as_mut()
                .reset(tokio::time::Instant::now() + REPORT_DELAY);
            report_armed = true;
        }
        tokio::select! {
            () = &mut report, if report_armed => {
                report_armed = false;
                report_filters(shared, ctx);
            },
            (_handle, event) = &mut driver => break Ended::Driver(event),
            () = expired.notified() => break Ended::Expired,
            message = receiver.recv(), if receiver_open => match message {
                // Dropping the acknowledgement token acknowledges a QoS 1 message.
                Some((publish, _ack)) => ingest.publish(publish),
                None => receiver_open = false,
            },
            command = commands.recv(), if disconnect_handle.is_some() && commands_open => match command {
                Some(command) => {
                    // A newer command for a filter replaces its queued re-subscribe.
                    match &command {
                        Command::Subscribe { filter, .. } | Command::Unsubscribe { filter } => {
                            resub.pending.retain(|(f, _)| f != filter);
                            resub.unsubs.retain(|f| f != filter);
                        }
                        _ => {}
                    }
                    dispatch(&client, command, ctx, shared, &expired, &mut ops);
                }
                None => commands_open = false,
            },
            () = stop.notified(), if disconnect_handle.is_some() => {
                if let Some(handle) = disconnect_handle.take() {
                    let _ = handle.disconnect(&DisconnectProperties::default());
                }
            },
            Some(joined) = ops.join_next(), if !ops.is_empty() => {
                if joined.is_err_and(|e| e.is_panic()) {
                    break Ended::OpPanic;
                }
                resub.feed(&client, ctx, shared, &expired, &mut ops, retry.as_mut());
            },
            () = &mut retry, if resub.blocked => {
                resub.feed(&client, ctx, shared, &expired, &mut ops, retry.as_mut());
            },
        }
    };
    ingest.sync_drops();
    let up_for = Some(up_at.elapsed());
    // Dropping the client side ends the open operations. Let them report "lost".
    let outcome = match ended {
        Ended::Expired => {
            // Drop the connection at once.
            drop((driver, disconnect_handle, receiver, client));
            drain(&mut ops).await;
            return AttemptEnd {
                outcome: Outcome::Failed(OP_TIMEOUT_END_TEXT.into()),
                up_for,
            };
        }
        Ended::OpPanic => {
            tracing::error!(generation, "an operation task panicked");
            drop((driver, disconnect_handle, receiver, client));
            drain(&mut ops).await;
            return AttemptEnd {
                outcome: Outcome::Failed(PANIC_TEXT.into()),
                up_for,
            };
        }
        Ended::Driver(event) => event,
    };
    drop((receiver, client));
    drain(&mut ops).await;
    AttemptEnd {
        outcome: disconnect_outcome(outcome),
        up_for,
    }
}

/// What the session still has to send after the CONNACK: the `Reconnected` event and the
/// subscribe of every desired filter.
struct Resub {
    announce: bool,
    pending: VecDeque<(String, Qos)>,
    /// Filters to remove from a resumed session (see [`SessionSubs`]).
    unsubs: VecDeque<String>,
    /// The last `feed` stopped because the event log has no room. The retry timer runs.
    blocked: bool,
}

impl Resub {
    /// Sends `Reconnected` and starts queued re-subscribes while the batch limit and the room in
    /// the event log allow. Half of the operation slots stay free for commands from the UI.
    /// With no room, it starts the retry timer.
    fn feed(
        &mut self,
        client: &Client,
        ctx: &AttemptCtx,
        shared: &Rc<Shared>,
        expired: &Rc<Notify>,
        ops: &mut JoinSet<()>,
        retry: std::pin::Pin<&mut tokio::time::Sleep>,
    ) {
        let generation = ctx.generation;
        self.blocked = false;
        if self.announce {
            if !shared.has_room() {
                return self.block(retry);
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
                client.clone(),
                filter,
                ctx.maybe.clone(),
                ctx.desired.clone(),
                expired.clone(),
            ));
        }
        while ops.len() < MAX_PENDING_OPS / 2 && !self.pending.is_empty() {
            if !shared.has_room() {
                return self.block(retry);
            }
            if let Some((filter, qos)) = self.pending.pop_front() {
                let subscribe = Command::Subscribe { filter, qos };
                dispatch(client, subscribe, ctx, shared, expired, ops);
            }
        }
    }

    fn block(&mut self, retry: std::pin::Pin<&mut tokio::time::Sleep>) {
        self.blocked = true;
        retry.reset(tokio::time::Instant::now() + FEED_RETRY);
    }
}

async fn drain(ops: &mut JoinSet<()>) {
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, async {
        while ops.join_next().await.is_some() {}
    })
    .await;
}

const CLIENT_CERT_TEXT: &str =
    "The broker needs a client certificate. Add your certificate and key.";
const PROTOCOL_TEXT: &str = "The broker sent data that is not valid MQTT 5.";

/// Turns incoming publishes into `Incoming` and queues them for the UI. Never waits for the UI.
struct Ingest {
    shared: Rc<Shared>,
    generation: u64,
    /// Attempt token. After a cancel or a loss, [`Shared::active`] differs and nothing is queued.
    token: u64,
    /// Value of `local_limits::dropped()` that we already counted.
    last_drops: Cell<u64>,
}

impl Ingest {
    fn publish(&mut self, publish: Publish) {
        self.sync_drops();
        let s = &*self.shared;
        // The attempt was cancelled: queue nothing more for the UI. This is not a drop.
        if s.active.get() != self.token {
            return;
        }
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
    let (client, shared, expired) = (client.clone(), shared.clone(), expired.clone());
    match command {
        Command::Subscribe { filter, qos } => {
            ops.spawn_local(subscribe_op(
                client, filter, qos, generation, maybe, shared, expired,
            ));
        }
        Command::Unsubscribe { filter } => {
            ops.spawn_local(unsubscribe_op(
                client, filter, generation, maybe, desired, shared, expired,
            ));
        }
        Command::Publish(outgoing) => {
            ops.spawn_local(publish_op(client, *outgoing, generation, shared, expired));
        }
        Command::Connect(_) => {}
    }
}

/// The Will of a profile as the MQTT library wants it. `None`: the topic is not valid.
fn wire_will(w: &Will) -> Option<ms_mqtt_client::packet::Will> {
    Some(ms_mqtt_client::packet::Will {
        topic_name: TopicName::try_from(w.topic.as_str()).ok()?,
        qos: wire_qos(w.qos),
        retain: w.retain,
        payload: Bytes::copy_from_slice(&w.payload),
        properties: ms_mqtt_client::packet::WillProperties {
            delay_interval: w.delay_secs,
            payload_format_indicator: PayloadFormatIndicator::Unspecified,
            message_expiry_interval: None,
            content_type: None,
            response_topic: None,
            correlation_data: None,
            user_properties: Vec::new(),
        },
    })
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
    maybe: SessionSubs,
    shared: Rc<Shared>,
    expired: Rc<Notify>,
) {
    let result = tokio::time::timeout(OP_TIMEOUT, async {
        let topic_filter =
            TopicFilter::new(&filter).map_err(|e| format!("Invalid topic filter: {e}"))?;
        // From the moment of the send, the broker may hold the filter.
        maybe.borrow_mut().insert(&filter);
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
    match &result {
        Ok(granted) => tracing::info!(generation, granted_qos = ?granted, "subscribed"),
        Err(why) => tracing::warn!(generation, reason = %why, "subscribe failed"),
    }
    tracing::debug!(generation, %filter, "subscribe result for filter");
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
    maybe: SessionSubs,
    desired: Rc<RefCell<Vec<(String, Qos)>>>,
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
    if result.is_ok() {
        forget(&maybe, &desired, &filter);
    }
    shared.event(match result {
        Ok(()) => Event::Unsubscribed {
            generation,
            filter,
            result: Ok(()),
        },
        Err(message) => Event::Unsubscribed {
            generation,
            filter,
            result: Err(message),
        },
    });
}

/// The broker no longer holds `filter`: drop it from the session set. A filter that is desired
/// again stays (the user subscribed to it after the UNSUBSCRIBE was sent).
fn forget(maybe: &SessionSubs, desired: &Rc<RefCell<Vec<(String, Qos)>>>, filter: &str) {
    if !desired.borrow().iter().any(|(f, _)| f == filter) {
        maybe.borrow_mut().remove(filter);
    }
}

/// Removes a filter that is not desired from a resumed session (see [`SessionSubs`]). It makes
/// no event: the user got the result when he removed the filter. A failure keeps the filter in
/// the set, so the next CONNACK that resumes the session tries again.
async fn stale_unsubscribe_op(
    client: Client,
    filter: String,
    maybe: SessionSubs,
    desired: Rc<RefCell<Vec<(String, Qos)>>>,
    expired: Rc<Notify>,
) {
    let sent = tokio::time::timeout(OP_TIMEOUT, async {
        let topic_filter = TopicFilter::new(&filter).map_err(|_| ())?;
        let token = client
            .unsubscribe(topic_filter, UnsubscribeProperties::default())
            .await
            .map_err(|_| ())?;
        token.await.map_err(|_| ())?.as_result().map_err(|_| ())
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
    client: Client,
    o: Outgoing,
    generation: u64,
    shared: Rc<Shared>,
    expired: Rc<Notify>,
) {
    #[cfg(test)]
    if o.topic == "__panic__" {
        panic!("test: operation task panic");
    }
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

/// The transport for one attempt. The [`TlsDiag`] (TLS only) tells why a handshake failed.
fn transport(c: &ConnectConfig) -> Result<(ConnectionTransportType, Option<TlsDiag>), String> {
    let p = &c.profile;
    let host = p.host.clone();
    match p.transport {
        Transport::Tcp => Ok((
            ConnectionTransportType::Tcp {
                hostname: host,
                port: p.port,
            },
            None,
        )),
        Transport::Tls => {
            let (tls_config, diag) = tls_config(c)?;
            Ok((
                ConnectionTransportType::Tls {
                    hostname: host,
                    port: p.port,
                    tls_config,
                },
                Some(diag),
            ))
        }
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
            let (tls_config, diag) = if secure {
                let (config, diag) = tls_config(c)?;
                (Some(config), Some(diag))
            } else {
                (None, None)
            };
            Ok((
                ConnectionTransportType::Ws {
                    request,
                    tls_config,
                },
                diag,
            ))
        }
    }
}

/// Reads the PEM files here. This runs on the net thread, never on the UI thread.
fn tls_config(
    c: &ConnectConfig,
) -> Result<(ms_mqtt_client::transport::TlsConfig, TlsDiag), String> {
    let p = &c.profile;
    let pair = match (&p.client_cert, &p.client_key) {
        (Some(cert), Some(key)) => Some((cert.as_path(), key.as_path())),
        (None, None) => None,
        _ => return Err("Set both the client certificate and the client key, or neither.".into()),
    };
    tls::build(
        p.ca_file.as_deref(),
        pair,
        c.key_passphrase.as_deref(),
        p.verify_tls,
    )
}

/// Retry or stop after an I/O error. A certificate problem that the handshake recorded always
/// stops. So does every error that we do not know.
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
        K::UnexpectedEof | K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe => {
            Outcome::Retry("The broker closed the connection.".into())
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
            Outcome::Retry("The broker closed the connection.".into())
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

fn connect_outcome(e: &ConnectError, diag: Option<&TlsDiag>, has_client_cert: bool) -> Outcome {
    match e {
        ConnectError::Io(e) => io_outcome(e, diag, has_client_cert),
        ConnectError::Protocol(_) => Outcome::Failed(PROTOCOL_TEXT.into()),
        ConnectError::ResponseTimeout => {
            Outcome::Retry("The broker did not answer the connection request.".into())
        }
        ConnectError::Rejected(ack) => {
            let text = connack_text(ack);
            if connack_is_temporary(&ack.reason) {
                Outcome::Retry(text)
            } else {
                Outcome::Failed(text)
            }
        }
    }
}

/// CONNACK reasons that a retry can fix. Bad credentials, a ban, a wrong protocol version and
/// every unknown reason are not here: they stop the session.
fn connack_is_temporary(reason: &ConnAckReason) -> bool {
    matches!(
        reason,
        ConnAckReason::ServerUnavailable
            | ConnAckReason::ServerBusy
            | ConnAckReason::ConnectionRateExceeded
            | ConnAckReason::QuotaExceeded
    )
}

/// DISCONNECT reasons that a retry can fix.
fn disconnect_is_temporary(reason: &DisconnectReason) -> bool {
    matches!(
        reason,
        DisconnectReason::NormalDisconnection
            | DisconnectReason::ServerBusy
            | DisconnectReason::ServerShuttingDown
            | DisconnectReason::KeepAliveTimeout
            | DisconnectReason::MessageRateTooHigh
            | DisconnectReason::QuotaExceeded
            | DisconnectReason::ConnectionRateExceeded
            | DisconnectReason::MaximumConnectTime
    )
}

fn disconnect_outcome(event: DisconnectedEvent) -> Outcome {
    match event {
        DisconnectedEvent::ApplicationDisconnect => Outcome::Disconnected,
        DisconnectedEvent::ServerDisconnect(d) => {
            let temporary = disconnect_is_temporary(&d.reason);
            let reason = d
                .properties
                .reason_string
                .unwrap_or_else(|| format!("{:?}", d.reason));
            let text = format!("The broker closed the connection: {reason}.");
            if temporary {
                Outcome::Retry(text)
            } else {
                Outcome::Failed(text)
            }
        }
        DisconnectedEvent::IoError(e) => io_outcome(&e, None, false),
        DisconnectedEvent::ProtocolError(_) => Outcome::Failed(PROTOCOL_TEXT.into()),
        DisconnectedEvent::PingTimeout => {
            Outcome::Retry("The broker did not answer the keep-alive ping.".into())
        }
    }
}

fn connack_text(ack: &ms_mqtt_client::packet::ConnAck) -> String {
    let reason = match ack.reason {
        ConnAckReason::BadUserNameOrPassword => "the user name or password is wrong",
        ConnAckReason::NotAuthorized => "this client is not authorized",
        ConnAckReason::ClientIdentifierNotValid => "the client ID is not valid",
        ConnAckReason::ServerUnavailable => "the server is not available",
        ConnAckReason::ServerBusy => "the server is busy",
        ConnAckReason::Banned => "this client is banned",
        ConnAckReason::BadAuthenticationMethod => "the authentication method is not accepted",
        ConnAckReason::UnsupportedProtocolVersion => "the broker does not support MQTT 5",
        ConnAckReason::PacketTooLarge => "the CONNECT packet is too large",
        ConnAckReason::QuotaExceeded => "a quota is exceeded",
        ConnAckReason::UseAnotherServer | ConnAckReason::ServerMoved => "use another server",
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
            active: Cell::new(0),
        });

        let profile = Profile {
            host: "127.0.0.1".into(),
            port,
            transport: Transport::Tcp,
            subscriptions: vec![],
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

    async fn next_event(net: &mut Net) -> Event {
        tokio::time::timeout(Duration::from_secs(5), net.events.recv())
            .await
            .expect("event timeout")
            .expect("events closed")
    }

    /// The broker drops the connection. The net thread reconnects with the same generation. It
    /// subscribes again with the filters of that moment, including one that the user added.
    #[tokio::test(flavor = "current_thread")]
    async fn reconnects_after_drop_and_resubscribes_live_filters() {
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<String>();
        let seen_tx = std::sync::Mutex::new(seen_tx);
        let broker = Fake::start(move |n, mut sock| {
            if !accept_connect(&mut sock, 0) {
                return;
            }
            while let Some((kind, body)) = read_packet(&mut sock) {
                if kind != 0x82 {
                    continue;
                }
                let filters = answer_subscribe(&mut sock, &body);
                if n == 0 && filters == ["live/x"] {
                    // Let the client see the SUBACK, then drop the connection.
                    std::thread::sleep(Duration::from_millis(50));
                    return;
                }
                if n == 1 {
                    for f in filters {
                        seen_tx.lock().expect("lock").send(f).expect("send");
                    }
                }
            }
        });
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(broker.port, &["base/#"]),
        ))))
        .expect("send");
        let first = wait_status(&mut net, |s| s.state == ConnState::Connected).await;
        assert!(matches!(
            next_event(&mut net).await,
            Event::Subscribed { filter, result: Ok(_), .. } if filter == "base/#"
        ));
        net.send(Command::Subscribe {
            filter: "live/x".into(),
            qos: Qos::AtMostOnce,
        })
        .expect("send");
        assert!(matches!(
            next_event(&mut net).await,
            Event::Subscribed { filter, result: Ok(_), .. } if filter == "live/x"
        ));

        let lost = wait_status(&mut net, |s| {
            matches!(s.state, ConnState::Reconnecting { .. })
        })
        .await;
        assert_eq!(lost.generation, first.generation);
        assert!(matches!(
            &lost.state,
            ConnState::Reconnecting { attempt: 1, reason } if reason.contains("closed")
        ));
        let back = wait_status(&mut net, |s| s.state == ConnState::Connected).await;
        assert_eq!(back.generation, first.generation, "same generation");

        match next_event(&mut net).await {
            Event::Reconnected { generation, .. } => assert_eq!(generation, first.generation),
            other => panic!("expected Reconnected, got {other:?}"),
        }
        let mut filters = vec![];
        for _ in 0..2 {
            match next_event(&mut net).await {
                Event::Subscribed {
                    generation,
                    filter,
                    result: Ok(_),
                } => {
                    assert_eq!(generation, first.generation);
                    filters.push(filter);
                }
                other => panic!("expected Subscribed, got {other:?}"),
            }
        }
        filters.sort();
        assert_eq!(filters, ["base/#", "live/x"]);
        let mut seen: Vec<_> = (0..2)
            .map(|_| {
                seen_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("broker saw")
            })
            .collect();
        seen.sort();
        assert_eq!(seen, ["base/#", "live/x"]);
        assert_eq!(broker.accepts(), 2);
        net.shutdown().await;
    }

    /// Subscribe and Unsubscribe while we wait for the retry change the next resubscribe.
    #[tokio::test(flavor = "current_thread")]
    async fn filters_changed_during_backoff_are_used_by_the_next_attempt() {
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<String>();
        let seen_tx = std::sync::Mutex::new(seen_tx);
        let broker = Fake::start(move |n, mut sock| {
            // First connection: refuse (temporary). Second: accept.
            if n == 0 {
                let _ = accept_connect(&mut sock, 0x88);
                return;
            }
            if !accept_connect(&mut sock, 0) {
                return;
            }
            while let Some((kind, body)) = read_packet(&mut sock) {
                if kind == 0x82 {
                    for f in answer_subscribe(&mut sock, &body) {
                        seen_tx.lock().expect("lock").send(f).expect("send");
                    }
                }
            }
        });
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(broker.port, &["a", "b"]),
        ))))
        .expect("send");
        wait_status(&mut net, |s| {
            matches!(s.state, ConnState::Reconnecting { .. })
        })
        .await;
        net.send(Command::Subscribe {
            filter: "c".into(),
            qos: Qos::AtMostOnce,
        })
        .expect("send");
        net.send(Command::Unsubscribe { filter: "a".into() })
            .expect("send");
        wait_status(&mut net, |s| s.state == ConnState::Connected).await;
        let mut seen: Vec<_> = (0..2)
            .map(|_| {
                seen_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("broker saw")
            })
            .collect();
        seen.sort();
        assert_eq!(seen, ["b", "c"]);
        assert!(seen_rx.recv_timeout(Duration::from_millis(200)).is_err());
        net.shutdown().await;
    }

    /// A refused port gives Reconnecting. A disconnect during the wait ends it at once, and no
    /// other attempt follows.
    #[tokio::test(flavor = "current_thread")]
    async fn disconnect_during_backoff_cancels_at_once() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        }; // closed: connections are refused
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(port, &[]),
        ))))
        .expect("send");
        let waiting = wait_status(&mut net, |s| {
            matches!(s.state, ConnState::Reconnecting { .. })
        })
        .await;
        let started = std::time::Instant::now();
        net.disconnect();
        let done = wait_status(&mut net, |s| s.state == ConnState::Disconnected).await;
        assert!(
            started.elapsed() < BACKOFF_BASE,
            "cancel waited for the backoff"
        );
        assert_eq!(done.generation, waiting.generation);
        // The wait is over. Nothing starts again.
        tokio::time::sleep(BACKOFF_MAX / 2).await;
        assert_eq!(net.status.borrow().state, ConnState::Disconnected);
        net.shutdown().await;
    }

    /// A CONNACK "not authorized" is final. No second connection follows.
    #[tokio::test(flavor = "current_thread")]
    async fn not_authorized_fails_without_retry() {
        let broker = Fake::start(|_, mut sock| {
            let _ = accept_connect(&mut sock, 0x87);
        });
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(broker.port, &[]),
        ))))
        .expect("send");
        let s = wait_status(&mut net, |s| matches!(s.state, ConnState::Failed(_))).await;
        assert!(
            matches!(&s.state, ConnState::Failed(t) if t.contains("not authorized")),
            "{s:?}"
        );
        tokio::time::sleep(BACKOFF_MAX).await;
        assert!(matches!(net.status.borrow().state, ConnState::Failed(_)));
        assert_eq!(broker.accepts(), 1, "no retry");
        net.shutdown().await;
    }

    /// A CONNACK "server unavailable" is temporary. The thread retries with growing attempts.
    #[tokio::test(flavor = "current_thread")]
    async fn server_unavailable_is_retried() {
        let broker = Fake::start(|_, mut sock| {
            let _ = accept_connect(&mut sock, 0x88);
        });
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(broker.port, &[]),
        ))))
        .expect("send");
        let s = wait_status(&mut net, |s| {
            matches!(s.state, ConnState::Reconnecting { .. })
        })
        .await;
        assert!(matches!(
            &s.state,
            ConnState::Reconnecting { attempt: 1, reason } if reason.contains("not available")
        ));
        let s = wait_status(&mut net, |s| {
            matches!(s.state, ConnState::Reconnecting { attempt: 2, .. })
        })
        .await;
        assert_eq!(s.generation, 1);
        assert!(broker.accepts() >= 2);
        net.disconnect();
        wait_status(&mut net, |s| s.state == ConnState::Disconnected).await;
        net.shutdown().await;
    }

    /// After the user disconnects, nothing more goes to the UI queue, even if the broker keeps
    /// sending. The queue is bounded, so what it already holds stays for the UI to read.
    #[tokio::test(flavor = "current_thread")]
    async fn nothing_is_queued_after_disconnect() {
        let broker = Fake::start(|_, mut sock| {
            if !accept_connect(&mut sock, 0) {
                return;
            }
            let Some((0x82, body)) = read_packet(&mut sock) else {
                return;
            };
            answer_subscribe(&mut sock, &body);
            // PUBLISH QoS 0, topic "t", no properties, payload "x".
            let publish = [0x30, 0x06, 0x00, 0x01, b't', 0x00, b'x', b'x'];
            while sock.write_all(&publish).is_ok() {
                std::thread::sleep(Duration::from_micros(100));
            }
        });
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(
            tcp_profile(broker.port, &["t"]),
        ))))
        .expect("send");
        let first = tokio::time::timeout(Duration::from_secs(5), net.incoming.recv())
            .await
            .expect("timeout")
            .expect("message");
        net.release(&first);
        net.disconnect();
        // From `Disconnecting` on, the net thread queues nothing. Empty the queue now, then
        // wait for the end. The broker still sends all the time.
        wait_status(&mut net, |s| {
            matches!(s.state, ConnState::Disconnecting | ConnState::Disconnected)
        })
        .await;
        while let Ok(m) = net.incoming.try_recv() {
            net.release(&m);
        }
        wait_status(&mut net, |s| s.state == ConnState::Disconnected).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            net.incoming.try_recv().is_err(),
            "a message came after Disconnected"
        );
        assert_eq!(net.queued_bytes(), 0);
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

    /// A TLS server with an unknown CA: the user gets the plain text, and there is no retry.
    #[tokio::test(flavor = "current_thread")]
    async fn untrusted_certificate_fails_without_retry() {
        use crate::tls::testutil::{cert, key};
        use openssl::ssl::{SslAcceptor, SslMethod};
        let (ca_key, srv_key) = (key(), key());
        let ca = cert("Test CA", &ca_key, None, true, -1, 30);
        let leaf = cert("localhost", &srv_key, Some((&ca, &ca_key)), false, -1, 30);
        let mut acceptor =
            SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).expect("acceptor");
        acceptor.set_certificate(&leaf).expect("cert");
        acceptor.set_private_key(&srv_key).expect("key");
        let acceptor = acceptor.build();
        let broker = Fake::start(move |_, sock| {
            let _ = acceptor.accept(sock);
        });
        let mut net = Net::start(NetLimits::default());
        let profile = Profile {
            host: "localhost".into(),
            port: broker.port,
            transport: Transport::Tls,
            subscriptions: vec![],
            ..Profile::default()
        };
        net.send(Command::Connect(Box::new(ConnectConfig::from(profile))))
            .expect("send");
        let s = wait_status(&mut net, |s| matches!(s.state, ConnState::Failed(_))).await;
        assert!(
            matches!(&s.state, ConnState::Failed(t) if t.contains("not trusted")),
            "{s:?}"
        );
        tokio::time::sleep(BACKOFF_MAX / 2).await;
        assert_eq!(broker.accepts(), 1, "no retry");
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
            limits: NetLimits::default(),
            active: Cell::new(0),
        });
        shared.set_status(1, ConnState::Connecting);
        shared.active.set(2); // attempt 2 is the active one
        assert!(!shared.set_connected(1, 1), "attempt 1 is stale");
        assert_eq!(status_rx.borrow().state, ConnState::Connecting);

        let publish = || Publish {
            payload: Bytes::from_static(b"x"),
            qos: DeliveryQoS::AtMostOnce,
            retain: false,
            topic_name: TopicName::new("t").expect("topic"),
            properties: PublishProperties::default(),
        };
        let mut stale = Ingest {
            shared: shared.clone(),
            generation: 1,
            token: 1,
            last_drops: Cell::new(local_limits::dropped()),
        };
        stale.publish(publish());
        assert!(incoming_rx.try_recv().is_err(), "stale message was queued");
        assert_eq!(shared.queued_bytes.load(Ordering::Acquire), 0);
        assert_eq!(shared.stats.drops.load(Ordering::Relaxed), 0);
        let mut live = Ingest { token: 2, ..stale };
        live.publish(publish());
        assert!(incoming_rx.try_recv().is_ok());

        assert!(shared.set_connected(1, 2));
        assert_eq!(status_rx.borrow().state, ConnState::Connected);
        // A cancel (active = 0) also blocks the attempt.
        shared.set_status(1, ConnState::Connecting);
        shared.active.set(0);
        assert!(!shared.set_connected(1, 2));
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
        use ConnAckReason as C;
        for r in [
            C::ServerUnavailable,
            C::ServerBusy,
            C::ConnectionRateExceeded,
            C::QuotaExceeded,
        ] {
            assert!(connack_is_temporary(&r), "{r:?}");
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
            assert!(!connack_is_temporary(&r), "{r:?}");
        }
        use DisconnectReason as D;
        for r in [
            D::ServerBusy,
            D::ServerShuttingDown,
            D::ConnectionRateExceeded,
            D::MaximumConnectTime,
        ] {
            assert!(disconnect_is_temporary(&r), "{r:?}");
        }
        for r in [
            D::NotAuthorized,
            D::ProtocolError,
            D::SessionTakenOver,
            D::PacketTooLarge,
            D::UnspecifiedError,
        ] {
            assert!(!disconnect_is_temporary(&r), "{r:?}");
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
        for e in [
            Error::other("failed to lookup address information: Name or service not known"),
            Error::new(K::InvalidData, "packet exceeds maximum packet size"),
            Error::other("something new"),
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
}
