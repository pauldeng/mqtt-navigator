//! Tests against the Docker broker in tests/broker (Mosquitto 2.x, eight listeners).
//! Run: `tests/broker/up.sh && cargo test -p navigator-core --features broker-tests --test
//! broker_matrix -- --test-threads=1 && tests/broker/down.sh`.
#![cfg(feature = "broker-tests")]
// Each test holds `LIMITS` for its whole run. The guard must live across the awaits.
#![allow(clippy::await_holding_lock)]

use std::path::PathBuf;
use std::process::Command as Process;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use navigator_core::model::{Incoming, MsgProps, Qos};
use navigator_core::net::{
    Command, ConnState, ConnectConfig, Event, Net, NetLimits, Outgoing, Profile, PublishResult,
    Transport,
};

const WAIT: Duration = Duration::from_secs(10);
/// A broker restart takes a few seconds. The client waits 1 s or more before it retries.
const RESTART_WAIT: Duration = Duration::from_secs(40);

const ANON: u16 = 18830;
const AUTH: u16 = 18831;
const TLS: u16 = 18832;
const MTLS: u16 = 18833;
const WS: u16 = 18834;
const WSS: u16 = 18835;
const EXPIRED: u16 = 18836;
const WRONG_HOST: u16 = 18837;

/// Every `Net` sets the process-global `ms_mqtt_client::local_limits`. Each test takes this lock.
static LIMITS: Mutex<()> = Mutex::new(());

fn limits_lock() -> std::sync::MutexGuard<'static, ()> {
    LIMITS.lock().unwrap_or_else(|e| e.into_inner())
}

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
        ..Profile::default()
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
            Event::Reconnected { .. } => {}
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
    let _limits = limits_lock();
    let mut net = Net::start(NetLimits::default());
    let prefix = prefix();
    let generation = connect(&mut net, tweak(profile(port, transport, &prefix))).await;
    wait_subscribed(&mut net, WAIT).await;
    publish_round_trip(&mut net, &prefix, generation).await;
    net.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn anonymous_tcp() {
    round_trip(ANON, Transport::Tcp, |p| p).await;
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
    let _limits = limits_lock();
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
async fn websocket() {
    round_trip(WS, Transport::Ws, |p| p).await;
}

#[tokio::test(flavor = "current_thread")]
async fn secure_websocket_with_test_ca() {
    round_trip(WSS, Transport::Wss, with_ca).await;
}

#[tokio::test(flavor = "current_thread")]
async fn tls_without_the_ca_is_not_trusted() {
    let _limits = limits_lock();
    let mut net = Net::start(NetLimits::default());
    let text = connect_failed(&mut net, profile(TLS, Transport::Tls, &prefix())).await;
    assert_text(&text, &["trusted"]);
    let text = connect_failed(&mut net, profile(WSS, Transport::Wss, &prefix())).await;
    assert_text(&text, &["trusted"]);
    net.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn tls_expired_certificate() {
    let _limits = limits_lock();
    let mut net = Net::start(NetLimits::default());
    let p = with_ca(profile(EXPIRED, Transport::Tls, &prefix()));
    let text = connect_failed(&mut net, p).await;
    assert_text(&text, &["expired"]);
    net.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn tls_wrong_host_name() {
    let _limits = limits_lock();
    let mut net = Net::start(NetLimits::default());
    let p = with_ca(profile(WRONG_HOST, Transport::Tls, &prefix()));
    let text = connect_failed(&mut net, p).await;
    assert_text(&text, &["host name"]);
    net.shutdown().await;
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
async fn mtls_without_client_certificate_fails() {
    let _limits = limits_lock();
    let mut net = Net::start(NetLimits::default());
    let p = with_ca(profile(MTLS, Transport::Tls, &prefix()));
    let text = connect_failed(&mut net, p).await;
    assert!(
        text.to_lowercase().contains("client certificate"),
        "unexpected text: {text}"
    );
    net.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn mtls_mismatched_key_fails() {
    let _limits = limits_lock();
    let mut net = Net::start(NetLimits::default());
    let p = with_client_cert(
        with_ca(profile(MTLS, Transport::Tls, &prefix())),
        "client-mismatch.key",
    );
    let text = connect_failed(&mut net, p).await;
    assert_text(&text, &["do not match"]);
    net.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn disconnect_stops_incoming() {
    let _limits = limits_lock();
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

#[tokio::test(flavor = "current_thread")]
async fn broker_restart_reconnects_and_resubscribes() {
    let _limits = limits_lock();
    let mut net = Net::start(NetLimits::default());
    let prefix = prefix();
    let generation = connect(&mut net, profile(ANON, Transport::Tcp, &prefix)).await;
    wait_subscribed(&mut net, WAIT).await;

    let compose = broker_dir().join("compose.yml");
    let restart = tokio::task::spawn_blocking(move || {
        Process::new("docker")
            .args(["compose", "-f"])
            .arg(compose)
            .args(["restart", "-t", "1", "mosquitto"])
            .status()
            .expect("run docker")
    });
    let s = tokio::time::timeout(
        RESTART_WAIT,
        net.status
            .wait_for(|s| matches!(s.state, ConnState::Reconnecting { .. })),
    )
    .await
    .expect("timeout: no Reconnecting")
    .expect("status")
    .clone();
    assert_eq!(s.generation, generation);
    let s = tokio::time::timeout(
        RESTART_WAIT,
        net.status.wait_for(|s| s.state == ConnState::Connected),
    )
    .await
    .expect("timeout: no Connected")
    .expect("status")
    .clone();
    assert_eq!(s.generation, generation);
    assert!(restart.await.expect("join").success(), "docker restart");

    // The client subscribes again. A new message arrives.
    wait_subscribed(&mut net, RESTART_WAIT).await;
    publish_round_trip(&mut net, &prefix, generation).await;
    assert_eq!(net.status.borrow().generation, generation);
    net.shutdown().await;
}
