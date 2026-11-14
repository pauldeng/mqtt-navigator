//! One small interface over the v3 and v5 clients (the plan's `Wire`), the connect function for
//! all five transports, and the adapters that run `start_with_control`.

use std::cell::RefCell;
use std::rc::Rc;

use ntex::ws::{WsClient, WsClientConfig};
use ntex_bytes::{ByteString, Bytes};
use ntex_io::IoConfig;
use ntex_mqtt::{MqttServiceConfig, control::Control, v3, v5};
use ntex_net::connect::{Connect, ConnectError};
use ntex_service::{Pipeline, cfg::SharedCfg, fn_service};
use ntex_tls::TlsConfig;
use ntex_util::time::Seconds;
use tokio::sync::mpsc;

use crate::dialer::{Dialer, Target};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ver {
    V3,
    V5,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tr {
    Tcp,
    Tls,
    Ws,
    Wss,
}

/// All knobs of one attempt. Times are scaled down by some fake-broker cases (the report says so).
#[derive(Clone)]
pub struct Opts {
    pub ver: Ver,
    pub tr: Tr,
    pub host: String,
    pub port: u16,
    pub ws_path: String,
    pub ssl: Option<openssl::ssl::SslConnector>,
    pub keep_alive: u16,
    pub max_packet: u32,
    pub write_timeout: u16,
    pub frame_rate: (u16, u16, u32),
    pub handshake_timeout: u16,
    pub client_id: String,
    pub username: Option<(String, String)>,
    pub resolver: Option<crate::dialer::Resolver>,
    /// The one deadline over DNS, dial, TLS, upgrade, CONNECT and CONNACK.
    pub connect_timeout: std::time::Duration,
}

impl Opts {
    pub fn new(ver: Ver, tr: Tr, host: &str, port: u16) -> Self {
        Self {
            ver,
            tr,
            host: host.into(),
            port,
            ws_path: "/mqtt".into(),
            ssl: None,
            keep_alive: 30,
            max_packet: 1 << 20,
            write_timeout: 10,
            frame_rate: (10, 60, 256),
            handshake_timeout: 10,
            client_id: format!("spike-{}", fastid()),
            username: None,
            resolver: None,
            connect_timeout: std::time::Duration::from_secs(10),
        }
    }
}

fn fastid() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    std::process::id() as u64 * 1000 + N.fetch_add(1, Ordering::Relaxed)
}

/// Largest Remaining Length `r` with `1 + varint_len(r) + r <= max_packet` (v3 `set_max_size`).
pub fn v3_max_remaining(max_packet: u32) -> u32 {
    let varint_len = |r: u32| match r {
        0..=127 => 1,
        128..=16_383 => 2,
        16_384..=2_097_151 => 3,
        _ => 4,
    };
    (0..max_packet)
        .rev()
        .find(|&r| 1 + varint_len(r) + r <= max_packet)
        .unwrap_or(0)
}

pub fn shared_cfg(o: &Opts) -> SharedCfg {
    let mut ws = WsClientConfig::new()
        .set_max_frame_size(o.max_packet as usize)
        .set_max_message_size(o.max_packet as usize)
        // The WebSocket handshake timeout stays at the ntex default (5 s), under our 10 s deadline.
        .set_protocols(["mqtt"])
        .expect("protocol");
    ws = ws.set_close_timeout(ntex_util::time::Millis(2000));
    let mut mqtt = MqttServiceConfig::new()
        .set_handshake_timeout(Seconds(o.handshake_timeout))
        .set_max_receive(16)
        .set_max_send(16);
    if o.ver == Ver::V3 {
        mqtt = mqtt.set_max_size(v3_max_remaining(o.max_packet));
    }
    let (a, b, c) = o.frame_rate;
    SharedCfg::new("spike")
        .add(
            IoConfig::new()
                .set_connect_timeout(ntex_util::time::Millis(
                    u32::from(o.handshake_timeout) * 1000,
                ))
                .set_write_timeout(Seconds(o.write_timeout))
                .set_frame_read_rate(Seconds(a), Seconds(b), c),
        )
        .add(mqtt)
        .add(TlsConfig::new())
        .add(ws)
        .build()
}

/// Bound of the incoming queue. A full queue drops the message (the app counts drops).
const INCOMING_BOUND: usize = 256;

/// Messages dropped because the incoming queue was full.
pub static DROPPED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub struct Msg {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: u8,
}

/// Why a connection ended, as the adapters see it.
#[derive(Debug, Clone, Default)]
pub struct End {
    pub result: String,
    pub stop: String,
    pub broker_disconnect: Option<String>,
    /// When the control service got `Stop`.
    pub stop_at: Option<std::time::Instant>,
}

#[derive(Clone)]
pub enum Sink {
    V3(v3::MqttSink),
    V5(v5::MqttSink),
}

pub struct Conn {
    pub sink: Sink,
    pub rx: mpsc::Receiver<Msg>,
    pub end: tokio::task::JoinHandle<End>,
    pub ack: String,
}

/// Why `connect` failed. `text` is the Debug of the ntex error. `diag` is our TLS diagnosis.
#[derive(Debug)]
pub struct ConnErr {
    pub text: String,
    /// `Display` of the I/O error inside, if any: the text `net.rs` `io_outcome` reads today.
    pub io_text: Option<String>,
    pub diag: Option<String>,
}

/// The text is read through `Debug` (it shows in the stop reason).
#[derive(Debug)]
#[allow(dead_code)]
pub struct HandlerErr(pub String);

pub async fn connect(
    o: &Opts,
    diag: Option<&navigator_core::tls::TlsDiag>,
) -> Result<Conn, ConnErr> {
    match tokio::time::timeout(o.connect_timeout, connect_inner(o, diag)).await {
        Ok(r) => r,
        Err(_) => Err(ConnErr {
            text: "Our connect deadline expired".into(),
            io_text: None,
            diag: None,
        }),
    }
}

async fn connect_inner(
    o: &Opts,
    diag: Option<&navigator_core::tls::TlsDiag>,
) -> Result<Conn, ConnErr> {
    let cfg = shared_cfg(o);
    let tls = matches!(o.tr, Tr::Tls | Tr::Wss);
    let mut dialer = Dialer::new(if tls { o.ssl.clone() } else { None });
    if let Some(r) = &o.resolver {
        dialer = dialer.with_resolver(r.clone());
    }
    let target = Target {
        host: o.host.clone(),
        port: o.port,
    };
    let ws = matches!(o.tr, Tr::Ws | Tr::Wss);
    let uri = format!(
        "{}://{}:{}{}",
        if tls { "wss" } else { "ws" },
        o.host,
        o.port,
        o.ws_path
    );
    // The WS connector: our dialer gives the socket, then the WebSocket upgrade runs on it.
    let ws_client = Rc::new(WsClient::new(uri, cfg.clone()).connector(dialer.clone()));
    let ws_connector = move |_: Connect<Target>| {
        let c = ws_client.clone();
        async move {
            let conn = c.connect().await.map_err(|e| {
                ntex_error::Error::from(ConnectError::Io(std::io::Error::other(format!("{e:?}"))))
            })?;
            Ok::<_, ntex_error::Error<ConnectError>>(conn.into_transport().seal())
        }
    };
    let ka = Seconds(o.keep_alive);
    let wrap = |text: String, io_text: Option<String>| ConnErr {
        text,
        io_text,
        diag: diag.and_then(|d| d.text()),
    };
    match o.ver {
        Ver::V3 => {
            let mut req = v3::client::Connect::new(target)
                .client_id(o.client_id.clone())
                .clean_session()
                .keep_alive(ka);
            if let Some((u, p)) = &o.username {
                req = req.username(u.clone()).password(Bytes::from(p.clone()));
            }
            let res = if ws {
                Pipeline::new(
                    cfg,
                    v3::client::MqttConnector::new().connector(ws_connector),
                )
                .call(req)
                .await
            } else {
                Pipeline::new(cfg, v3::client::MqttConnector::new().connector(dialer))
                    .call(req)
                    .await
            };
            match res {
                Ok(c) => {
                    let ack = format!("session_present={}", c.session_present());
                    Ok(start_v3(c, ack))
                }
                Err(e) => Err(wrap(format!("{:?}", *e), io_text(&e))),
            }
        }
        Ver::V5 => {
            let mut req = v5::client::Connect::new(target)
                .client_id(o.client_id.clone())
                .clean_start()
                .keep_alive(ka)
                .max_packet_size(o.max_packet)
                .max_receive(16);
            if let Some((u, p)) = &o.username {
                req = req
                    .username(ByteString::from(u.clone()))
                    .password(Bytes::from(p.clone()));
            }
            let res = if ws {
                Pipeline::new(
                    cfg,
                    v5::client::MqttConnector::new().connector(ws_connector),
                )
                .call(req)
                .await
            } else {
                Pipeline::new(cfg, v5::client::MqttConnector::new().connector(dialer))
                    .call(req)
                    .await
            };
            match res {
                Ok(c) => {
                    let ack = format!("{:?}", c.packet());
                    Ok(start_v5(c, ack))
                }
                Err(e) => Err(wrap(format!("{:?}", *e), io_text(&e))),
            }
        }
    }
}

fn io_text<T: std::fmt::Debug>(e: &ntex_mqtt::error::MqttClientError<T>) -> Option<String> {
    use ntex_mqtt::error::MqttClientError as E;
    match e {
        E::Disconnected(Some(io)) | E::Connect(ConnectError::Io(io)) => Some(io.to_string()),
        _ => None,
    }
}

/// `catch_unwind` for a future: a panic after an await becomes an `Err`. Boxed, so it is `Unpin`.
pub struct Contain<T>(std::pin::Pin<Box<dyn Future<Output = T>>>);

impl<T> Contain<T> {
    pub fn new(f: impl Future<Output = T> + 'static) -> Self {
        Self(Box::pin(f))
    }
}

impl<T> Future for Contain<T> {
    type Output = Result<T, String>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| this.0.as_mut().poll(cx))) {
            Ok(p) => p.map(Ok),
            Err(p) => {
                let msg = p
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic".into());
                std::task::Poll::Ready(Err(msg))
            }
        }
    }
}

/// Probe for the panic case: the first handler call is held pending (a gate), the second call
/// panics after an await. The counters prove the identities of the calls.
#[derive(Default)]
pub struct Probe {
    pub on: std::cell::Cell<bool>,
    pub started: std::cell::Cell<usize>,
    pub completed: std::cell::Cell<usize>,
    pub panicked: std::cell::Cell<usize>,
}

thread_local! {
    pub static PROBE: Probe = Probe::default();
}

async fn probe_gate() {
    let n = PROBE.with(|p| {
        if p.on.get() {
            p.started.set(p.started.get() + 1);
            p.started.get()
        } else {
            0
        }
    });
    match n {
        0 => {}
        1 => std::future::pending::<()>().await,
        _ => {
            ntex_util::time::sleep(ntex_util::time::Millis(50)).await;
            PROBE.with(|p| p.panicked.set(p.panicked.get() + 1));
            panic!("spike: handler panic after await");
        }
    }
}

fn start_v3(c: v3::client::Client, ack: String) -> Conn {
    use v3::client::ProtocolMessage as M;
    let sink = c.sink();
    let (tx, rx) = mpsc::channel(INCOMING_BOUND);
    let stop = Rc::new(RefCell::new(String::new()));
    let (stop2, sink2) = (stop.clone(), sink.clone());
    let end = tokio::task::spawn_local(async move {
        let proto = fn_service(move |msg: M| {
            let (tx, sink) = (tx.clone(), sink2.clone());
            async move {
                let run = Contain::new(async move {
                    probe_gate().await;
                    match msg {
                        M::Publish(p) => {
                            let (qos, topic) = (p.packet().qos, p.packet().topic.to_string());
                            if qos == v3::QoS::ExactlyOnce {
                                return Err(HandlerErr("QoS 2".into()));
                            }
                            let payload = p
                                .read_all()
                                .await
                                .map_err(|e| HandlerErr(format!("{e:?}")))?;
                            let msg = Msg {
                                topic,
                                payload: payload.to_vec(),
                                qos: qos as u8,
                            };
                            if tx.try_send(msg).is_err() {
                                DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            Ok(p.ack())
                        }
                        other => Ok(other.ack()),
                    }
                });
                match run.await {
                    Ok(r) => {
                        PROBE.with(|p| p.completed.set(p.completed.get() + 1));
                        r
                    }
                    Err(p) => {
                        sink.force_close();
                        Err(HandlerErr(format!("panic: {p}")))
                    }
                }
            }
        });
        let stop_at = Rc::new(RefCell::new(None));
        let stop_at2 = stop_at.clone();
        let control = fn_service(move |m: Control<HandlerErr>| {
            if let Control::Stop(r) = &m {
                *stop2.borrow_mut() = format!("{r:?}");
                *stop_at2.borrow_mut() = Some(std::time::Instant::now());
            }
            async { Ok::<_, ()>(None) }
        });
        let r = c.start_with_control(proto, control).await;
        End {
            result: format!("{r:?}"),
            stop: stop.borrow().clone(),
            broker_disconnect: None,
            stop_at: *stop_at.borrow(),
        }
    });
    Conn {
        sink: Sink::V3(sink),
        rx,
        end,
        ack,
    }
}

fn start_v5(c: v5::client::Client, ack: String) -> Conn {
    use v5::client::ProtocolMessage as M;
    let sink = c.sink();
    let (tx, rx) = mpsc::channel(INCOMING_BOUND);
    let stop = Rc::new(RefCell::new(String::new()));
    let bd = Rc::new(RefCell::new(None));
    let (stop2, sink2, bd2) = (stop.clone(), sink.clone(), bd.clone());
    let end = tokio::task::spawn_local(async move {
        let proto = fn_service(move |msg: M| {
            let (tx, sink, bd) = (tx.clone(), sink2.clone(), bd2.clone());
            let sink_in = sink.clone();
            async move {
                let run = Contain::new(async move {
                    probe_gate().await;
                    if let M::Publish(p) = &msg
                        && p.packet().qos == v5::QoS::ExactlyOnce
                    {
                        // `ProtocolMessage::disconnect` closes the io before the dispatcher
                        // writes its packet, so the DISCONNECT is lost (checked by case d).
                        // `close_with_reason` writes it first.
                        sink_in.close_with_reason(v5::codec::Disconnect::new(
                            v5::codec::DisconnectReasonCode::QosNotSupported,
                        ));
                        return Err(HandlerErr("QoS 2".into()));
                    }
                    match msg {
                        M::Publish(p) => {
                            let (qos, topic) = (p.packet().qos, p.packet().topic.to_string());
                            let payload = p
                                .read_all()
                                .await
                                .map_err(|e| HandlerErr(format!("{e:?}")))?;
                            let msg = Msg {
                                topic,
                                payload: payload.to_vec(),
                                qos: qos as u8,
                            };
                            if tx.try_send(msg).is_err() {
                                DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            Ok(p.ack(v5::codec::PublishAckReason::Success))
                        }
                        M::Disconnect(d) => {
                            *bd.borrow_mut() = Some(format!("{:?}", d.packet()));
                            Ok(d.ack())
                        }
                        other => Ok(other.ack()),
                    }
                });
                match run.await {
                    Ok(r) => {
                        PROBE.with(|p| p.completed.set(p.completed.get() + 1));
                        r
                    }
                    Err(p) => {
                        sink.force_close();
                        Err(HandlerErr(format!("panic: {p}")))
                    }
                }
            }
        });
        let stop_at = Rc::new(RefCell::new(None));
        let stop_at2 = stop_at.clone();
        let control = fn_service(move |m: Control<HandlerErr>| {
            if let Control::Stop(r) = &m {
                *stop2.borrow_mut() = format!("{r:?}");
                *stop_at2.borrow_mut() = Some(std::time::Instant::now());
            }
            async { Ok::<_, ()>(None) }
        });
        let r = c.start_with_control(proto, control).await;
        End {
            result: format!("{r:?}"),
            stop: stop.borrow().clone(),
            broker_disconnect: bd.borrow().clone(),
            stop_at: *stop_at.borrow(),
        }
    });
    Conn {
        sink: Sink::V5(sink),
        rx,
        end,
        ack,
    }
}

impl Sink {
    pub fn is_open(&self) -> bool {
        match self {
            Sink::V3(s) => s.is_open(),
            Sink::V5(s) => s.is_open(),
        }
    }
    pub fn close(&self) {
        match self {
            Sink::V3(s) => s.close(),
            Sink::V5(s) => s.close(),
        }
    }
    pub fn force_close(&self) {
        match self {
            Sink::V3(s) => s.force_close(),
            Sink::V5(s) => s.force_close(),
        }
    }

    /// QoS 0 or 1 publish. Returns the ack text.
    pub async fn publish(&self, topic: &str, payload: Vec<u8>, qos: u8) -> Result<String, String> {
        let body = Bytes::from(payload);
        match (self, qos) {
            (Sink::V3(s), 0) => s
                .publish(topic.to_owned())
                .send_at_most_once(body)
                .await
                .map(|()| "sent".into())
                .map_err(|e| format!("{e:?}")),
            (Sink::V3(s), _) => s
                .publish(topic.to_owned())
                .send_at_least_once(body)
                .await
                .map(|()| "acked".into())
                .map_err(|e| format!("{e:?}")),
            (Sink::V5(s), 0) => s
                .publish(topic.to_owned())
                .send_at_most_once(body)
                .await
                .map(|()| "sent".into())
                .map_err(|e| format!("{e:?}")),
            (Sink::V5(s), _) => s
                .publish(topic.to_owned())
                .send_at_least_once(body)
                .await
                .map(|a| format!("acked {:?}", a.reason_code))
                .map_err(|e| format!("{e:?}")),
        }
    }

    pub async fn subscribe(&self, filter: &str, qos: u8) -> Result<String, String> {
        let f = ByteString::from(filter.to_owned());
        match self {
            Sink::V3(s) => {
                let q = if qos == 0 {
                    v3::QoS::AtMostOnce
                } else {
                    v3::QoS::AtLeastOnce
                };
                s.subscribe()
                    .topic_filter(f, q)
                    .send()
                    .await
                    .map(|r| format!("{r:?}"))
                    .map_err(|e| format!("{e:?}"))
            }
            Sink::V5(s) => {
                let opts = v5::codec::SubscriptionOptions {
                    qos: if qos == 0 {
                        v5::QoS::AtMostOnce
                    } else {
                        v5::QoS::AtLeastOnce
                    },
                    no_local: false,
                    retain_as_published: false,
                    retain_handling: v5::codec::RetainHandling::AtSubscribe,
                };
                s.subscribe(None)
                    .topic_filter(f, opts)
                    .send()
                    .await
                    .map(|r| format!("{:?}", r.status))
                    .map_err(|e| format!("{e:?}"))
            }
        }
    }
}
