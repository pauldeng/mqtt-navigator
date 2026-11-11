//! Compatibility of `Net` with the Docker brokers in tests/broker: Mosquitto, EMQX and HiveMQ CE.
//! Each broker runs the same tests (anonymous TCP and WebSocket): connect, subscribe, a QoS 0 and
//! QoS 1 round trip with MQTT 5 user properties and a content type, retained delivery and clear,
//! a persistent session, and a broker restart. A probe test prints the CONNACK limits.
//!
//! Run: `tests/broker/up.sh && cargo test -p navigator-core --features broker-tests --test
//! broker_compat -- --test-threads=1 --nocapture && tests/broker/down.sh`.
#![cfg(feature = "broker-tests")]
// Each test holds `LIMITS` for its whole run. The guard must live across the awaits.
#![allow(clippy::await_holding_lock)]

use std::num::NonZeroU16;
use std::path::PathBuf;
use std::process::Command as Process;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use ms_mqtt_client::client::{ClientOptions, ConnectResult, KeepAliveConfig, new_client};
use ms_mqtt_client::packet::{ConnectProperties, SessionExpiryInterval};
use ms_mqtt_client::transport::{ConnectionTransportConfig, ConnectionTransportType};
use navigator_core::model::{Incoming, MsgProps, Qos};
use navigator_core::net::{
    Command, ConnState, Event, Net, NetLimits, Outgoing, Profile, PublishResult, Transport,
};

const WAIT: Duration = Duration::from_secs(10);
/// A broker restart takes up to 20 s. The client waits 1 s or more before it retries.
const RESTART_WAIT: Duration = Duration::from_secs(60);

/// Every `Net` sets the process-global `ms_mqtt_client::local_limits`. Each test takes this lock.
static LIMITS: Mutex<()> = Mutex::new(());

fn limits_lock() -> std::sync::MutexGuard<'static, ()> {
    LIMITS.lock().unwrap_or_else(|e| e.into_inner())
}

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
        ..Profile::default()
    }
}

async fn connect(net: &mut Net, p: Profile) -> u64 {
    let previous = net.status.borrow().generation;
    net.send(Command::Connect(Box::new(p.into())))
        .expect("send");
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
            Event::Reconnected { .. } | Event::SessionFilters { .. } => {}
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
            Event::Reconnected { .. } | Event::SessionFilters { .. } => {}
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
    let _limits = limits_lock();
    for (port, transport) in [(b.tcp, Transport::Tcp), (b.ws, Transport::Ws)] {
        let mut net = Net::start(NetLimits::default());
        let prefix = prefix();
        let generation = connect(&mut net, profile(port, transport, &prefix)).await;
        wait_subscribed(&mut net, WAIT).await;
        round_trip(&mut net, &prefix, generation).await;
        net.shutdown().await;
    }
}

/// A retained message reaches a new subscriber with the retain flag. An empty retained message
/// clears it.
async fn retained(b: Broker) {
    let _limits = limits_lock();
    let prefix = prefix();
    let topic = format!("{prefix}/r");
    let mut publisher = Net::start(NetLimits::default());
    let mut p = profile(b.tcp, Transport::Tcp, &prefix);
    p.subscriptions.clear();
    connect(&mut publisher, p).await;
    publisher
        .send(out(1, &topic, b"kept", Qos::AtLeastOnce, true))
        .expect("send");
    assert_eq!(
        wait_published(&mut publisher, 1).await,
        PublishResult::Acknowledged
    );

    let mut late = Net::start(NetLimits::default());
    let generation = connect(&mut late, profile(b.tcp, Transport::Tcp, &prefix)).await;
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
    connect(&mut after, profile(b.tcp, Transport::Tcp, &prefix)).await;
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
    let _limits = limits_lock();
    let prefix = prefix();
    let mut p = profile(b.tcp, Transport::Tcp, &prefix);
    p.clean_start = false;
    p.session_expiry_secs = 60;

    let mut first = Net::start(NetLimits::default());
    connect(&mut first, p.clone()).await;
    wait_subscribed(&mut first, WAIT).await;
    first.shutdown().await;

    let mut other = Net::start(NetLimits::default());
    let mut q = profile(b.tcp, Transport::Tcp, &prefix);
    q.subscriptions.clear();
    connect(&mut other, q).await;
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
    connect(&mut again, p).await;
    let m = next_incoming(&mut again, WAIT).await;
    assert_eq!(&m.payload[..], b"queued");
    again.shutdown().await;
}

/// After a restart the session goes back to `Connected` with the same generation, subscribes
/// again, and messages flow.
async fn restart_reconnects(b: Broker) {
    let _limits = limits_lock();
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
    let _limits = limits_lock();
    let (_client, connect_handle, _receiver) = new_client(ClientOptions::default());
    let ConnectResult::Success(_connection, connack, _handle) = connect_handle
        .connect(
            ConnectionTransportConfig {
                transport_type: ConnectionTransportType::Tcp {
                    hostname: "127.0.0.1".into(),
                    port: b.tcp,
                },
                timeout: Some(WAIT),
                proxy: None,
                tcp_nodelay: true,
            },
            true,
            KeepAliveConfig::Duration {
                ping_after: NonZeroU16::new(600).expect("non-zero"),
                response_timeout: Duration::from_secs(10),
            },
            None,
            None,
            None,
            ConnectProperties {
                session_expiry_interval: SessionExpiryInterval::Duration(7 * 86_400),
                ..ConnectProperties::default()
            },
            Some(WAIT),
        )
        .await
    else {
        panic!("{}: connect failed", b.service);
    };
    let p = &connack.properties;
    println!(
        "compat {}: session_present {} session_expiry {:?} receive_max {} max_qos {:?} \
         retain {} max_packet {} topic_alias_max {} wildcard {} sub_ids {} shared {} \
         server_keep_alive {:?} assigned_id {} user_props {:?}",
        b.service,
        connack.session_present,
        p.session_expiry_interval,
        p.receive_maximum,
        p.maximum_qos,
        p.retain_available,
        p.maximum_packet_size,
        p.topic_alias_maximum,
        p.wildcard_subscription_available,
        p.subscription_identifiers_available,
        p.shared_subscription_available,
        p.server_keep_alive,
        p.assigned_client_identifier.is_some(),
        p.user_properties,
    );
    assert!(connack.is_success());
    assert!(!connack.session_present, "clean start gives a new session");
    assert!(
        p.retain_available,
        "all three brokers keep retained messages"
    );
    assert!(p.wildcard_subscription_available);

    let mut net = Net::start(NetLimits::default());
    connect(&mut net, profile(b.tcp, Transport::Tcp, &prefix())).await;
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
                super::connack_probe($broker).await;
            }

            #[tokio::test(flavor = "current_thread")]
            async fn tcp_and_websocket_round_trip_with_properties() {
                tcp_and_ws_round_trip($broker).await;
            }

            #[tokio::test(flavor = "current_thread")]
            async fn retained_delivery_and_clear() {
                retained($broker).await;
            }

            #[tokio::test(flavor = "current_thread")]
            async fn persistent_session_keeps_queued_message() {
                persistent_session($broker).await;
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
