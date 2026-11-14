//! The private `Wire` interface (D6 of the ntex plan): one set of operations over an MQTT
//! client version. Today there is the MQTT 5 adapter. The MQTT 3.1.1 adapter adds a second
//! variant of [`Wire`] and a second arm in [`connect`]. The supervisor and the session loop in
//! `net.rs` use only this module, so they do not change.
//!
//! The adapter owns three jobs:
//!
//! - It connects: our dialer, TLS, the WebSocket upgrade, CONNECT and CONNACK, under one deadline.
//! - It runs the connection with `start_with_control`. The protocol service gets the messages of
//!   the broker, the control service gets the reason why the connection stopped. Each callback
//!   runs inside `catch_unwind`: ntex runs extra calls in detached tasks, so a panic there would
//!   skip the join of our attempt task.
//! - It maps ntex errors to our [`Outcome`] and to the texts of [`PublishResult`] and the events.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

use ntex::ws::{WsClient, WsClientConfig, error::WsClientError};
use ntex_bytes::{ByteString, Bytes as NtexBytes};
use ntex_error::Error as NtexError;
use ntex_io::IoConfig;
use ntex_mqtt::control::{Control, Reason};
use ntex_mqtt::error::{DecodeError, MqttClientError, MqttProtocolError, SendPacketError};
use ntex_mqtt::v5::client::{Connect as ConnectV5, MqttConnector, ProtocolMessage};
use ntex_mqtt::v5::codec::{
    ConnectAck, ConnectAckReason, Disconnect, DisconnectReasonCode, LastWill, PublishAckReason,
    RetainHandling, SubscribeAck, SubscribeAckReason, SubscriptionOptions, UnsubscribeAck,
    UnsubscribeAckReason,
};
use ntex_mqtt::v5::{self, MqttSink};
use ntex_mqtt::{MqttServiceConfig, QoS as WireQos};
use ntex_net::connect::{Connect, ConnectError};
use ntex_service::cfg::SharedCfg;
use ntex_service::{Pipeline, fn_service};
use ntex_tls::TlsConfig;
use ntex_util::time::{Millis, Seconds};

use super::dial::{Dialer, Target};
use super::{
    CONNECT_TIMEOUT, ConnectConfig, Ingest, NetLimits, Outcome, Outgoing, PANIC_TEXT,
    PublishResult, Transport, io_outcome,
};
use crate::model::{Incoming, MsgProps, Qos};
use crate::text::display_safe;
use crate::tls::TlsDiag;

/// Time that a graceful close may take before the session forces it.
pub(super) const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// A write that stays blocked this long ends the connection (the attempt reconnects).
const WRITE_TIMEOUT: Seconds = Seconds(10);
/// The handshake timeout of ntex. Our own deadline ([`CONNECT_TIMEOUT`]) is the same length.
const HANDSHAKE_TIMEOUT: Seconds = Seconds(10);
/// Frame read rate: the timeout of one check, the limit for a whole packet, the least bytes for
/// each check. A started packet that stops cannot hold the connection.
const FRAME_RATE: (Seconds, Seconds, u32) = (Seconds(10), Seconds(60), 256);
/// Incoming topic aliases that we accept. This is the limit of the ntex client dispatcher.
const TOPIC_ALIAS_MAX: u16 = 16;
/// Receive Maximum that we announce in CONNECT, and the limit of in-flight QoS 1 messages.
const RECEIVE_MAX: u16 = 16;

pub(super) const QOS2_TEXT: &str =
    "The broker sent a QoS 2 message. This app supports QoS 0 and 1.";
/// A close that has no cause: ntex reports a keep-alive death, a broker close and a lost
/// network the same way (a plain close). So the text names all of them.
pub(super) const CONNECTION_CLOSED_TEXT: &str =
    "The connection closed (the broker did not answer, or the network failed).";
const PROTOCOL_TEXT: &str = "The broker sent data that is not valid MQTT 5.";
const DUPLICATE_ID_TEXT: &str =
    "The broker broke the MQTT protocol: it reused a packet ID that was still in use.";
const TOO_LARGE_TEXT: &str = "The broker sent a packet that is larger than the allowed size.";
const WRITE_TIMEOUT_TEXT: &str = "The broker did not read our data for 10 s.";
const READ_TIMEOUT_TEXT: &str = "The broker stopped in the middle of a packet.";
const CLOSED_TEXT: &str = "The broker closed the connection.";

/// The operations of a connected client. Cheap to clone. Every operation can wait for the
/// network, so the caller sets the time limit.
#[derive(Clone)]
pub(super) enum Wire {
    V5(MqttSink),
}

/// Why an UNSUBSCRIBE did not finish.
pub(super) enum UnsubFail {
    /// The request did not reach the broker, or its answer was lost. The text is a cause.
    Lost(String),
    /// The broker answered with an error. The text is complete.
    Rejected(String),
}

impl Wire {
    /// Sends DISCONNECT and closes the connection when the data is out. Open operations end
    /// with "lost" at once. The connection can need up to one second to shut down.
    pub(super) fn close(&self) {
        match self {
            Self::V5(s) => s.close(),
        }
    }

    /// Closes at once. Blocked writes and open operations end within milliseconds.
    pub(super) fn force_close(&self) {
        match self {
            Self::V5(s) => s.force_close(),
        }
    }

    pub(super) async fn publish(&self, o: &Outgoing) -> PublishResult {
        // `Net::send` checked the message already. This is the guard for a direct caller.
        if let Err(why) = crate::publish::validate_outgoing(o, None) {
            return PublishResult::Rejected(why);
        }
        // The topic rules of the old client: no control or unassigned code points.
        if let Some(e) = crate::topic::topic_error(&o.topic) {
            return PublishResult::Rejected(format!("Invalid topic: {e}"));
        }
        if let Some(e) = o
            .props
            .response_topic
            .as_deref()
            .and_then(crate::topic::topic_error)
        {
            return PublishResult::Rejected(format!("Invalid response topic: {e}"));
        }
        match self {
            Self::V5(s) => publish_v5(s, o).await,
        }
    }

    pub(super) async fn subscribe(&self, filter: &str, qos: Qos) -> Result<Qos, String> {
        match self {
            Self::V5(s) => subscribe_v5(s, filter, qos).await,
        }
    }

    pub(super) async fn unsubscribe(&self, filter: &str) -> Result<(), UnsubFail> {
        match self {
            Self::V5(s) => unsubscribe_v5(s, filter).await,
        }
    }
}

fn wire_qos(qos: Qos) -> WireQos {
    match qos {
        Qos::AtMostOnce => WireQos::AtMostOnce,
        Qos::AtLeastOnce => WireQos::AtLeastOnce,
    }
}

async fn publish_v5(sink: &MqttSink, o: &Outgoing) -> PublishResult {
    let p = &o.props;
    let builder = sink
        .publish(ByteString::from(o.topic.clone()))
        .retain(o.retain)
        .properties(|pp| {
            pp.content_type = p.content_type.clone().map(ByteString::from);
            pp.response_topic = p.response_topic.clone().map(ByteString::from);
            pp.correlation_data = p
                .correlation_data
                .as_ref()
                .map(|c| NtexBytes::copy_from_slice(c));
            pp.message_expiry_interval = p.message_expiry;
            pp.user_properties = p
                .user
                .iter()
                .map(|(k, v)| (ByteString::from(k.clone()), ByteString::from(v.clone())))
                .collect();
            pp.is_utf8_payload = p.payload_utf8;
        });
    let payload = NtexBytes::copy_from_slice(&o.payload);
    match o.qos {
        Qos::AtMostOnce => match builder.send_at_most_once(payload).await {
            Ok(()) => PublishResult::Sent,
            Err(e) => send_failure(e),
        },
        Qos::AtLeastOnce => match builder.send_at_least_once(payload).await {
            Ok(ack) => match ack.reason_code {
                // "No matching subscribers" is a success for the publisher.
                PublishAckReason::Success | PublishAckReason::NoMatchingSubscribers => {
                    PublishResult::Acknowledged
                }
                reason => PublishResult::Rejected(match ack.reason_string {
                    Some(text) => format!("{reason:?}: {}", display_safe(&text)),
                    None => format!("{reason:?}"),
                }),
            },
            Err(e) => send_failure(e),
        },
    }
}

/// A send that failed: the connection is gone ("lost"), or the packet cannot be encoded.
fn send_failure(e: SendPacketError) -> PublishResult {
    match e {
        SendPacketError::Encode(_) => {
            PublishResult::Rejected("The broker does not accept a message of this size.".into())
        }
        _ => PublishResult::LostOnDisconnect,
    }
}

async fn subscribe_v5(sink: &MqttSink, filter: &str, qos: Qos) -> Result<Qos, String> {
    let options = SubscriptionOptions {
        qos: wire_qos(qos),
        no_local: false,
        retain_as_published: false,
        retain_handling: RetainHandling::AtSubscribe,
    };
    let ack = sink
        .subscribe(None)
        .topic_filter(ByteString::from(filter.to_owned()), options)
        .send()
        .await
        .map_err(|e| match e {
            SendPacketError::Disconnected => "Not connected.".to_owned(),
            other => other.to_string(),
        })?;
    subscribe_result(&ack)
}

/// The result of one SUBSCRIBE from its SUBACK. A refusal keeps the reason text and adds the
/// broker's reason string (it can hold direction controls, so it goes through `display_safe`).
pub(super) fn subscribe_result(ack: &SubscribeAck) -> Result<Qos, String> {
    match ack.status.first() {
        Some(SubscribeAckReason::GrantedQos0) => Ok(Qos::AtMostOnce),
        Some(SubscribeAckReason::GrantedQos1) => Ok(Qos::AtLeastOnce),
        Some(SubscribeAckReason::GrantedQos2) => {
            Err("The broker granted QoS 2. This app supports QoS 0 and 1.".into())
        }
        Some(other) => Err(match &ack.reason_string {
            Some(why) => format!(
                "{} Broker reason: {}",
                suback_text(*other),
                display_safe(why)
            ),
            None => suback_text(*other),
        }),
        None => Err("The broker sent an empty SUBACK.".to_owned()),
    }
}

async fn unsubscribe_v5(sink: &MqttSink, filter: &str) -> Result<(), UnsubFail> {
    let ack = sink
        .unsubscribe()
        .topic_filter(ByteString::from(filter.to_owned()))
        .send()
        .await
        .map_err(|e| {
            UnsubFail::Lost(match e {
                SendPacketError::Disconnected => "Not connected.".to_owned(),
                other => other.to_string(),
            })
        })?;
    unsubscribe_result(&ack)
}

/// The result of one UNSUBSCRIBE from its UNSUBACK. A refusal keeps the reason name and adds the
/// broker's reason string.
pub(super) fn unsubscribe_result(ack: &UnsubscribeAck) -> Result<(), UnsubFail> {
    match ack.status.first() {
        // "No subscription existed" also means: the broker does not hold the filter.
        Some(UnsubscribeAckReason::Success | UnsubscribeAckReason::NoSubscriptionExisted) => Ok(()),
        Some(other) => Err(UnsubFail::Rejected(match &ack.reason_string {
            Some(why) => format!("Unsubscribe rejected: {other:?}: {}", display_safe(why)),
            None => format!("Unsubscribe rejected: {other:?}"),
        })),
        None => Err(UnsubFail::Lost("The broker sent an empty UNSUBACK.".into())),
    }
}

pub(super) fn suback_text(r: SubscribeAckReason) -> String {
    match r {
        SubscribeAckReason::NotAuthorized => "Not authorized to subscribe.".into(),
        SubscribeAckReason::TopicFilterInvalid => {
            "The broker says the topic filter is not valid.".into()
        }
        SubscribeAckReason::QuotaExceeded => "The subscription quota is exceeded.".into(),
        SubscribeAckReason::SharedSubscriptionNotSupported => {
            "The broker does not support shared subscriptions.".into()
        }
        SubscribeAckReason::WildcardSubscriptionsNotSupported => {
            "The broker does not support wildcard subscriptions.".into()
        }
        other => format!("The broker refused the subscription: {other:?}."),
    }
}

// ---------------------------------------------------------------------------------------------
// Connect
// ---------------------------------------------------------------------------------------------

/// A connection that is up: the operations, the facts of the CONNACK and the future that runs
/// the connection. Poll `driver` until it ends; it gives the outcome.
pub(super) struct Connected {
    pub wire: Wire,
    pub session_present: bool,
    /// The Maximum Packet Size that the broker announced. `None`: no limit announced.
    pub broker_max_packet: Option<u32>,
    pub driver: Pin<Box<dyn Future<Output = Outcome>>>,
}

/// The shared configuration of one connection: ntex reads each limit from here, so the limits
/// are per connection and nothing is global.
fn shared_cfg(max_packet: usize) -> SharedCfg {
    let ws = WsClientConfig::new()
        .set_max_frame_size(max_packet)
        .set_max_message_size(max_packet)
        .set_protocols(["mqtt"])
        .expect("the constant protocol name is valid")
        .set_close_timeout(Millis(2000));
    let mqtt = MqttServiceConfig::new()
        .set_handshake_timeout(HANDSHAKE_TIMEOUT)
        .set_max_receive(RECEIVE_MAX)
        .set_max_send(RECEIVE_MAX);
    let (check, cap, least) = FRAME_RATE;
    SharedCfg::new("navigator")
        .add(
            IoConfig::new()
                .set_connect_timeout(Millis(10_000))
                .set_write_timeout(WRITE_TIMEOUT)
                .set_frame_read_rate(check, cap, least),
        )
        .add(mqtt)
        .add(TlsConfig::new())
        .add(ws)
        .build()
}

/// What `connect` needs besides the profile.
pub(super) struct Params<'a> {
    pub config: &'a ConnectConfig,
    pub limits: NetLimits,
    /// The OpenSSL connector for TLS and WSS.
    pub ssl: Option<openssl::ssl::SslConnector>,
    pub diag: Option<&'a TlsDiag>,
    pub ingest: Rc<Ingest>,
}

/// Connects under one deadline. The error is the outcome of the attempt.
pub(super) async fn connect(p: Params<'_>) -> Result<Connected, Outcome> {
    let up = Rc::new(Cell::new(false));
    match tokio::time::timeout(CONNECT_TIMEOUT, connect_v5(&p, up.clone())).await {
        Ok(result) => result,
        // After the transport is up, a missing CONNACK is the broker's fault.
        Err(_) if up.get() => Err(Outcome::Retry(
            "The broker did not answer the connection request.".into(),
        )),
        Err(_) => Err(Outcome::Retry("The connection timed out.".into())),
    }
}

/// The WebSocket address of a profile: the host stays the profile host (HTTP authority, SNI and
/// verification use it).
fn ws_uri(c: &ConnectConfig) -> String {
    let p = &c.profile;
    let path = if p.ws_path.starts_with('/') {
        p.ws_path.clone()
    } else {
        format!("/{}", p.ws_path)
    };
    let host = if p.host.contains(':') && !p.host.starts_with('[') {
        format!("[{}]", p.host)
    } else {
        p.host.clone()
    };
    format!(
        "{}://{host}:{}{path}",
        if p.transport == Transport::Wss {
            "wss"
        } else {
            "ws"
        },
        p.port
    )
}

/// The error of a failed WebSocket upgrade as an I/O error, so `io_outcome` sorts it. The
/// upgrade timeout becomes `Other` with the text "Timeout".
fn ws_error(e: &NtexError<WsClientError>) -> NtexError<ConnectError> {
    let io = match &**e {
        WsClientError::Connect(ConnectError::Io(io)) => {
            std::io::Error::new(io.kind(), io.to_string())
        }
        WsClientError::Disconnected(Some(io)) => std::io::Error::new(io.kind(), io.to_string()),
        // The peer closed before it answered the upgrade: a temporary close.
        WsClientError::Disconnected(None) => std::io::ErrorKind::UnexpectedEof.into(),
        WsClientError::Timeout => std::io::Error::other("Timeout"),
        other => std::io::Error::other(other.to_string()),
    };
    NtexError::from(ConnectError::Io(io))
}

async fn connect_v5(p: &Params<'_>, up: Rc<Cell<bool>>) -> Result<Connected, Outcome> {
    let config = p.config;
    let profile = &config.profile;
    let cfg = shared_cfg(p.limits.max_packet);
    let tls = matches!(profile.transport, Transport::Tls | Transport::Wss);
    let dialer = Dialer::new(if tls { p.ssl.clone() } else { None }, up);
    let target = Target {
        host: profile.host.clone(),
        port: profile.port,
    };
    let will = match profile.will.as_ref().map(last_will) {
        Some(None) => return Err(Outcome::Failed("The Will topic is not valid.".into())),
        Some(Some(w)) => Some(w),
        None => None,
    };
    let max_packet = u32::try_from(p.limits.max_packet).unwrap_or(u32::MAX);
    let mut req = ConnectV5::new(target)
        .client_id(profile.client_id.clone())
        .keep_alive(Seconds(profile.keepalive_secs))
        .max_packet_size(max_packet)
        .max_receive(RECEIVE_MAX)
        .packet(|c| {
            c.clean_start = profile.clean_start;
            c.session_expiry_interval_secs = profile.session_expiry_secs;
            c.topic_alias_max = TOPIC_ALIAS_MAX;
        });
    if let Some(w) = will {
        req = req.last_will(w);
    }
    if let Some(u) = &profile.username {
        req = req.username(ByteString::from(u.clone()));
    }
    if let Some(pw) = &config.password {
        req = req.password(NtexBytes::copy_from_slice(pw.as_bytes()));
    }

    let result = if matches!(profile.transport, Transport::Ws | Transport::Wss) {
        // Our dialer gives the socket (and TLS for WSS), then the upgrade runs on it.
        let client = Rc::new(WsClient::new(ws_uri(config), cfg.clone()).connector(dialer.clone()));
        let connector = move |_: Connect<Target>| {
            let (client, dialer) = (client.clone(), dialer.clone());
            async move {
                let conn = client.connect().await.map_err(|e| ws_error(&e))?;
                dialer.mark_up();
                Ok::<_, NtexError<ConnectError>>(conn.into_transport().seal())
            }
        };
        Pipeline::new(cfg, MqttConnector::new().connector(connector))
            .call(req)
            .await
    } else {
        Pipeline::new(cfg, MqttConnector::new().connector(dialer))
            .call(req)
            .await
    };
    let client = result.map_err(|e| connect_outcome(&e, p.diag, profile.client_cert.is_some()))?;

    let ack = client.packet();
    let (session_present, broker_max_packet) = (ack.session_present, ack.max_packet_size);
    let sink = client.sink();
    let flags = Rc::new(Flags::default());
    let handler = Handler {
        ingest: p.ingest.clone(),
        sink: sink.clone(),
        flags: flags.clone(),
        max_packet: p.limits.max_packet,
    };
    let driver = Box::pin(async move {
        let proto = fn_service(move |msg: ProtocolMessage| handler.clone().call(msg));
        let control = {
            let flags = flags.clone();
            fn_service(move |m: Control<HandlerError>| {
                if let Control::Stop(reason) = m {
                    *flags.stop.borrow_mut() = Some(reason);
                }
                async { Ok::<_, ()>(None) }
            })
        };
        if let Err(e) = client.start_with_control(proto, control).await {
            tracing::debug!(error = %e, "the MQTT dispatcher ended with an error");
        }
        stop_outcome(&flags)
    });
    Ok(Connected {
        wire: Wire::V5(sink),
        session_present,
        broker_max_packet,
        driver,
    })
}

/// The Will of a profile as ntex wants it. `None`: the topic is not valid.
fn last_will(w: &crate::profile::Will) -> Option<LastWill> {
    let t = &w.topic;
    if crate::topic::topic_error(t).is_some() {
        return None;
    }
    Some(LastWill {
        qos: wire_qos(w.qos),
        retain: w.retain,
        topic: ByteString::from(t.clone()),
        message: NtexBytes::copy_from_slice(&w.payload),
        will_delay_interval_sec: Some(w.delay_secs).filter(|&d| d > 0),
        correlation_data: None,
        message_expiry_interval: None,
        content_type: None,
        user_properties: Vec::new(),
        is_utf8_payload: None,
        response_topic: None,
    })
}

// ---------------------------------------------------------------------------------------------
// Handler and control
// ---------------------------------------------------------------------------------------------

/// Why the protocol service stopped the connection.
#[derive(Debug)]
enum HandlerError {
    /// The broker sent QoS 2.
    Qos2,
    /// The handler panicked.
    Panic,
    /// A payload could not be read (the connection is closing) or is over the limit.
    Payload,
}

/// What the callbacks learned. `stop_outcome` reads it when the connection has ended.
#[derive(Default)]
struct Flags {
    qos2: Cell<bool>,
    panicked: Cell<bool>,
    /// The DISCONNECT of the broker: reason and reason string. Kept before the close completes.
    broker_disconnect: RefCell<Option<(DisconnectReasonCode, Option<String>)>>,
    /// The stop reason that the control service got.
    stop: RefCell<Option<Reason<HandlerError>>>,
}

#[derive(Clone)]
struct Handler {
    ingest: Rc<Ingest>,
    sink: MqttSink,
    flags: Rc<Flags>,
    max_packet: usize,
}

impl Handler {
    async fn call(
        self,
        msg: ProtocolMessage,
    ) -> Result<v5::client::ProtocolMessageAck, HandlerError> {
        let sink = self.sink.clone();
        let flags = self.flags.clone();
        match Contain::new(self.handle(msg)).await {
            Ok(result) => result,
            Err(text) => {
                // A panic in a call that ntex runs in a detached task must not hang the attempt.
                tracing::error!(panic = %text, "an MQTT handler panicked");
                flags.panicked.set(true);
                sink.force_close();
                Err(HandlerError::Panic)
            }
        }
    }

    async fn handle(
        self,
        msg: ProtocolMessage,
    ) -> Result<v5::client::ProtocolMessageAck, HandlerError> {
        match msg {
            ProtocolMessage::Publish(p) => {
                #[cfg(test)]
                if p.packet().topic == "__handler_hold__" {
                    // A call that never ends, so that the panic below runs as a second call.
                    std::future::pending::<()>().await;
                }
                #[cfg(test)]
                if p.packet().topic == "__handler_panic__" {
                    ntex_util::time::sleep(Millis(10)).await;
                    panic!("test: handler panic after an await");
                }
                if p.packet().qos == WireQos::ExactlyOnce {
                    // `ProtocolMessage::disconnect` loses its packet: the dispatcher closes the
                    // I/O before it writes it. `close_with_reason` writes the DISCONNECT first.
                    self.flags.qos2.set(true);
                    self.sink
                        .close_with_reason(Disconnect::new(DisconnectReasonCode::QosNotSupported));
                    return Err(HandlerError::Qos2);
                }
                // The codec checked the declared size already. This is the second line.
                if p.packet_size() as usize > self.max_packet {
                    self.sink.force_close();
                    return Err(HandlerError::Payload);
                }
                let payload = p.read_all().await.map_err(|_| HandlerError::Payload)?;
                let qos1 = p.packet().qos == WireQos::AtLeastOnce;
                self.ingest
                    .queue(|generation| incoming(generation, p.packet(), &payload));
                Ok(if qos1 {
                    p.ack(PublishAckReason::Success)
                } else {
                    p.ack_qos0()
                })
            }
            ProtocolMessage::Disconnect(d) => {
                let pkt = d.packet();
                *self.flags.broker_disconnect.borrow_mut() = Some((
                    pkt.reason_code,
                    pkt.reason_string.as_ref().map(|s| s.to_string()),
                ));
                Ok(d.ack())
            }
            other => Ok(other.ack()),
        }
    }
}

fn incoming(generation: u64, p: &v5::codec::Publish, payload: &NtexBytes) -> Incoming {
    let props = &p.properties;
    Incoming {
        generation,
        topic: std::sync::Arc::from(p.topic.as_str()),
        payload: bytes::Bytes::copy_from_slice(payload),
        // We subscribe with maximum QoS 1. QoS 2 never gets here.
        qos: if p.qos == WireQos::AtMostOnce {
            Qos::AtMostOnce
        } else {
            Qos::AtLeastOnce
        },
        retain: p.retain,
        props: MsgProps {
            content_type: props.content_type.as_ref().map(|s| s.to_string()),
            response_topic: props.response_topic.as_ref().map(|s| s.to_string()),
            correlation_data: props
                .correlation_data
                .as_ref()
                .map(|c| bytes::Bytes::copy_from_slice(c)),
            message_expiry: props.message_expiry_interval,
            user: props
                .user_properties
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            payload_utf8: props.is_utf8_payload,
        },
        received_at: std::time::SystemTime::now(),
    }
}

/// `catch_unwind` for a future: a panic after an await becomes an `Err` with its text. The
/// future is boxed, so this is `Unpin`.
struct Contain<T>(Pin<Box<dyn Future<Output = T>>>);

impl<T> Contain<T> {
    fn new(f: impl Future<Output = T> + 'static) -> Self {
        Self(Box::pin(f))
    }
}

impl<T> Future for Contain<T> {
    type Output = Result<T, String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| this.0.as_mut().poll(cx))) {
            Ok(p) => p.map(Ok),
            Err(p) => Poll::Ready(Err(p
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".into()))),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Error text
// ---------------------------------------------------------------------------------------------

fn connect_outcome(
    e: &NtexError<MqttClientError<Box<ConnectAck>>>,
    diag: Option<&TlsDiag>,
    has_client_cert: bool,
) -> Outcome {
    match &**e {
        MqttClientError::Connect(ConnectError::Io(io))
        | MqttClientError::Disconnected(Some(io)) => io_outcome(io, diag, has_client_cert),
        MqttClientError::Connect(other) => Outcome::Failed(format!("Network error: {other}")),
        MqttClientError::Disconnected(None) => Outcome::Retry(CLOSED_TEXT.into()),
        MqttClientError::Protocol(e) => protocol_outcome(e),
        MqttClientError::ConnectTimeout => {
            Outcome::Retry("The broker did not answer the connection request.".into())
        }
        MqttClientError::Ack(ack) => {
            let text = connack_text(ack);
            if connack_is_temporary(ack.reason_code) {
                Outcome::Retry(text)
            } else {
                Outcome::Failed(text)
            }
        }
    }
}

/// The outcome of a protocol error of the connection or of the handshake.
pub(super) fn protocol_outcome(e: &MqttProtocolError) -> Outcome {
    match e {
        MqttProtocolError::KeepAliveTimeout => Outcome::Retry(CONNECTION_CLOSED_TEXT.into()),
        MqttProtocolError::WriteTimeout => Outcome::Retry(WRITE_TIMEOUT_TEXT.into()),
        MqttProtocolError::ReadTimeout => Outcome::Retry(READ_TIMEOUT_TEXT.into()),
        MqttProtocolError::Decode(DecodeError::MaxSizeExceeded { .. }) => {
            Outcome::Failed(TOO_LARGE_TEXT.into())
        }
        MqttProtocolError::ProtocolViolation(v)
            if v.message() == ntex_mqtt::error::SpecViolation::PacketId_2_2_1_3_Pub.to_string() =>
        {
            Outcome::Failed(DUPLICATE_ID_TEXT.into())
        }
        _ => Outcome::Failed(PROTOCOL_TEXT.into()),
    }
}

/// How the connection ended, from what the callbacks saw.
fn stop_outcome(f: &Flags) -> Outcome {
    if f.qos2.get() {
        return Outcome::Failed(QOS2_TEXT.into());
    }
    if f.panicked.get() {
        return Outcome::Failed(PANIC_TEXT.into());
    }
    if let Some((reason, text)) = f.broker_disconnect.take() {
        return disconnect_outcome(reason, text.as_deref());
    }
    match f.stop.take() {
        Some(Reason::Protocol(e)) => protocol_outcome(e.get_ref()),
        Some(Reason::PeerGone(gone)) => match gone.err() {
            Some(io) => io_outcome(io, None, false),
            None => Outcome::Retry(CONNECTION_CLOSED_TEXT.into()),
        },
        Some(Reason::Error(e)) => match e.get_ref() {
            HandlerError::Qos2 => Outcome::Failed(QOS2_TEXT.into()),
            HandlerError::Panic => Outcome::Failed(PANIC_TEXT.into()),
            HandlerError::Payload => Outcome::Retry(CONNECTION_CLOSED_TEXT.into()),
        },
        None => Outcome::Retry(CONNECTION_CLOSED_TEXT.into()),
    }
}

/// CONNACK reasons that a retry can fix. Bad credentials, a ban, a wrong protocol version and
/// every unknown reason are not here: they stop the session.
pub(super) fn connack_is_temporary(reason: ConnectAckReason) -> bool {
    matches!(
        reason,
        ConnectAckReason::ServerUnavailable
            | ConnectAckReason::ServerBusy
            | ConnectAckReason::ConnectionRateExceeded
            | ConnectAckReason::QuotaExceeded
    )
}

/// DISCONNECT reasons that a retry can fix.
pub(super) fn disconnect_is_temporary(reason: DisconnectReasonCode) -> bool {
    matches!(
        reason,
        DisconnectReasonCode::NormalDisconnection
            | DisconnectReasonCode::ServerBusy
            | DisconnectReasonCode::ServerShuttingDown
            | DisconnectReasonCode::KeepAliveTimeout
            | DisconnectReasonCode::MessageRateTooHigh
            | DisconnectReasonCode::QuotaExceeded
            | DisconnectReasonCode::ConnectionRateExceeded
            | DisconnectReasonCode::MaximumConnectTime
    )
}

pub(super) fn disconnect_outcome(reason: DisconnectReasonCode, text: Option<&str>) -> Outcome {
    let shown = text.map_or_else(|| format!("{reason:?}"), |s| display_safe(s).into_owned());
    let text = format!("The broker closed the connection: {shown}.");
    if disconnect_is_temporary(reason) {
        Outcome::Retry(text)
    } else {
        Outcome::Failed(text)
    }
}

fn connack_text(ack: &ConnectAck) -> String {
    let reason = match ack.reason_code {
        ConnectAckReason::BadUserNameOrPassword => "the user name or password is wrong",
        ConnectAckReason::NotAuthorized => "this client is not authorized",
        ConnectAckReason::ClientIdentifierNotValid => "the client ID is not valid",
        ConnectAckReason::ServerUnavailable => "the server is not available",
        ConnectAckReason::ServerBusy => "the server is busy",
        ConnectAckReason::Banned => "this client is banned",
        ConnectAckReason::BadAuthenticationMethod => "the authentication method is not accepted",
        ConnectAckReason::UnsupportedProtocolVersion => "the broker does not support MQTT 5",
        ConnectAckReason::PacketTooLarge => "the CONNECT packet is too large",
        ConnectAckReason::QuotaExceeded => "a quota is exceeded",
        ConnectAckReason::UseAnotherServer | ConnectAckReason::ServerMoved => "use another server",
        ConnectAckReason::ConnectionRateExceeded => "the connection rate is exceeded",
        _ => "",
    };
    // The broker chooses this text. It can hold direction controls.
    let text = ack.reason_string.as_deref().map(display_safe);
    match (&text, reason) {
        (Some(s), "") => format!("The broker rejected the connection: {s}."),
        (Some(s), r) => format!("The broker rejected the connection: {r} ({s})."),
        (None, "") => format!("The broker rejected the connection: {:?}.", ack.reason_code),
        (None, r) => format!("The broker rejected the connection: {r}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io_of(e: &NtexError<WsClientError>) -> std::io::Error {
        match &*ws_error(e) {
            ConnectError::Io(io) => std::io::Error::new(io.kind(), io.to_string()),
            other => panic!("expected an I/O error, got {other:?}"),
        }
    }

    /// The upgrade timeout has kind Other and the text "Timeout" (the `io_outcome` branch reads
    /// it). A refused TCP connection keeps its kind. Another upgrade error keeps its text.
    #[test]
    fn websocket_errors_become_io_errors() {
        let timeout = io_of(&NtexError::from(WsClientError::Timeout));
        assert_eq!(timeout.kind(), std::io::ErrorKind::Other);
        assert_eq!(timeout.to_string(), "Timeout");

        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let e = io_of(&NtexError::from(WsClientError::Connect(ConnectError::Io(
            refused,
        ))));
        assert_eq!(e.kind(), std::io::ErrorKind::ConnectionRefused);

        let e = io_of(&NtexError::from(WsClientError::InvalidUpgradeHeader));
        assert_eq!(e.to_string(), "Invalid upgrade header");
    }

    #[test]
    fn a_disconnect_before_the_upgrade_answer_is_a_temporary_close() {
        let e = io_of(&NtexError::from(WsClientError::Disconnected(None)));
        assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(matches!(
            io_outcome(&e, None, false),
            Outcome::Retry(t) if t == "The broker closed the connection."
        ));
    }

    fn suback(reason: SubscribeAckReason, why: Option<&str>) -> SubscribeAck {
        SubscribeAck {
            packet_id: std::num::NonZeroU16::MIN,
            properties: Vec::new(),
            reason_string: why.map(ByteString::from),
            status: vec![reason],
        }
    }

    fn unsuback(reason: UnsubscribeAckReason, why: Option<&str>) -> UnsubscribeAck {
        UnsubscribeAck {
            packet_id: std::num::NonZeroU16::MIN,
            properties: Vec::new(),
            reason_string: why.map(ByteString::from),
            status: vec![reason],
        }
    }

    /// The broker's reason string is in a refusal, after the reason text. It is cleaned first.
    #[test]
    fn suback_and_unsuback_keep_the_reason_string() {
        assert_eq!(
            subscribe_result(&suback(
                SubscribeAckReason::NotAuthorized,
                Some("acl rule 7")
            )),
            Err("Not authorized to subscribe. Broker reason: acl rule 7".to_owned())
        );
        assert_eq!(
            subscribe_result(&suback(SubscribeAckReason::NotAuthorized, None)),
            Err("Not authorized to subscribe.".to_owned())
        );
        assert_eq!(
            subscribe_result(&suback(SubscribeAckReason::GrantedQos1, Some("ignored"))),
            Ok(Qos::AtLeastOnce)
        );
        // A direction control is made visible.
        let shown = subscribe_result(&suback(
            SubscribeAckReason::QuotaExceeded,
            Some("a\u{202e}b"),
        ))
        .expect_err("refused");
        assert!(!shown.contains('\u{202e}'), "{shown}");

        let rejected = |ack| match unsubscribe_result(&ack) {
            Err(UnsubFail::Rejected(t)) => t,
            other => panic!("expected a rejection, got {:?}", other.is_ok()),
        };
        assert_eq!(
            rejected(unsuback(
                UnsubscribeAckReason::NotAuthorized,
                Some("no unsub for you")
            )),
            "Unsubscribe rejected: NotAuthorized: no unsub for you"
        );
        assert_eq!(
            rejected(unsuback(UnsubscribeAckReason::NotAuthorized, None)),
            "Unsubscribe rejected: NotAuthorized"
        );
        assert!(
            unsubscribe_result(&unsuback(
                UnsubscribeAckReason::NoSubscriptionExisted,
                Some("x")
            ))
            .is_ok()
        );
    }

    #[test]
    fn a_will_topic_with_a_wildcard_is_not_valid() {
        let will = |topic: &str| crate::profile::Will {
            topic: topic.into(),
            payload: vec![1],
            qos: Qos::AtLeastOnce,
            retain: true,
            delay_secs: 5,
        };
        assert!(last_will(&will("a/b")).is_some_and(|w| w.will_delay_interval_sec == Some(5)));
        for bad in ["", "a/+", "a/#", "a\0b"] {
            assert!(last_will(&will(bad)).is_none(), "{bad:?}");
        }
    }
}
