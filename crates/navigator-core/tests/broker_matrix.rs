//! Tests against the Docker brokers in tests/broker (Mosquitto 2.x: ten listeners on two services).
//! Run: `tests/broker/up.sh && cargo test -p navigator-core --features broker-tests --test
//! broker_matrix && tests/broker/down.sh`.
#![cfg(feature = "broker-tests")]

use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use navigator_core::model::{Incoming, MsgProps, Qos};
use navigator_core::net::{
    Command, ConnState, ConnectConfig, Event, Net, NetLimits, Outgoing, Profile, Protocol,
    PublishResult, Transport,
};

const WAIT: Duration = Duration::from_secs(10);

const ANON: u16 = 18830;
const AUTH: u16 = 18831;
const TLS: u16 = 18832;
const MTLS: u16 = 18833;
const WS: u16 = 18834;
const WSS: u16 = 18835;
const EXPIRED: u16 = 18836;
const WRONG_HOST: u16 = 18837;
const WSS_MTLS: u16 = 18838;
/// A second Mosquitto with the limits that are global in Mosquitto: `max_keepalive 5`,
/// `max_inflight_messages 2` and `max_packet_size 4096`.
const LIMITS: u16 = 18860;

fn broker_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/broker")
}

fn cert(name: &str) -> PathBuf {
    broker_dir().join("certs").join(name)
}

/// A topic prefix that no other run uses.
fn prefix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("test/matrix/{}-{nanos}", std::process::id())
}

fn profile(port: u16, transport: Transport, prefix: &str) -> Profile {
    Profile {
        host: "127.0.0.1".into(),
        port,
        transport,
        subscriptions: vec![(format!("{prefix}/#"), Qos::AtLeastOnce)],
        protocol: Protocol::V5,
        ..Profile::default()
    }
}

/// A profile for the given MQTT version.
fn profile_with(protocol: Protocol, port: u16, transport: Transport, prefix: &str) -> Profile {
    Profile {
        protocol,
        ..profile(port, transport, prefix)
    }
}

fn with_ca(mut p: Profile) -> Profile {
    p.ca_file = Some(cert("ca.crt"));
    p
}

fn with_client_cert(mut p: Profile, key: &str) -> Profile {
    p.client_cert = Some(cert("client.crt"));
    p.client_key = Some(cert(key));
    p
}

async fn wait_connected(net: &mut Net, previous: u64) -> u64 {
    let s = tokio::time::timeout(
        WAIT,
        net.status
            .wait_for(|s| s.generation > previous && s.state == ConnState::Connected),
    )
    .await
    .expect("timeout: not connected")
    .expect("status");
    s.generation
}

async fn connect(net: &mut Net, p: impl Into<ConnectConfig>) -> u64 {
    let previous = net.status.borrow().generation;
    net.send(Command::Connect(Box::new(p.into())))
        .expect("send");
    wait_connected(net, previous).await
}

/// Connects and expects `Failed`. Returns the text. A retry must not start.
async fn connect_failed(net: &mut Net, p: impl Into<ConnectConfig>) -> String {
    net.send(Command::Connect(Box::new(p.into())))
        .expect("send");
    tokio::time::timeout(
        WAIT,
        net.status
            .wait_for(|s| matches!(s.state, ConnState::Failed(_))),
    )
    .await
    .expect("timeout: no Failed state")
    .expect("status");
    // The first retry would start after 1 s. Wait longer than that.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    match net.status.borrow().state.clone() {
        ConnState::Failed(text) => text,
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// Checks the text for one of the keywords. The match ignores case.
fn assert_text(text: &str, keywords: &[&str]) {
    let lower = text.to_lowercase();
    assert!(
        keywords.iter().any(|k| lower.contains(k)),
        "{text:?} has none of {keywords:?}"
    );
}

async fn next_event_within(net: &mut Net, wait: Duration) -> Event {
    tokio::time::timeout(wait, net.events.recv())
        .await
        .expect("event timeout")
        .expect("events closed")
}

async fn next_incoming(net: &mut Net) -> Incoming {
    let m = tokio::time::timeout(WAIT, net.incoming.recv())
        .await
        .expect("message timeout")
        .expect("closed");
    net.release(&m);
    m
}

/// Waits for the `Subscribed` event of the profile filter. Skips `Reconnected`.
async fn wait_subscribed(net: &mut Net, wait: Duration) {
    loop {
        match next_event_within(net, wait).await {
            Event::Subscribed { result, .. } => {
                assert_eq!(result, Ok(Qos::AtLeastOnce));
                return;
            }
            Event::Reconnected { .. } | Event::ConnectionDetails { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn out(op_id: u64, topic: &str, qos: Qos) -> Command {
    Command::Publish(Box::new(Outgoing {
        op_id,
        topic: topic.into(),
        payload: Bytes::from_static(b"hello"),
        qos,
        retain: false,
        props: MsgProps::default(),
    }))
}

/// Publishes with QoS 0 and QoS 1. Both messages must come back to the subscription.
async fn publish_round_trip(net: &mut Net, prefix: &str, generation: u64) {
    for (id, qos, expect) in [
        (1, Qos::AtMostOnce, PublishResult::Sent),
        (2, Qos::AtLeastOnce, PublishResult::Acknowledged),
    ] {
        let topic = format!("{prefix}/x{id}");
        net.send(out(id, &topic, qos)).expect("send");
        let m = next_incoming(net).await;
        assert_eq!(&*m.topic, topic);
        assert_eq!(&m.payload[..], b"hello");
        assert_eq!(m.generation, generation);
        match next_event_within(net, WAIT).await {
            Event::Published { op_id, result, .. } => assert_eq!((op_id, result), (id, expect)),
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// Connect, subscribe, publish QoS 0 and QoS 1, receive, disconnect.
async fn round_trip<C: Into<ConnectConfig>>(
    port: u16,
    transport: Transport,
    tweak: impl FnOnce(Profile) -> C,
) {
    round_trip_with(Protocol::V5, port, transport, tweak).await;
}

/// The same round trip with the given MQTT version.
async fn round_trip_with<C: Into<ConnectConfig>>(
    protocol: Protocol,
    port: u16,
    transport: Transport,
    tweak: impl FnOnce(Profile) -> C,
) {
    let mut net = Net::start(NetLimits::default());
    let prefix = prefix();
    let mut config: ConnectConfig = tweak(profile(port, transport, &prefix)).into();
    config.protocol = protocol;
    let generation = connect(&mut net, config).await;
    wait_subscribed(&mut net, WAIT).await;
    publish_round_trip(&mut net, &prefix, generation).await;
    if protocol == Protocol::V311 && transport == Transport::Tcp {
        // MQTT 3.1.1 UNSUBACK has no reason code: its own decode path (`wire.rs`, v3 branch).
        net.send(Command::Unsubscribe {
            filter: format!("{prefix}/#"),
        })
        .expect("send");
        match next_event_within(&mut net, WAIT).await {
            Event::Unsubscribed { filter, result, .. } => {
                assert_eq!((filter, result), (format!("{prefix}/#"), Ok(())));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    net.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn password_tcp() {
    round_trip(AUTH, Transport::Tcp, |p| {
        let mut c = ConnectConfig::from(p);
        c.profile.username = Some("test".into());
        c.password = Some("test-pass".into());
        c
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn wrong_password_fails_without_retry() {
    let mut net = Net::start(NetLimits::default());
    let mut p = profile(AUTH, Transport::Tcp, &prefix());
    p.username = Some("test".into());
    let mut c = ConnectConfig::from(p);
    c.password = Some("wrong".into());
    let text = connect_failed(&mut net, c).await;
    assert_text(&text, &["not authorized", "credentials", "password"]);
    // No credentials at all fail too.
    let text = connect_failed(&mut net, profile(AUTH, Transport::Tcp, &prefix())).await;
    assert_text(&text, &["not authorized", "credentials", "password"]);
    net.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn tls_with_test_ca() {
    round_trip(TLS, Transport::Tls, with_ca).await;
}

#[tokio::test(flavor = "current_thread")]
async fn mtls_with_client_certificate() {
    round_trip(MTLS, Transport::Tls, |p| {
        with_client_cert(with_ca(p), "client.key")
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn secure_websocket_with_test_ca() {
    round_trip(WSS, Transport::Wss, with_ca).await;
}

/// MQTT 3.1.1 over every listener that the V5 round trips use.
#[tokio::test(flavor = "current_thread")]
async fn v3_anonymous_tcp() {
    round_trip_with(Protocol::V311, ANON, Transport::Tcp, |p| p).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v3_password_tcp() {
    round_trip_with(Protocol::V311, AUTH, Transport::Tcp, |p| {
        let mut c = ConnectConfig::from(p);
        c.profile.username = Some("test".into());
        c.password = Some("test-pass".into());
        c
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn v3_wrong_password_fails_without_retry() {
    let mut net = Net::start(NetLimits::default());
    let mut p = profile(AUTH, Transport::Tcp, &prefix());
    p.username = Some("test".into());
    let mut c = ConnectConfig::from(p);
    c.protocol = Protocol::V311;
    c.password = Some("wrong".into());
    let text = connect_failed(&mut net, c).await;
    // Mosquitto answers a wrong password with "not authorized" (return code 5). Return code 4
    // has a fake-broker test in `net.rs`.
    assert_eq!(
        text,
        "The broker rejected the connection: this client is not authorized."
    );
    // No credentials at all: the same answer.
    let mut c = ConnectConfig::from(profile(AUTH, Transport::Tcp, &prefix()));
    c.protocol = Protocol::V311;
    let text = connect_failed(&mut net, c).await;
    assert_eq!(
        text,
        "The broker rejected the connection: this client is not authorized."
    );
    net.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn v3_secure_websocket_with_test_ca() {
    round_trip_with(Protocol::V311, WSS, Transport::Wss, with_ca).await;
}

/// The TLS failures stop before the first MQTT byte, so one version is enough (v5).
async fn tls_without_the_ca_is_not_trusted() {
    let mut net = Net::start(NetLimits::default());
    let text = connect_failed(&mut net, profile(TLS, Transport::Tls, &prefix())).await;
    assert_text(&text, &["trusted"]);
    let text = connect_failed(&mut net, profile(WSS, Transport::Wss, &prefix())).await;
    assert_text(&text, &["trusted"]);
    net.shutdown().await;
}

async fn tls_expired_certificate() {
    let mut net = Net::start(NetLimits::default());
    let p = with_ca(profile(EXPIRED, Transport::Tls, &prefix()));
    assert_text(&connect_failed(&mut net, p).await, &["expired"]);
    net.shutdown().await;
}

async fn tls_wrong_host_name() {
    let mut net = Net::start(NetLimits::default());
    let p = with_ca(profile(WRONG_HOST, Transport::Tls, &prefix()));
    assert_text(&connect_failed(&mut net, p).await, &["host name"]);
    net.shutdown().await;
}

async fn mtls_without_client_certificate_fails() {
    let mut net = Net::start(NetLimits::default());
    let p = with_ca(profile(MTLS, Transport::Tls, &prefix()));
    let text = connect_failed(&mut net, p).await;
    assert!(
        text.to_lowercase().contains("client certificate"),
        "unexpected text: {text}"
    );
    net.shutdown().await;
}

async fn mtls_mismatched_key_fails() {
    let mut net = Net::start(NetLimits::default());
    let p = with_client_cert(
        with_ca(profile(MTLS, Transport::Tls, &prefix())),
        "client-mismatch.key",
    );
    assert_text(&connect_failed(&mut net, p).await, &["do not match"]);
    net.shutdown().await;
}

/// The anonymous TCP and WebSocket round trips and the five TLS failures share nothing but the
/// broker. They run at the same time, each on its own `Net`: the failures wait 1.5 s each to see
/// that no retry starts.
#[tokio::test(flavor = "current_thread")]
async fn anonymous_transports_and_tls_failures() {
    tokio::join!(
        round_trip(ANON, Transport::Tcp, |p| p),
        round_trip(WS, Transport::Ws, |p| p),
        tls_without_the_ca_is_not_trusted(),
        tls_expired_certificate(),
        tls_wrong_host_name(),
        mtls_without_client_certificate_fails(),
        mtls_mismatched_key_fails(),
    );
}

#[tokio::test(flavor = "current_thread")]
async fn tls_verify_off_accepts_untrusted_certificates() {
    // No CA file, so the server is not trusted. The expired and wrong-host listeners are bad too.
    for port in [TLS, EXPIRED, WRONG_HOST] {
        round_trip(port, Transport::Tls, |mut p| {
            p.verify_tls = false;
            p
        })
        .await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn disconnect_stops_incoming() {
    let mut net = Net::start(NetLimits::default());
    let prefix = prefix();
    let generation = connect(&mut net, profile(ANON, Transport::Tcp, &prefix)).await;
    wait_subscribed(&mut net, WAIT).await;
    publish_round_trip(&mut net, &prefix, generation).await;
    net.disconnect();
    let ended = tokio::time::timeout(
        WAIT,
        net.status.wait_for(|s| s.state == ConnState::Disconnected),
    )
    .await
    .expect("timeout")
    .expect("status")
    .generation;
    assert_eq!(ended, generation);
    while net.incoming.try_recv().is_ok() {}
    // A second client publishes. This client must not get it.
    let mut other = Net::start(NetLimits::default());
    let mut p = profile(ANON, Transport::Tcp, &prefix);
    p.subscriptions.clear();
    connect(&mut other, p).await;
    other
        .send(out(1, &format!("{prefix}/late"), Qos::AtLeastOnce))
        .expect("send");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(net.incoming.try_recv().is_err(), "message after disconnect");
    other.shutdown().await;
}

/// A QoS 1 message with a whole packet of exactly 1 MiB (the default `max_packet`) goes out and
/// comes back, on every transport and both versions (S1 spike case group "c"; the spike is deleted). In MQTT 5
/// Mosquitto adds a Topic Alias property (id 0x23 and 2 bytes: 3 bytes) to the first copy that it
/// forwards for a topic. The proxy test `v5_forwarded_copy_carries_a_topic_alias` shows this on
/// the wire. So a v5 packet of 1 MiB - 3 bytes comes back as exactly 1 MiB. The second packet is
/// exactly 1 MiB when it goes out. Its copy has an empty topic (the alias is known), so it is
/// smaller than 1 MiB and arrives too.
async fn one_mib_round_trip(protocol: Protocol, port: u16, transport: Transport, ca: bool) {
    const MIB: usize = 1024 * 1024;
    const TOPIC_ALIAS: usize = 3;
    let mut net = Net::start(NetLimits::default());
    let prefix = prefix();
    let topic = format!("{prefix}/mib");
    let mut p = profile_with(protocol, port, transport, &prefix);
    p.subscriptions = vec![(topic.clone(), Qos::AtLeastOnce)];
    if ca {
        p = with_ca(p);
    }
    connect(&mut net, p).await;
    wait_subscribed(&mut net, WAIT).await;
    let sends = if protocol == Protocol::V5 {
        vec![MIB - TOPIC_ALIAS, MIB]
    } else {
        vec![MIB]
    };
    for (op_id, total) in (1u64..).zip(sends) {
        net.send(one_mib_message(&topic, protocol, op_id, total))
            .expect("send");
        let m = next_incoming(&mut net).await;
        assert_eq!(m.payload.len(), total - one_mib_overhead(&topic, protocol));
        match next_event_within(&mut net, WAIT).await {
            Event::Published {
                op_id: id, result, ..
            } => assert_eq!((id, result), (op_id, PublishResult::Acknowledged)),
            other => panic!("unexpected {other:?}"),
        }
    }
    net.shutdown().await;
}

/// Fixed header 1, length bytes 3, topic length 2, topic, packet ID 2, and in MQTT 5 one byte
/// for the empty property list.
fn one_mib_overhead(topic: &str, protocol: Protocol) -> usize {
    1 + 3 + 2 + topic.len() + 2 + usize::from(protocol == Protocol::V5)
}

/// A QoS 1 message whose whole packet is `total` bytes.
fn one_mib_message(topic: &str, protocol: Protocol, op_id: u64, total: usize) -> Command {
    Command::Publish(Box::new(Outgoing {
        op_id,
        topic: topic.into(),
        payload: Bytes::from(vec![b'x'; total - one_mib_overhead(topic, protocol)]),
        qos: Qos::AtLeastOnce,
        retain: false,
        props: MsgProps::default(),
    }))
}

/// A packet that the proxy saw: the direction, the time, the whole size and the first bytes.
struct Seen {
    to_client: bool,
    at: std::time::Instant,
    total: usize,
    head: Vec<u8>,
}

/// A TCP proxy for one connection to `upstream`. It copies the bytes both ways and reports each
/// packet of the type `kind` (the high nibble of the first byte: 0x30 PUBLISH, 0xC0 PINGREQ).
/// Returns the port to dial and the report channel.
fn sniffing_proxy(upstream: u16, kind: u8) -> (u16, std::sync::mpsc::Receiver<Seen>) {
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok((client, _)) = listener.accept() else {
            return;
        };
        let Ok(broker) = TcpStream::connect(("127.0.0.1", upstream)) else {
            return;
        };
        let pump = |mut from: TcpStream,
                    mut to: TcpStream,
                    to_client: bool,
                    tx: std::sync::mpsc::Sender<Seen>| {
            std::thread::spawn(move || {
                let (mut buf, mut chunk) = (Vec::<u8>::new(), vec![0u8; 65536]);
                while let Ok(n) = from.read(&mut chunk) {
                    if n == 0 || to.write_all(&chunk[..n]).is_err() {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    // Split the buffer into whole packets.
                    loop {
                        let (mut len, mut mult, mut i) = (0usize, 1usize, 1usize);
                        let mut complete = false;
                        while let Some(&b) = buf.get(i) {
                            i += 1;
                            len += usize::from(b & 0x7f) * mult;
                            mult *= 128;
                            if b & 0x80 == 0 {
                                complete = true;
                                break;
                            }
                        }
                        if !complete || buf.len() < i + len {
                            break;
                        }
                        let packet: Vec<u8> = buf.drain(..i + len).collect();
                        if packet[0] & 0xf0 == kind {
                            let _ = tx.send(Seen {
                                to_client,
                                at: std::time::Instant::now(),
                                total: packet.len(),
                                head: packet[..packet.len().min(256)].to_vec(),
                            });
                        }
                    }
                }
                let _ = to.shutdown(Shutdown::Write);
            });
        };
        pump(
            client.try_clone().expect("clone"),
            broker.try_clone().expect("clone"),
            false,
            tx.clone(),
        );
        pump(broker, client, true, tx);
    });
    (port, rx)
}

/// The packet that Mosquitto forwards to a `Net` subscriber, seen on the wire (this explains the 3
/// bytes in `one_mib_round_trip`). The client announces a Topic Alias Maximum in CONNECT. For the
/// first message on a topic the broker adds the property Topic Alias (0x23, 2 bytes: 3 bytes with
/// the property length 3 instead of 0). For the second message it sends an empty topic and the
/// same alias. The sizes are asserted from the captured packets.
#[tokio::test(flavor = "current_thread")]
async fn v5_forwarded_copy_carries_a_topic_alias() {
    const MIB: usize = 1024 * 1024;
    let (port, seen) = sniffing_proxy(ANON, 0x30);
    let mut net = Net::start(NetLimits::default());
    let prefix = prefix();
    let topic = format!("{prefix}/alias");
    let mut p = profile_with(Protocol::V5, port, Transport::Tcp, &prefix);
    p.subscriptions = vec![(topic.clone(), Qos::AtLeastOnce)];
    connect(&mut net, p).await;
    wait_subscribed(&mut net, WAIT).await;
    let overhead = one_mib_overhead(&topic, Protocol::V5);
    for (op_id, total) in [(1u64, MIB - 3), (2, MIB)] {
        net.send(one_mib_message(&topic, Protocol::V5, op_id, total))
            .expect("send");
        let m = next_incoming(&mut net).await;
        assert_eq!(m.payload.len(), total - overhead);
        match next_event_within(&mut net, WAIT).await {
            Event::Published { result, .. } => assert_eq!(result, PublishResult::Acknowledged),
            other => panic!("unexpected {other:?}"),
        }
    }
    net.shutdown().await;
    // The proxy reports a packet after it forwarded it: wait for the four PUBLISH packets.
    let packets: Vec<Seen> = (0..4)
        .map(|_| {
            seen.recv_timeout(WAIT)
                .expect("a PUBLISH packet at the proxy")
        })
        .collect();
    let sent: Vec<_> = packets.iter().filter(|s| !s.to_client).collect();
    let copies: Vec<_> = packets.iter().filter(|s| s.to_client).collect();
    // What we send is exactly the size that the test computes.
    assert_eq!(
        sent.iter().map(|s| s.total).collect::<Vec<_>>(),
        [MIB - 3, MIB]
    );
    assert_eq!(copies.len(), 2, "forwarded copies");
    // First copy: the topic, the packet ID, then property length 3 and Topic Alias (0x23).
    let first = copies[0];
    let at = first.head.len().min(1 + 3 + 2 + topic.len() + 2);
    assert_eq!(
        &first.head[at..at + 4],
        &[3, 0x23, 0, 1],
        "{:02x?}",
        &first.head[at - 6..at + 4]
    );
    assert_eq!(
        first.total, MIB,
        "the first copy is 3 bytes larger than the packet we sent"
    );
    // Second copy: topic length 0 (the alias is known), so it is smaller than what we sent.
    let second = copies[1];
    assert_eq!(&second.head[4..6], &[0, 0], "topic length");
    assert_eq!(second.total, MIB + 3 - topic.len());
}

/// MQTT 5, Server Keep Alive: the broker answers a keep alive above 5 s with 5 (`max_keepalive`),
/// and the client follows it, not the 60 s of the profile (plan, row "A dead peer is found").
/// The proxy sees the first PINGREQ about 5 s after the connect. The CONNACK values are in the
/// connection details.
#[tokio::test(flavor = "current_thread")]
async fn v5_server_keep_alive_5_s_is_followed() {
    let (port, seen) = sniffing_proxy(LIMITS, 0xC0);
    let mut net = Net::start(NetLimits::default());
    let mut p = profile(port, Transport::Tcp, &prefix());
    p.keepalive_secs = 60;
    p.subscriptions.clear();
    connect(&mut net, p).await;
    let up = std::time::Instant::now();
    match next_event_within(&mut net, WAIT).await {
        Event::ConnectionDetails { details, .. } => {
            let v5 = details.v5.expect("v5 details");
            // `max_inflight_messages 2` is the Receive Maximum of this broker.
            assert_eq!(
                (
                    v5.keep_alive_secs,
                    v5.server_keep_alive_secs,
                    v5.receive_max
                ),
                (5, Some(5), 2)
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    let ping = seen.recv_timeout(WAIT).expect("a PINGREQ");
    assert!(!ping.to_client);
    let first = ping.at.duration_since(up);
    assert!(
        (Duration::from_millis(4_500)..=Duration::from_millis(6_500)).contains(&first),
        "{first:?}"
    );
    assert_eq!(net.status.borrow().state, ConnState::Connected);
    net.shutdown().await;
}

/// A broker that needs a client certificate over secure WebSocket (18838). With the certificate
/// the round trip works; without it the attempt fails with the client-certificate text. The TLS
/// set-up is the same for both versions, and the upgrade runs after it, so the four runs show
/// that the certificate reaches the WebSocket client (`wire.rs`, WSS connector).
#[tokio::test(flavor = "current_thread")]
async fn mtls_over_secure_websocket() {
    async fn with(protocol: Protocol) {
        round_trip_with(protocol, WSS_MTLS, Transport::Wss, |p| {
            with_client_cert(with_ca(p), "client.key")
        })
        .await;
    }
    async fn without(protocol: Protocol) {
        let mut net = Net::start(NetLimits::default());
        let p = with_ca(profile_with(protocol, WSS_MTLS, Transport::Wss, &prefix()));
        let text = connect_failed(&mut net, p).await;
        assert_eq!(
            text,
            "The broker needs a client certificate. Add your certificate and key."
        );
        net.shutdown().await;
    }
    tokio::join!(
        with(Protocol::V5),
        with(Protocol::V311),
        without(Protocol::V5),
        without(Protocol::V311),
    );
}

/// MQTT 5, the broker announces Maximum Packet Size 4096 in CONNACK (the limits broker, 18860). The limit is in the
/// connection details and in `broker_max_packet`. A PUBLISH of exactly that size is sent; one
/// byte more is refused before the send, with the text of `publish::validate_outgoing`. The
/// broker would end the session for the larger packet, so the state stays `Connected` (plan, row
/// "Publish size", S1 finding 4).
#[tokio::test(flavor = "current_thread")]
async fn a_v5_publish_over_the_announced_maximum_packet_size_is_refused_before_the_send() {
    const LIMIT: usize = 4096;
    let mut net = Net::start(NetLimits::default());
    let prefix = prefix();
    let mut p = profile(LIMITS, Transport::Tcp, &prefix);
    p.subscriptions.clear();
    connect(&mut net, p).await;
    match next_event_within(&mut net, WAIT).await {
        Event::ConnectionDetails { details, .. } => {
            assert_eq!(details.v5.expect("v5").max_packet_size, Some(LIMIT as u32));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(net.broker_max_packet(), Some(LIMIT as u32));
    let topic = format!("{prefix}/m");
    let message = |op_id: u64, total: usize| {
        Command::Publish(Box::new(Outgoing {
            op_id,
            topic: topic.clone(),
            // Fixed header 1, length 2, topic 2 + len, packet ID 2, empty properties 1.
            payload: Bytes::from(vec![b'x'; total - (1 + 2 + 2 + topic.len() + 2 + 1)]),
            qos: Qos::AtLeastOnce,
            retain: false,
            props: MsgProps::default(),
        }))
    };
    net.send(message(1, LIMIT)).expect("send");
    net.send(message(2, LIMIT + 1)).expect("send");
    let mut results = Vec::new();
    while results.len() < 2 {
        match next_event_within(&mut net, WAIT).await {
            Event::Published { op_id, result, .. } => results.push((op_id, result)),
            other => panic!("unexpected {other:?}"),
        }
    }
    results.sort_by_key(|(id, _)| *id);
    assert_eq!(
        results,
        vec![
            (1, PublishResult::Acknowledged),
            (
                2,
                PublishResult::Rejected(
                    "The message needs 4097 bytes. The broker accepts 4096 bytes at most.".into()
                )
            ),
        ]
    );
    assert_eq!(net.status.borrow().state, ConnState::Connected);
    net.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn one_mib_over_tcp() {
    one_mib_round_trip(Protocol::V5, ANON, Transport::Tcp, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn one_mib_over_websocket() {
    one_mib_round_trip(Protocol::V5, WS, Transport::Ws, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn one_mib_over_secure_websocket() {
    one_mib_round_trip(Protocol::V5, WSS, Transport::Wss, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v3_one_mib_over_secure_websocket() {
    one_mib_round_trip(Protocol::V311, WSS, Transport::Wss, true).await;
}

/// A DNS name (not an IP address) gives SNI, host name verification and the WebSocket authority
/// (S1 spike case "TLS (localhost)"; the spike is deleted).
async fn dns_name_round_trip(protocol: Protocol, port: u16, transport: Transport) {
    round_trip_with(protocol, port, transport, |mut p| {
        p.host = "localhost".into();
        with_ca(p)
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn tls_with_a_dns_name() {
    dns_name_round_trip(Protocol::V5, TLS, Transport::Tls).await;
}

#[tokio::test(flavor = "current_thread")]
async fn secure_websocket_with_a_dns_name() {
    dns_name_round_trip(Protocol::V5, WSS, Transport::Wss).await;
}
