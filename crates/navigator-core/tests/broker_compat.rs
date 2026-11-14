//! Compatibility of `Net` with the Docker brokers in tests/broker: Mosquitto, EMQX and HiveMQ CE.
//! Each broker runs the same tests (anonymous TCP and WebSocket): connect, subscribe, a QoS 0 and
//! QoS 1 round trip with MQTT 5 user properties and a content type, retained delivery and clear,
//! a persistent session, and a broker restart. A probe test prints the CONNACK limits.
//!
//! Run: `tests/broker/up.sh && cargo test -p navigator-core --features broker-tests --test
//! broker_compat -- --test-threads=1 --nocapture && tests/broker/down.sh`.
#![cfg(feature = "broker-tests")]

use std::path::PathBuf;
use std::process::Command as Process;
use std::time::Duration;

use bytes::Bytes;
use navigator_core::model::{Incoming, MsgProps, Qos};
use navigator_core::net::{
    Command, ConnState, ConnectConfig, Event, Net, NetLimits, Outgoing, Profile, Protocol,
    PublishResult, Transport,
};

/// The restart test restarts the broker that the other tests of the same broker use. It takes the
/// write side; every other test takes the read side, so a restart never runs beside another test.
/// Nothing else needs serialization: each test has its own topic prefix and client IDs, and
/// `Net` has no process-global state.
static BROKER: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

const WAIT: Duration = Duration::from_secs(10);
/// A broker restart takes up to 20 s. The client waits 1 s or more before it retries.
const RESTART_WAIT: Duration = Duration::from_secs(60);

/// One broker: its compose service and its anonymous listeners.
#[derive(Clone, Copy)]
struct Broker {
    service: &'static str,
    tcp: u16,
    ws: u16,
}

const MOSQUITTO: Broker = Broker {
    service: "mosquitto",
    tcp: 18830,
    ws: 18834,
};
const EMQX: Broker = Broker {
    service: "emqx",
    tcp: 18840,
    ws: 18841,
};
const HIVEMQ: Broker = Broker {
    service: "hivemq",
    tcp: 18850,
    ws: 18851,
};

fn prefix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("test/compat/{}-{nanos}", std::process::id())
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

async fn connect(net: &mut Net, p: Profile) -> u64 {
    connect_with(net, p, Protocol::V5).await
}

async fn connect_with(net: &mut Net, p: Profile, protocol: Protocol) -> u64 {
    let previous = net.status.borrow().generation;
    let mut config = ConnectConfig::from(p);
    config.protocol = protocol;
    net.send(Command::Connect(Box::new(config))).expect("send");
    wait_connected(net, previous).await
}

async fn wait_connected(net: &mut Net, previous: u64) -> u64 {
    tokio::time::timeout(
        WAIT,
        net.status
            .wait_for(|s| s.generation > previous && s.state == ConnState::Connected),
    )
    .await
    .expect("timeout: not connected")
    .expect("status")
    .generation
}

async fn next_event(net: &mut Net, wait: Duration) -> Event {
    tokio::time::timeout(wait, net.events.recv())
        .await
        .expect("event timeout")
        .expect("events closed")
}

async fn next_incoming(net: &mut Net, wait: Duration) -> Incoming {
    let m = tokio::time::timeout(wait, net.incoming.recv())
        .await
        .expect("message timeout")
        .expect("closed");
    net.release(&m);
    m
}

/// Waits for the `Subscribed` event. Skips `Reconnected`.
async fn wait_subscribed(net: &mut Net, wait: Duration) {
    loop {
        match next_event(net, wait).await {
            Event::Subscribed { result, .. } => {
                assert_eq!(result, Ok(Qos::AtLeastOnce));
                return;
            }
            Event::Reconnected { .. }
            | Event::SessionFilters { .. }
            | Event::ConnectionDetails { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// Waits for the result of publish `op_id`. Skips events that are not a result.
async fn wait_published(net: &mut Net, op_id: u64) -> PublishResult {
    loop {
        match next_event(net, WAIT).await {
            Event::Published {
                op_id: id, result, ..
            } if id == op_id => return result,
            Event::Reconnected { .. }
            | Event::SessionFilters { .. }
            | Event::ConnectionDetails { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn out(op_id: u64, topic: &str, payload: &'static [u8], qos: Qos, retain: bool) -> Command {
    Command::Publish(Box::new(Outgoing {
        op_id,
        topic: topic.into(),
        payload: Bytes::from_static(payload),
        qos,
        retain,
        props: MsgProps::default(),
    }))
}

fn props() -> MsgProps {
    MsgProps {
        content_type: Some("application/json".into()),
        user: vec![
            ("source".into(), "navigator".into()),
            ("source".into(), "twice".into()),
            ("k2".into(), "v2".into()),
        ],
        payload_utf8: true,
        ..MsgProps::default()
    }
}

/// QoS 0 and QoS 1 messages with MQTT 5 properties must come back with the same properties.
async fn round_trip(net: &mut Net, prefix: &str, generation: u64) {
    for (id, qos, expect) in [
        (1, Qos::AtMostOnce, PublishResult::Sent),
        (2, Qos::AtLeastOnce, PublishResult::Acknowledged),
    ] {
        let topic = format!("{prefix}/x{id}");
        net.send(Command::Publish(Box::new(Outgoing {
            op_id: id,
            topic: topic.clone(),
            payload: Bytes::from_static(br#"{"a":1}"#),
            qos,
            retain: false,
            props: props(),
        })))
        .expect("send");
        let m = next_incoming(net, WAIT).await;
        assert_eq!(&*m.topic, topic);
        assert_eq!(&m.payload[..], br#"{"a":1}"#);
        assert_eq!(m.generation, generation);
        assert!(!m.retain);
        assert_eq!(m.props.content_type.as_deref(), Some("application/json"));
        assert!(m.props.payload_utf8, "payload format indicator");
        // The user properties keep their order and their duplicates.
        assert_eq!(m.props.user, props().user);
        assert_eq!(wait_published(net, id).await, expect);
    }
}

async fn tcp_and_ws_round_trip(b: Broker) {
    for (port, transport) in [(b.tcp, Transport::Tcp), (b.ws, Transport::Ws)] {
        let mut net = Net::start(NetLimits::default());
        let prefix = prefix();
        let generation = connect(&mut net, profile(port, transport, &prefix)).await;
        wait_subscribed(&mut net, WAIT).await;
        round_trip(&mut net, &prefix, generation).await;
        net.shutdown().await;
    }
}

/// MQTT 3.1.1: QoS 0 and QoS 1 round trip on TCP and WebSocket. There are no properties.
async fn v3_tcp_and_ws_round_trip(b: Broker) {
    for (port, transport) in [(b.tcp, Transport::Tcp), (b.ws, Transport::Ws)] {
        let mut net = Net::start(NetLimits::default());
        let prefix = prefix();
        let generation =
            connect_with(&mut net, profile(port, transport, &prefix), Protocol::V311).await;
        wait_subscribed(&mut net, WAIT).await;
        for (id, qos, expect) in [
            (1, Qos::AtMostOnce, PublishResult::Sent),
            (2, Qos::AtLeastOnce, PublishResult::Acknowledged),
        ] {
            let topic = format!("{prefix}/x{id}");
            net.send(out(id, &topic, b"v3", qos, false)).expect("send");
            let m = next_incoming(&mut net, WAIT).await;
            assert_eq!((&*m.topic, &m.payload[..]), (&*topic, &b"v3"[..]));
            assert_eq!(m.generation, generation);
            assert_eq!(m.props, MsgProps::default());
            assert_eq!(wait_published(&mut net, id).await, expect);
        }
        net.shutdown().await;
    }
}

/// A retained message reaches a new subscriber with the retain flag. An empty retained message
/// clears it.
async fn retained(b: Broker) {
    retained_with(b, Protocol::V5).await;
}

async fn retained_with(b: Broker, protocol: Protocol) {
    let prefix = prefix();
    let topic = format!("{prefix}/r");
    let mut publisher = Net::start(NetLimits::default());
    let mut p = profile(b.tcp, Transport::Tcp, &prefix);
    p.subscriptions.clear();
    connect_with(&mut publisher, p, protocol).await;
    publisher
        .send(out(1, &topic, b"kept", Qos::AtLeastOnce, true))
        .expect("send");
    assert_eq!(
        wait_published(&mut publisher, 1).await,
        PublishResult::Acknowledged
    );

    let mut late = Net::start(NetLimits::default());
    let generation =
        connect_with(&mut late, profile(b.tcp, Transport::Tcp, &prefix), protocol).await;
    wait_subscribed(&mut late, WAIT).await;
    let m = next_incoming(&mut late, WAIT).await;
    assert_eq!(
        (&*m.topic, &m.payload[..], m.retain),
        (&*topic, &b"kept"[..], true)
    );
    assert_eq!(m.generation, generation);
    late.shutdown().await;

    publisher
        .send(out(2, &topic, b"", Qos::AtLeastOnce, true))
        .expect("send");
    assert_eq!(
        wait_published(&mut publisher, 2).await,
        PublishResult::Acknowledged
    );
    publisher.shutdown().await;

    let mut after = Net::start(NetLimits::default());
    connect_with(
        &mut after,
        profile(b.tcp, Transport::Tcp, &prefix),
        protocol,
    )
    .await;
    wait_subscribed(&mut after, WAIT).await;
    let nothing = tokio::time::timeout(Duration::from_millis(1500), after.incoming.recv()).await;
    assert!(
        nothing.is_err(),
        "retained message after the clear: {nothing:?}"
    );
    after.shutdown().await;
}

/// A persistent session (clean start off, expiry 60 s) keeps a QoS 1 message while the client is
/// away.
async fn persistent_session(b: Broker) {
    persistent_session_with(b, Protocol::V5).await;
}

/// MQTT 3.1.1: Clean Session off is the persistent session. The profile expiry does not matter.
async fn persistent_session_with(b: Broker, protocol: Protocol) {
    let prefix = prefix();
    let mut p = profile(b.tcp, Transport::Tcp, &prefix);
    p.clean_start = false;
    p.session_expiry_secs = 60;

    let mut first = Net::start(NetLimits::default());
    connect_with(&mut first, p.clone(), protocol).await;
    wait_subscribed(&mut first, WAIT).await;
    first.shutdown().await;

    let mut other = Net::start(NetLimits::default());
    let mut q = profile(b.tcp, Transport::Tcp, &prefix);
    q.subscriptions.clear();
    connect_with(&mut other, q, protocol).await;
    other
        .send(out(
            1,
            &format!("{prefix}/offline"),
            b"queued",
            Qos::AtLeastOnce,
            false,
        ))
        .expect("send");
    assert_eq!(
        wait_published(&mut other, 1).await,
        PublishResult::Acknowledged
    );
    other.shutdown().await;

    let mut again = Net::start(NetLimits::default());
    connect_with(&mut again, p, protocol).await;
    let m = next_incoming(&mut again, WAIT).await;
    assert_eq!(&m.payload[..], b"queued");
    again.shutdown().await;
}

/// After a restart the session goes back to `Connected` with the same generation, subscribes
/// again, and messages flow.
async fn restart_reconnects(b: Broker) {
    let _broker = BROKER.write().await;
    let mut net = Net::start(NetLimits::default());
    let prefix = prefix();
    let generation = connect(&mut net, profile(b.tcp, Transport::Tcp, &prefix)).await;
    wait_subscribed(&mut net, WAIT).await;

    let compose = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/broker/compose.yml");
    let restart = tokio::task::spawn_blocking(move || {
        Process::new("docker")
            .args(["compose", "-f"])
            .arg(compose)
            .args(["restart", "-t", "1", b.service])
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
    wait_subscribed(&mut net, RESTART_WAIT).await;
    round_trip(&mut net, &prefix, generation).await;
    assert_eq!(net.status.borrow().generation, generation);
    net.shutdown().await;
}

/// Prints what the broker says in CONNACK, for a client that asks for 600 s keep alive and a
/// session expiry of 7 days. Checks only what MQTT 5 requires of every broker.
async fn connack_probe(b: Broker) {
    let mut net = Net::start(NetLimits::default());
    let mut p = profile(b.tcp, Transport::Tcp, &prefix());
    p.keepalive_secs = 600;
    p.session_expiry_secs = 7 * 86_400;
    connect(&mut net, p).await;
    let details = loop {
        match next_event(&mut net, WAIT).await {
            Event::ConnectionDetails { details, .. } => break details,
            Event::Subscribed { .. } | Event::SessionFilters { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    };
    let v5 = details
        .v5
        .as_ref()
        .expect("an MQTT 5 connection has v5 details");
    println!(
        "compat {}: session_present {} keep_alive {} server_keep_alive {:?} receive_max {} \
         max_qos {} retain {} max_packet {:?} topic_alias_max {} wildcard {} sub_ids {} \
         shared {} assigned_id {}",
        b.service,
        details.session_present,
        v5.keep_alive_secs,
        v5.server_keep_alive_secs,
        v5.receive_max,
        v5.max_qos,
        v5.retain_available,
        v5.max_packet_size,
        v5.topic_alias_max,
        v5.wildcard_subscriptions,
        v5.subscription_ids,
        v5.shared_subscriptions,
        v5.assigned_client_id.is_some(),
    );
    assert_eq!(details.protocol, Protocol::V5);
    assert!(!details.session_present, "clean start gives a new session");
    assert!(
        v5.retain_available,
        "all three brokers keep retained messages"
    );
    assert!(v5.wildcard_subscriptions);
    println!(
        "compat {}: Net::broker_max_packet {:?}",
        b.service,
        net.broker_max_packet()
    );
    net.shutdown().await;
}

macro_rules! broker_tests {
    ($mod:ident, $broker:expr) => {
        mod $mod {
            use super::*;

            #[tokio::test(flavor = "current_thread")]
            async fn connack_probe() {
                let _broker = BROKER.read().await;
                super::connack_probe($broker).await;
            }

            #[tokio::test(flavor = "current_thread")]
            async fn tcp_and_websocket_round_trip_with_properties() {
                let _broker = BROKER.read().await;
                tcp_and_ws_round_trip($broker).await;
            }

            #[tokio::test(flavor = "current_thread")]
            async fn retained_delivery_and_clear() {
                let _broker = BROKER.read().await;
                retained($broker).await;
            }

            #[tokio::test(flavor = "current_thread")]
            async fn persistent_session_keeps_queued_message() {
                let _broker = BROKER.read().await;
                persistent_session($broker).await;
            }

            #[tokio::test(flavor = "current_thread")]
            async fn v3_tcp_and_websocket_round_trip() {
                let _broker = BROKER.read().await;
                v3_tcp_and_ws_round_trip($broker).await;
            }

            #[tokio::test(flavor = "current_thread")]
            async fn v3_retained_delivery_and_clear() {
                let _broker = BROKER.read().await;
                retained_with($broker, Protocol::V311).await;
            }

            #[tokio::test(flavor = "current_thread")]
            async fn v3_persistent_session_keeps_queued_message() {
                let _broker = BROKER.read().await;
                persistent_session_with($broker, Protocol::V311).await;
            }

            #[tokio::test(flavor = "current_thread")]
            async fn restart_reconnects_and_resubscribes() {
                restart_reconnects($broker).await;
            }
        }
    };
}

broker_tests!(mosquitto, MOSQUITTO);
broker_tests!(emqx, EMQX);
broker_tests!(hivemq, HIVEMQ);
