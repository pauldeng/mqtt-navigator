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
    Command, ConnState, ConnectConfig, Event, Net, NetLimits, Outgoing, Profile, PublishResult,
    Transport, queue_cost,
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

async fn connect(net: &mut Net, p: impl Into<ConnectConfig>) -> u64 {
    let previous = net.status.borrow().generation;
    net.send(Command::Connect(Box::new(p.into())))
        .expect("send");
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

/// The next event, without the reports of the session filters (see `next_session_filters`).
async fn next_event(net: &mut Net) -> Event {
    loop {
        match next_event_raw(net).await {
            Event::SessionFilters { .. } => {}
            other => return other,
        }
    }
}

async fn next_event_raw(net: &mut Net) -> Event {
    tokio::time::timeout(WAIT, net.events.recv())
        .await
        .expect("event timeout")
        .expect("events closed")
}

/// Waits for a report of the session filters that satisfies `f`. Returns the set.
async fn next_session_filters(net: &mut Net, f: impl Fn(&[String]) -> bool) -> Vec<String> {
    loop {
        if let Event::SessionFilters { filters, .. } = next_event_raw(net).await
            && f(&filters)
        {
            return filters;
        }
    }
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
    net.send(Command::Connect(Box::new(p.into())))
        .expect("send");
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
    net.send(Command::Connect(Box::new(p.into())))
        .expect("send");
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

/// The UI does not read events while it sends 1000 QoS 0 publishes. Each publish must still get
/// exactly one `Published` result (sent, or rejected by the cap). The old log held 256 events and dropped the rest.
#[tokio::test(flavor = "current_thread")]
async fn slow_ui_still_gets_every_publish_result() {
    if !broker_up() {
        return;
    }
    let _limits = limits_lock();
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, profile(&[])).await;

    const N: u64 = 1000;
    let topic = format!("{}/slow", prefix());
    let mut seen = vec![0u8; N as usize];
    let mut got = 0u64;
    let take = |seen: &mut Vec<u8>, got: &mut u64, event: Event| match event {
        // The cap of 256 open operations can reject a publish. That is a result too.
        Event::Published {
            op_id,
            result: PublishResult::Sent,
            ..
        }
        | Event::Published {
            op_id,
            result: PublishResult::Rejected(_),
            ..
        } => {
            seen[op_id as usize] += 1;
            *got += 1;
        }
        other => panic!("unexpected {other:?}"),
    };
    for i in 0..N {
        loop {
            match net.send(out(i, &topic, b"x", Qos::AtMostOnce)) {
                Ok(()) => break,
                // The command queue is full, or the log has no spare room. Read a little.
                Err(navigator_core::net::SendError::Full) => {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    while let Ok(e) = net.events.try_recv() {
                        take(&mut seen, &mut got, e);
                    }
                }
                Err(e) => panic!("{e}"),
            }
        }
    }
    while got < N {
        take(&mut seen, &mut got, next_event(&mut net).await);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(net.events.try_recv().is_err(), "an extra event");
    assert!(seen.iter().all(|&n| n == 1), "a result is missing or twice");
    assert_eq!(net.stats.event_drops.load(Ordering::Relaxed), 0);

    net.shutdown().await;
}

/// 3000 desired filters and a UI that drains slowly. The automatic subscribe must wait for room
/// in the event log. Every `Subscribed` result and every `Published` result must arrive once,
/// and nothing may be dropped.
#[tokio::test(flavor = "current_thread")]
async fn many_filters_and_a_slow_ui_lose_no_result() {
    if !broker_up() {
        return;
    }
    let _limits = limits_lock();
    let mut net = Net::start(NetLimits::default());
    let p = prefix();
    const FILTERS: usize = 3000;
    const PUBS: u64 = 300;
    let filters: Vec<String> = (0..FILTERS).map(|i| format!("{p}/f/{i}")).collect();
    let refs: Vec<&str> = filters.iter().map(String::as_str).collect();
    connect(&mut net, profile(&refs)).await;

    let mut subs = std::collections::HashSet::new();
    let mut pubs = vec![0u8; PUBS as usize];
    let mut take = |event: Event| match event {
        Event::Subscribed {
            filter,
            result: Ok(_),
            ..
        } => assert!(subs.insert(filter), "a Subscribed result came twice"),
        // While the 3000 re-subscribes use the operation slots, a publish can get the documented
        // "Too many messages are waiting" answer. It is that publish's single result.
        Event::Published {
            op_id,
            result: PublishResult::Acknowledged,
            ..
        } => pubs[op_id as usize] += 1,
        Event::Published {
            op_id,
            result: PublishResult::Rejected(why),
            ..
        } if why.contains("Too many") => pubs[op_id as usize] += 1,
        other => panic!("unexpected {other:?}"),
    };
    let topic = format!("{p}/f/0");
    // The UI reads a few events at each tick. The publishes must wait for room.
    for i in 0..PUBS {
        loop {
            match net.send(out(i, &topic, b"x", Qos::AtLeastOnce)) {
                Ok(()) => break,
                Err(navigator_core::net::SendError::Full) => {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    for _ in 0..3 {
                        if let Ok(e) = net.events.try_recv() {
                            take(e);
                        }
                    }
                }
                Err(e) => panic!("{e}"),
            }
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let event = tokio::time::timeout_at(deadline, net.events.recv())
            .await
            .expect("a result is missing")
            .expect("events closed");
        take(event);
        tokio::time::sleep(Duration::from_micros(500)).await;
        // `take` holds the sets, so count through the log: stop when the log is quiet.
        if net.events.is_empty() && net.stats.event_drops.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if net.events.is_empty() {
                break;
            }
        }
    }
    drop(take);
    assert_eq!(subs.len(), FILTERS, "every Subscribed result arrives");
    assert!(
        pubs.iter().all(|&n| n == 1),
        "every publish result arrives once"
    );
    assert_eq!(net.stats.event_drops.load(Ordering::Relaxed), 0);
    net.shutdown().await;
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
        net.send(Command::Connect(Box::new(p.clone().into())))
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

/// A TCP forwarder to the broker that a test can cut, like a cable pull.
struct CuttableProxy {
    port: u16,
    streams: std::sync::Arc<Mutex<Vec<std::net::TcpStream>>>,
    cut: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl CuttableProxy {
    fn start() -> Self {
        Self::start_on(0)
    }

    /// Like `start`, on a given port (0: any). A port of an older proxy can need a moment to
    /// become free.
    fn start_on(port: u16) -> Self {
        let listener = (0..50)
            .find_map(|_| {
                std::net::TcpListener::bind(("127.0.0.1", port))
                    .map_err(|_| std::thread::sleep(Duration::from_millis(100)))
                    .ok()
            })
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let streams = std::sync::Arc::new(Mutex::new(Vec::new()));
        let cut = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s, c) = (streams.clone(), cut.clone());
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                if c.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(upstream) = std::net::TcpStream::connect("127.0.0.1:1883") else {
                    continue;
                };
                for (mut from, mut to) in [
                    (client.try_clone(), upstream.try_clone()),
                    (upstream.try_clone(), client.try_clone()),
                ]
                .into_iter()
                .filter_map(|(a, b)| Some((a.ok()?, b.ok()?)))
                {
                    std::thread::spawn(move || {
                        let _ = std::io::copy(&mut from, &mut to);
                        let _ = to.shutdown(std::net::Shutdown::Both);
                    });
                }
                s.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend([client, upstream]);
            }
        });
        Self { port, streams, cut }
    }

    /// Closes every connection without an MQTT DISCONNECT, and refuses new ones.
    fn cut(&self) {
        self.cut.store(true, Ordering::SeqCst);
        for s in self
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port)); // wakes the accept loop
    }
}

/// The CONNECT carries the Will: when the connection drops without DISCONNECT, the broker
/// publishes it to a second client.
#[tokio::test(flavor = "current_thread")]
async fn will_is_published_when_the_connection_drops() {
    let _limits = limits_lock();
    if !broker_up() {
        return;
    }
    let p = prefix();
    let will_topic = format!("{p}/will");
    let mut observer = Net::start(NetLimits::default());
    let mut watcher = profile(&[&will_topic]);
    watcher.client_id = format!("obs-{}", watcher.client_id);
    connect(&mut observer, watcher).await;
    assert!(matches!(
        next_event(&mut observer).await,
        Event::Subscribed { result: Ok(_), .. }
    ));

    let proxy = CuttableProxy::start();
    let mut victim = Net::start(NetLimits::default());
    let mut v = profile(&[]);
    v.port = proxy.port;
    v.will = Some(navigator_core::net::Will {
        topic: will_topic.clone(),
        payload: vec![0, b'g', b'o', b'n', b'e', 0xff],
        qos: Qos::AtLeastOnce,
        retain: false,
        delay_secs: 0,
    });
    connect(&mut victim, v).await;
    proxy.cut();

    let m = next_incoming(&mut observer).await;
    assert_eq!(*m.topic, *will_topic);
    assert_eq!(&m.payload[..], &[0, b'g', b'o', b'n', b'e', 0xff]);
    assert_eq!(m.qos, Qos::AtLeastOnce);
    victim.shutdown().await;
    observer.shutdown().await;
}

/// A Connect that the user replaced before it answered. The generation is chosen when the
/// Connect is sent, so the UI knows which generation is the new one. The old Connect (and the
/// Subscribe that followed it) must not show up under the new generation.
#[tokio::test(flavor = "current_thread")]
async fn a_replaced_connect_never_uses_the_new_generation() {
    let _limits = limits_lock();
    if !broker_up() {
        return;
    }
    let mut net = Net::start(NetLimits::default());
    let mut a = profile(&[]);
    a.port = 1; // nothing listens here: the Connect A never gets a CONNACK
    let ga = net.connect(a.into()).expect("connect A");
    net.send(Command::Subscribe {
        filter: "a/only".into(),
        qos: Qos::AtMostOnce,
    })
    .expect("subscribe for A");
    // The user switches to B before A has published anything.
    net.disconnect();
    let p = prefix();
    let gb = net
        .connect(profile(&[&format!("{p}/b")]).into())
        .expect("connect B");
    assert!(gb > ga, "generations grow: {ga} then {gb}");
    tokio::time::timeout(
        WAIT,
        net.status
            .wait_for(|s| s.generation == gb && s.state == ConnState::Connected),
    )
    .await
    .expect("B connects")
    .expect("status");
    // B works: its own SUBACK carries its generation. Events of A have another one.
    let mut b_events = 0;
    loop {
        let event = next_event(&mut net).await;
        let g = match &event {
            Event::Reconnected { generation, .. }
            | Event::Subscribed { generation, .. }
            | Event::Unsubscribed { generation, .. }
            | Event::Published { generation, .. }
            | Event::SessionFilters { generation, .. }
            | Event::Error { generation, .. } => *generation,
        };
        if g != gb {
            assert!(g < gb, "{event:?}");
            continue;
        }
        assert!(
            matches!(&event, Event::Subscribed { result: Ok(_), .. }),
            "{event:?}"
        );
        b_events += 1;
        break;
    }
    assert_eq!(b_events, 1);
    publish_plain(&format!("{p}/b"), b"hi".to_vec()).await;
    let m = next_incoming(&mut net).await;
    assert_eq!(m.generation, gb);
    net.shutdown().await;
}

/// A filter that the user removes while the connection is down. The broker keeps the session
/// (clean start off, session expiry on), so it still has the subscription. The net thread must
/// remove it after the CONNACK, or the broker keeps sending its messages.
#[tokio::test(flavor = "current_thread")]
async fn a_removal_while_offline_reaches_a_resumed_session() {
    let _limits = limits_lock();
    if !broker_up() {
        return;
    }
    let p = prefix();
    let topic = format!("{p}/kept");
    let proxy = CuttableProxy::start();
    let port = proxy.port;
    let mut net = Net::start(NetLimits::default());
    let mut v = profile(&[&topic]);
    v.port = port;
    v.clean_start = false;
    v.session_expiry_secs = 60;
    let generation = connect(&mut net, v).await;
    assert!(matches!(
        next_event(&mut net).await,
        Event::Subscribed { result: Ok(_), .. }
    ));

    // Cable pull. The net thread waits in the backoff, and nothing listens on the port.
    proxy.cut();
    wait_state(&mut net, |s| matches!(s, ConnState::Reconnecting { .. })).await;
    net.send(Command::Unsubscribe {
        filter: topic.clone(),
    })
    .expect("unsubscribe");
    assert!(matches!(
        next_event(&mut net).await,
        Event::Unsubscribed { result: Ok(()), .. }
    ));

    // The way to the broker is back. The broker resumes the session.
    let _proxy2 = CuttableProxy::start_on(port);
    tokio::time::timeout(
        WAIT * 2,
        net.status
            .wait_for(|s| s.generation == generation && s.state == ConnState::Connected),
    )
    .await
    .expect("reconnects")
    .expect("status");
    // Let the UNSUBSCRIBE reach the broker, then publish.
    tokio::time::sleep(Duration::from_millis(500)).await;
    publish_plain(&topic, b"must not arrive".to_vec()).await;
    let got = tokio::time::timeout(Duration::from_secs(2), net.incoming.recv()).await;
    assert!(got.is_err(), "the removed filter still delivers: {got:?}");
    net.shutdown().await;
}

/// A persistent-session profile. The same value gives the same broker session.
fn persistent(subs: &[&str], port: u16) -> Profile {
    Profile {
        port,
        clean_start: false,
        session_expiry_secs: 60,
        ..profile(subs)
    }
}

/// Waits until nothing arrives for a short time. `Err` shows the message that arrived.
async fn expect_silence(net: &mut Net) {
    let got = tokio::time::timeout(Duration::from_secs(2), net.incoming.recv()).await;
    assert!(got.is_err(), "the removed filter still delivers: {got:?}");
}

/// The user removes a filter after a manual Disconnect. The next Connect resumes the broker
/// session, which still holds the filter. The net thread must remove it.
#[tokio::test(flavor = "current_thread")]
async fn a_removal_after_a_manual_disconnect_reaches_the_next_session() {
    let _limits = limits_lock();
    if !broker_up() {
        return;
    }
    let p = prefix();
    let topic = format!("{p}/x");
    let mut net = Net::start(NetLimits::default());
    let first = persistent(&[&topic], 1883);
    connect(&mut net, first.clone()).await;
    assert!(matches!(
        next_event(&mut net).await,
        Event::Subscribed { result: Ok(_), .. }
    ));
    net.disconnect();
    wait_state(&mut net, |s| s == &ConnState::Disconnected).await;
    // Nothing is connected: the net thread answers with an error, and the UI keeps the removal.
    net.send(Command::Unsubscribe {
        filter: topic.clone(),
    })
    .expect("unsubscribe");
    assert!(matches!(
        next_event(&mut net).await,
        Event::Unsubscribed { result: Err(_), .. }
    ));
    // The same session (host, port, client ID), now without the filter.
    connect(
        &mut net,
        Profile {
            subscriptions: vec![],
            ..first
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    publish_plain(&topic, b"must not arrive".to_vec()).await;
    expect_silence(&mut net).await;
    net.shutdown().await;
}

/// A live UNSUBSCRIBE that never reaches the broker (the connection is cut first). The next
/// connection resumes the session, and the net thread removes the filter then.
#[tokio::test(flavor = "current_thread")]
async fn a_lost_unsubscribe_is_retried_on_the_resumed_session() {
    let _limits = limits_lock();
    if !broker_up() {
        return;
    }
    let p = prefix();
    let topic = format!("{p}/lost");
    let proxy = CuttableProxy::start();
    let port = proxy.port;
    let mut net = Net::start(NetLimits::default());
    let generation = connect(&mut net, persistent(&[&topic], port)).await;
    assert!(matches!(
        next_event(&mut net).await,
        Event::Subscribed { result: Ok(_), .. }
    ));
    // Cut the cable, and send the UNSUBSCRIBE at once: the broker cannot get it.
    proxy.cut();
    net.send(Command::Unsubscribe {
        filter: topic.clone(),
    })
    .expect("unsubscribe");
    let _ = next_event(&mut net).await; // Ok (offline) or the error of the lost request
    let _proxy2 = CuttableProxy::start_on(port);
    tokio::time::timeout(
        WAIT * 2,
        net.status
            .wait_for(|s| s.generation == generation && s.state == ConnState::Connected),
    )
    .await
    .expect("reconnects")
    .expect("status");
    tokio::time::sleep(Duration::from_millis(500)).await;
    publish_plain(&topic, b"must not arrive".to_vec()).await;
    expect_silence(&mut net).await;
    net.shutdown().await;
}

/// A clean session holds nothing, so the same steps need no removal and still work.
#[tokio::test(flavor = "current_thread")]
async fn a_clean_session_forgets_the_filters() {
    let _limits = limits_lock();
    if !broker_up() {
        return;
    }
    let p = prefix();
    let topic = format!("{p}/clean");
    let mut net = Net::start(NetLimits::default());
    let first = profile(&[&topic]);
    connect(&mut net, first.clone()).await;
    assert!(matches!(
        next_event(&mut net).await,
        Event::Subscribed { result: Ok(_), .. }
    ));
    net.disconnect();
    wait_state(&mut net, |s| s == &ConnState::Disconnected).await;
    connect(
        &mut net,
        Profile {
            subscriptions: vec![],
            ..first
        },
    )
    .await;
    publish_plain(&topic, b"nobody listens".to_vec()).await;
    expect_silence(&mut net).await;
    // Subscribing again still works.
    net.send(Command::Subscribe {
        filter: topic.clone(),
        qos: Qos::AtLeastOnce,
    })
    .expect("subscribe");
    assert!(matches!(
        next_event(&mut net).await,
        Event::Subscribed { result: Ok(_), .. }
    ));
    publish_plain(&topic, b"now".to_vec()).await;
    assert_eq!(&next_incoming(&mut net).await.payload[..], b"now");
    net.shutdown().await;
}

/// The app starts again between the removal and the reconnect: a NEW `Net` has no memory. The
/// saved set (from `Event::SessionFilters`) seeds it, so it still removes the filter from the
/// resumed session.
#[tokio::test(flavor = "current_thread")]
async fn a_removal_survives_a_restart_with_the_saved_session_filters() {
    let _limits = limits_lock();
    if !broker_up() {
        return;
    }
    let p = prefix();
    let topic = format!("{p}/restart");
    let first = persistent(&[&topic], 1883);
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, first.clone()).await;
    // The report comes after the SUBSCRIBE was sent. The app would save this set.
    let saved = next_session_filters(&mut net, |f| f.contains(&topic)).await;
    net.shutdown().await;

    // The user removed the filter (the new profile has no subscriptions), and the app restarts.
    let mut fresh = Net::start(NetLimits::default());
    let config = ConnectConfig {
        session_filters: saved,
        ..ConnectConfig::from(Profile {
            subscriptions: vec![],
            ..first
        })
    };
    fresh
        .send(Command::Connect(Box::new(config)))
        .expect("connect");
    wait_state(&mut fresh, |s| s == &ConnState::Connected).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    publish_plain(&topic, b"must not arrive".to_vec()).await;
    expect_silence(&mut fresh).await;
    // The set is empty now: the UNSUBACK was reported.
    next_session_filters(&mut fresh, |f| f.is_empty()).await;
    fresh.shutdown().await;
}

/// A new profile shares the broker session of an older one (an imported copy with the same
/// client ID). The net thread has the set in its cache and nothing changes, but the new profile
/// must still get the set to save it. Without it, a restart after a removal forgets the filter.
#[tokio::test(flavor = "current_thread")]
async fn a_new_profile_on_a_shared_session_gets_the_cached_set() {
    let _limits = limits_lock();
    if !broker_up() {
        return;
    }
    let p = prefix();
    let topic = format!("{p}/shared");
    let original = persistent(&[&topic], 1883);
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, original.clone()).await;
    next_session_filters(&mut net, |f| f.contains(&topic)).await;
    net.disconnect();
    wait_state(&mut net, |s| s == &ConnState::Disconnected).await;

    // The imported copy: a new ID, the same broker and client ID, no saved set.
    let copy = Profile {
        id: uuid::Uuid::new_v4(),
        session_filters: vec![],
        ..original
    };
    connect(&mut net, copy.clone()).await;
    // Nothing changed in the cached set, and it is reported anyway.
    let saved = next_session_filters(&mut net, |f| f.contains(&topic)).await;
    net.shutdown().await;

    // The user removes the filter from the copy, and the app restarts with the saved set.
    let mut fresh = Net::start(NetLimits::default());
    let config = ConnectConfig {
        session_filters: saved,
        ..ConnectConfig::from(Profile {
            subscriptions: vec![],
            ..copy
        })
    };
    fresh
        .send(Command::Connect(Box::new(config)))
        .expect("connect");
    wait_state(&mut fresh, |s| s == &ConnState::Connected).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    publish_plain(&topic, b"must not arrive".to_vec()).await;
    expect_silence(&mut fresh).await;
    fresh.shutdown().await;
}
