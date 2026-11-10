//! Integration tests against a local broker at 127.0.0.1:1883 (plain TCP, anonymous).
//! Each test returns early if the broker is not reachable.

// Each test holds `LIMITS` for its whole run. The guard must live across the awaits.
#![allow(clippy::await_holding_lock)]

use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use ms_mqtt_client::client::{ClientOptions, ConnectResult, KeepAliveConfig, new_client};
use ms_mqtt_client::packet::{ConnectProperties, PublishProperties};
use ms_mqtt_client::topic::TopicName;
use ms_mqtt_client::transport::{ConnectionTransportConfig, ConnectionTransportType};
use navigator_core::model::{Incoming, MsgProps, Qos};
use navigator_core::net::{
    Command, ConnState, Event, Net, NetLimits, Outgoing, Profile, PublishResult, Transport,
    queue_cost,
};

const WAIT: Duration = Duration::from_secs(10);

/// Every `Net` sets the process-global `ms_mqtt_client::local_limits`. Tests run in parallel by
/// default, so each test takes this lock first.
static LIMITS: Mutex<()> = Mutex::new(());

fn limits_lock() -> std::sync::MutexGuard<'static, ()> {
    LIMITS.lock().unwrap_or_else(|e| e.into_inner())
}

fn broker_up() -> bool {
    let up = std::net::TcpStream::connect_timeout(
        &"127.0.0.1:1883".parse().expect("addr"),
        Duration::from_secs(1),
    )
    .is_ok();
    if !up {
        eprintln!("skipped: no broker at 127.0.0.1:1883");
    }
    up
}

/// A topic prefix that no other run uses.
fn prefix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("test/nav/{}-{nanos}", std::process::id())
}

fn profile(subs: &[&str]) -> Profile {
    Profile {
        host: "127.0.0.1".into(),
        port: 1883,
        transport: Transport::Tcp,
        subscriptions: subs
            .iter()
            .map(|s| ((*s).to_owned(), Qos::AtLeastOnce))
            .collect(),
        ..Profile::default()
    }
}

async fn connect(net: &mut Net, p: Profile) -> u64 {
    let previous = net.status.borrow().generation;
    net.send(Command::Connect(Box::new(p))).expect("send");
    let s = tokio::time::timeout(
        WAIT,
        net.status
            .wait_for(|s| s.generation > previous && s.state == ConnState::Connected),
    )
    .await
    .expect("timeout")
    .expect("status");
    s.generation
}

/// Waits for a state and returns its generation.
async fn wait_state(net: &mut Net, f: impl Fn(&ConnState) -> bool) -> u64 {
    let s = tokio::time::timeout(WAIT, net.status.wait_for(|s| f(&s.state)))
        .await
        .expect("timeout")
        .expect("status");
    s.generation
}

async fn next_event(net: &mut Net) -> Event {
    tokio::time::timeout(WAIT, net.events.recv())
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

/// Sends a command. Waits while the command queue is full.
async fn send_wait(net: &Net, cmd: Command) {
    loop {
        match net.send(cmd.clone()) {
            Ok(()) => return,
            Err(navigator_core::net::SendError::Full) => {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Err(e) => panic!("{e}"),
        }
    }
}

fn out(op_id: u64, topic: &str, payload: &'static [u8], qos: Qos) -> Command {
    Command::Publish(Box::new(Outgoing {
        op_id,
        topic: topic.into(),
        payload: Bytes::from_static(payload),
        qos,
        retain: false,
        props: MsgProps {
            content_type: Some("text/plain".into()),
            user: vec![("k".into(), "v".into())],
            ..MsgProps::default()
        },
    }))
}

#[tokio::test(flavor = "current_thread")]
async fn subscribe_publish_receive_and_disconnect() {
    let _limits = limits_lock();
    if !broker_up() {
        return;
    }
    let mut net = Net::start(NetLimits::default());
    let p = prefix();
    let gen1 = connect(&mut net, profile(&[&format!("{p}/#"), "a/#/b"])).await;
    assert_eq!(gen1, 1);

    // Two initial subscriptions: one valid, one invalid. Order of the events is not fixed.
    let mut results = Vec::new();
    for _ in 0..2 {
        match next_event(&mut net).await {
            Event::Subscribed {
                generation,
                filter,
                result,
            } => {
                assert_eq!(generation, gen1);
                results.push((filter, result));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    let ok = results
        .iter()
        .find(|(f, _)| f.starts_with("test/"))
        .expect("valid filter");
    assert_eq!(ok.1, Ok(Qos::AtLeastOnce));
    let bad = results
        .iter()
        .find(|(f, _)| f == "a/#/b")
        .expect("invalid filter");
    assert!(
        bad.1
            .as_ref()
            .is_err_and(|e| e.starts_with("Invalid topic filter")),
        "{bad:?}"
    );

    for (id, qos, expect) in [
        (1, Qos::AtMostOnce, PublishResult::Sent),
        (2, Qos::AtLeastOnce, PublishResult::Acknowledged),
    ] {
        let topic = format!("{p}/x{id}");
        net.send(out(id, &topic, b"hello", qos)).expect("send");
        // The PUBACK and the message can arrive in any order.
        let m = next_incoming(&mut net).await;
        assert_eq!(&*m.topic, topic);
        assert_eq!(&m.payload[..], b"hello");
        assert_eq!(m.generation, gen1);
        assert_eq!(m.props.content_type.as_deref(), Some("text/plain"));
        assert_eq!(m.props.user, vec![("k".to_owned(), "v".to_owned())]);
        match next_event(&mut net).await {
            Event::Published { op_id, result, .. } => assert_eq!((op_id, result), (id, expect)),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(net.queued_bytes(), 0);

    // Unsubscribe works.
    net.send(Command::Unsubscribe {
        filter: format!("{p}/#"),
    })
    .expect("send");
    assert!(matches!(
        next_event(&mut net).await,
        Event::Unsubscribed { .. }
    ));

    net.disconnect();
    let ended = wait_state(&mut net, |s| s == &ConnState::Disconnected).await;
    assert_eq!(ended, gen1);
    while net.incoming.try_recv().is_ok() {}
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        net.incoming.try_recv().is_err(),
        "no message after disconnect"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reconnect_increments_generation() {
    let _limits = limits_lock();
    if !broker_up() {
        return;
    }
    let mut net = Net::start(NetLimits::default());
    assert_eq!(connect(&mut net, profile(&[])).await, 1);
    // A second Connect while connected stops the old session first.
    assert_eq!(connect(&mut net, profile(&[])).await, 2);
    net.disconnect();
    wait_state(&mut net, |s| s == &ConnState::Disconnected).await;
    assert_eq!(connect(&mut net, profile(&[])).await, 3);
}

/// A refused connection is a temporary error: the net thread retries until the user stops it.
#[tokio::test(flavor = "current_thread")]
async fn connection_refused_is_retried_until_disconnect() {
    let _limits = limits_lock();
    let mut net = Net::start(NetLimits::default());
    let mut p = profile(&[]);
    p.port = 1; // nothing listens here
    net.send(Command::Connect(Box::new(p))).expect("send");
    wait_state(&mut net, |s| matches!(s, ConnState::Reconnecting { .. })).await;
    let s = net.status.borrow().clone();
    assert!(
        matches!(&s.state, ConnState::Reconnecting { attempt: 1, reason } if reason.contains("refused")),
        "{s:?}"
    );
    net.disconnect();
    let generation = wait_state(&mut net, |s| s == &ConnState::Disconnected).await;
    assert_eq!(generation, s.generation);
}

/// A second client that publishes without Net.
async fn publish_plain(topic: &str, payload: Vec<u8>) {
    let (client, handle, _rx) = new_client(ClientOptions::default());
    let ConnectResult::Success(conn, _, _dh) = handle
        .connect(
            ConnectionTransportConfig {
                transport_type: ConnectionTransportType::Tcp {
                    hostname: "127.0.0.1".into(),
                    port: 1883,
                },
                timeout: None,
                proxy: None,
                tcp_nodelay: true,
            },
            true,
            KeepAliveConfig::Infinite,
            None,
            None,
            None,
            ConnectProperties::default(),
            None,
        )
        .await
    else {
        panic!("second client cannot connect");
    };
    let work = async {
        let t = client
            .publish_qos1(
                TopicName::new(topic).expect("topic"),
                Bytes::from(payload),
                false,
                PublishProperties::default(),
            )
            .await
            .expect("publish");
        t.await.expect("puback");
    };
    tokio::select! { _ = conn.run_until_disconnect() => panic!("ended"), () = work => {} }
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_packet_is_not_delivered() {
    let _limits = limits_lock();
    if !broker_up() {
        return;
    }
    let limits = NetLimits {
        max_packet: 1024,
        ..NetLimits::default()
    };
    let mut net = Net::start(limits);
    let p = prefix();
    connect(&mut net, profile(&[&format!("{p}/#")])).await;
    assert!(matches!(
        next_event(&mut net).await,
        Event::Subscribed { result: Ok(_), .. }
    ));

    publish_plain(&format!("{p}/big"), vec![b'x'; 4096]).await;
    publish_plain(&format!("{p}/small"), b"ok".to_vec()).await;

    // A compliant broker (mosquitto) does not send packets above the advertised maximum.
    // A non-compliant one makes the client end the session. Both are fine. A 4096 byte message
    // must never reach the UI.
    let mut got_small = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Some(m)) = tokio::time::timeout_at(deadline, net.incoming.recv()).await {
        assert!(m.payload.len() <= 1024, "oversized payload delivered");
        got_small |= *m.topic == *format!("{p}/small");
    }
    let state = net.status.borrow().state.clone();
    assert!(
        got_small || matches!(state, ConnState::Failed(_)),
        "state {state:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn full_queue_drops_and_bounds_memory() {
    let _limits = limits_lock();
    if !broker_up() {
        return;
    }
    let limits = NetLimits {
        queue_len: 4,
        ..NetLimits::default()
    };
    let mut net = Net::start(limits);
    let p = prefix();
    connect(&mut net, profile(&[&format!("{p}/#")])).await;
    assert!(matches!(
        next_event(&mut net).await,
        Event::Subscribed { result: Ok(_), .. }
    ));

    for i in 0..100 {
        send_wait(
            &net,
            out(i, &format!("{p}/m"), b"0123456789", Qos::AtMostOnce),
        )
        .await;
    }
    // Wait until all 100 were seen: 4 queued + the rest dropped.
    let deadline = tokio::time::Instant::now() + WAIT;
    while net.stats.drops.load(Ordering::Relaxed) < 96 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(net.stats.drops.load(Ordering::Relaxed) > 0);
    // The counter equals the cost of what is in the queue.
    let queued = net.queued_bytes();
    let (mut n, mut cost) = (0, 0);
    while let Ok(m) = net.incoming.try_recv() {
        n += 1;
        cost += queue_cost(&m);
        net.release(&m);
    }
    assert!(n <= 4, "got {n}");
    assert_eq!(queued, cost);
    assert_eq!(net.queued_bytes(), 0);
}

/// A fake broker that ignores the advertised maximum packet size, like a hostile one.
#[tokio::test(flavor = "current_thread")]
async fn hostile_broker_oversized_packet_fails_session() {
    let _limits = limits_lock();
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 512];
        let _ = sock.read(&mut buf); // CONNECT
        let _ = sock.write_all(&[0x20, 0x03, 0x00, 0x00, 0x00]); // CONNACK success
        // PUBLISH QoS 0, topic "t", 8000 payload bytes (remaining length 8003 as a varint).
        let mut packet = vec![0x30, 0xC3, 0x3E, 0x00, 0x01, b't', 0x00];
        packet.extend(vec![b'x'; 8000]);
        let _ = sock.write_all(&packet);
        std::thread::sleep(Duration::from_millis(500));
    });
    let mut net = Net::start(NetLimits {
        max_packet: 1024,
        ..NetLimits::default()
    });
    let mut p = profile(&[]);
    p.port = port;
    net.send(Command::Connect(Box::new(p))).expect("send");
    wait_state(&mut net, |s| matches!(s, ConnState::Failed(_))).await;
    let state = net.status.borrow().state.clone();
    assert!(
        matches!(&state, ConnState::Failed(t) if t.contains("larger")),
        "{state:?}"
    );
    assert!(net.incoming.try_recv().is_err());
    server.join().expect("server");
}

/// Messages with an empty payload and many user properties must count against the byte budget.
#[tokio::test(flavor = "current_thread")]
async fn property_heavy_messages_are_bounded_and_budget_recovers() {
    let _limits = limits_lock();
    if !broker_up() {
        return;
    }
    let mut net = Net::start(NetLimits {
        queue_bytes: 8 * 1024,
        ..NetLimits::default()
    });
    let p = prefix();
    connect(&mut net, profile(&[&format!("{p}/#")])).await;
    assert!(matches!(
        next_event(&mut net).await,
        Event::Subscribed { result: Ok(_), .. }
    ));

    let heavy = |op_id| {
        Command::Publish(Box::new(Outgoing {
            op_id,
            topic: format!("{p}/heavy"),
            payload: Bytes::new(),
            qos: Qos::AtMostOnce,
            retain: false,
            props: MsgProps {
                user: (0..20)
                    .map(|i| (format!("key{i}"), "v".repeat(100)))
                    .collect(),
                ..MsgProps::default()
            },
        }))
    };
    for i in 0..40 {
        send_wait(&net, heavy(i)).await;
    }
    let deadline = tokio::time::Instant::now() + WAIT;
    while net.stats.drops.load(Ordering::Relaxed) == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(net.stats.drops.load(Ordering::Relaxed) > 0, "no drops");
    assert!(
        net.queued_bytes() <= 8 * 1024,
        "queued {}",
        net.queued_bytes()
    );
    assert!(net.queued_bytes() > 0);

    // The UI drains. The budget returns to 0 because release frees what was charged.
    let mut n = 0;
    while let Ok(m) = net.incoming.try_recv() {
        assert!(m.payload.is_empty());
        assert!(queue_cost(&m) > 2000);
        net.release(&m);
        n += 1;
    }
    assert!((1..40).contains(&n), "got {n}");
    assert_eq!(net.queued_bytes(), 0);

    // New messages are admitted again.
    net.send(out(100, &format!("{p}/after"), b"ok", Qos::AtMostOnce))
        .expect("send");
    loop {
        let m = next_incoming(&mut net).await;
        if *m.topic == *format!("{p}/after") {
            break;
        }
    }
    assert_eq!(net.queued_bytes(), 0);
}

/// A fake broker that accepts `n` connections, answers CONNECT and then never answers again.
fn silent_broker_n(n: usize) -> (u16, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let handlers: Vec<_> = (0..n)
            .map(|_| {
                let (mut sock, _) = listener.accept().expect("accept");
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf); // CONNECT
                    let _ = sock.write_all(&[0x20, 0x03, 0x00, 0x00, 0x00]); // CONNACK success
                    sock.set_read_timeout(Some(Duration::from_secs(8)))
                        .expect("timeout");
                    while sock.read(&mut buf).is_ok_and(|n| n > 0) {} // read and ignore
                })
            })
            .collect();
        for h in handlers {
            h.join().expect("handler");
        }
    });
    (port, server)
}

/// A fake broker that accepts CONNECT and then never sends a PUBACK.
fn silent_broker() -> (u16, std::thread::JoinHandle<()>) {
    silent_broker_n(1)
}

#[tokio::test(flavor = "current_thread")]
async fn pending_operations_are_capped() {
    let _limits = limits_lock();
    let (port, server) = silent_broker();
    let mut net = Net::start(NetLimits::default());
    let mut p = profile(&[]);
    p.port = port;
    connect(&mut net, p).await;

    // 400 QoS 1 publishes that the broker never acknowledges. At most 256 can wait.
    let mut texts = Vec::new();
    for i in 0..400u64 {
        send_wait(&net, out(i, "t/x", b"x", Qos::AtLeastOnce)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while texts.len() < 400 - 256 {
        match tokio::time::timeout_at(deadline, net.events.recv()).await {
            Ok(Some(Event::Published {
                result: PublishResult::Rejected(t),
                ..
            })) => texts.push(t),
            other => panic!("unexpected {other:?} after {} rejections", texts.len()),
        }
    }
    assert!(
        texts
            .iter()
            .any(|t| t == "Too many messages are waiting. Try again."),
        "{texts:?}"
    );

    net.shutdown().await;
    server.join().expect("server");
}

/// `shutdown` resolves after the clean disconnect. `drop` does not wait.
#[tokio::test(flavor = "current_thread")]
async fn shutdown_resolves_and_drop_does_not_wait() {
    let _limits = limits_lock();
    let (port, server) = silent_broker();
    let mut net = Net::start(NetLimits::default());
    let mut p = profile(&[]);
    p.port = port;
    connect(&mut net, p).await;
    let done = net.shutdown();
    tokio::time::timeout(WAIT, done).await.expect("shutdown");
    assert_eq!(net.status.borrow().state, ConnState::Disconnected);
    let started = std::time::Instant::now();
    drop(net);
    assert!(started.elapsed() < Duration::from_millis(100));
    server.join().expect("server");
}

/// Disconnect and Connect, sent back to back. The net thread can read the Connect before it sees
/// the cancel. The cancel must not stop the new session. The order is random, so repeat.
#[tokio::test(flavor = "current_thread")]
async fn disconnect_then_connect_keeps_the_new_session() {
    let _limits = limits_lock();
    const ROUNDS: usize = 10;
    let (port, server) = silent_broker_n(ROUNDS + 1);
    let mut net = Net::start(NetLimits::default());
    let mut p = profile(&[]);
    p.port = port;
    let mut generation = connect(&mut net, p.clone()).await;
    for _ in 0..ROUNDS {
        net.disconnect();
        net.send(Command::Connect(Box::new(p.clone())))
            .expect("send");
        let s = tokio::time::timeout(
            WAIT,
            net.status
                .wait_for(|s| s.generation > generation && s.state == ConnState::Connected),
        )
        .await
        .expect("the new session was stopped")
        .expect("status")
        .clone();
        generation = s.generation;
        // The cancel must not stop it later either.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(net.status.borrow().state, ConnState::Connected);
    }
    net.shutdown().await;
    server.join().expect("server");
}

/// A fake broker that answers the first UNSUBSCRIBE with one reason code.
fn unsuback_broker(reason: u8) -> (u16, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf); // CONNECT
        let _ = sock.write_all(&[0x20, 0x03, 0x00, 0x00, 0x00]); // CONNACK success
        sock.set_read_timeout(Some(Duration::from_secs(8)))
            .expect("timeout");
        let n = sock.read(&mut buf).expect("UNSUBSCRIBE");
        assert!(
            n >= 4 && buf[0] == 0xA2,
            "not an UNSUBSCRIBE: {:?}",
            &buf[..n]
        );
        // UNSUBACK: packet identifier, no properties, one reason code.
        let _ = sock.write_all(&[0xB0, 0x04, buf[2], buf[3], 0x00, reason]);
        while sock.read(&mut buf).is_ok_and(|n| n > 0) {}
    });
    (port, server)
}

#[tokio::test(flavor = "current_thread")]
async fn unsuback_reason_is_checked() {
    let _limits = limits_lock();
    for (reason, expect_ok) in [(0x00, true), (0x11, true), (0x87, false)] {
        let (port, server) = unsuback_broker(reason);
        let mut net = Net::start(NetLimits::default());
        let mut p = profile(&[]);
        p.port = port;
        connect(&mut net, p).await;
        net.send(Command::Unsubscribe {
            filter: "a/b".into(),
        })
        .expect("send");
        match (next_event(&mut net).await, expect_ok) {
            (Event::Unsubscribed { filter, result, .. }, true) => {
                assert_eq!(filter, "a/b");
                assert_eq!(result, Ok(()));
            }
            (Event::Unsubscribed { filter, result, .. }, false) => {
                assert_eq!(filter, "a/b");
                assert_eq!(
                    result,
                    Err("Unsubscribe rejected: NotAuthorized".to_string())
                );
            }
            (other, _) => panic!("reason {reason:#x}: unexpected {other:?}"),
        }
        net.shutdown().await;
        server.join().expect("server");
    }
}
