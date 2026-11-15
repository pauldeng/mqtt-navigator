//! Integration tests against a local broker at 127.0.0.1:18830 (plain TCP, anonymous).
//! Run them with `--features broker-tests` and `tests/broker/up.sh mosquitto`. A test fails if
//! the broker is not reachable: a missing broker must never look like a pass.

#![cfg(feature = "broker-tests")]

use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
mod common;
use common::Publisher;
use navigator_core::model::{Incoming, MsgProps, Qos};
use navigator_core::net::{
    Command, ConnState, ConnectConfig, Event, Net, NetLimits, Outgoing, Profile, Protocol,
    PublishResult, RetainHandling, SubOptions, Transport, queue_cost,
};

const WAIT: Duration = Duration::from_secs(10);

/// Fails the test if there is no broker at 127.0.0.1:18830.
fn require_broker() {
    std::net::TcpStream::connect_timeout(
        &"127.0.0.1:18830".parse().expect("addr"),
        Duration::from_secs(1),
    )
    .expect("no broker at 127.0.0.1:18830: start it with tests/broker/up.sh mosquitto");
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
        port: 18830,
        transport: Transport::Tcp,
        subscriptions: subs
            .iter()
            .map(|s| ((*s).to_owned(), Qos::AtLeastOnce))
            .collect(),
        protocol: Protocol::V5,
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
            // The skipped details must belong to a connection that exists. The lifecycle test
            // `connection_details_come_once_for_each_connect` reads them raw.
            Event::ConnectionDetails { generation, .. } => {
                let latest = net.status.borrow().generation;
                assert!(
                    generation >= 1 && generation <= latest,
                    "{generation} of {latest}"
                );
            }
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
    require_broker();
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
                ..
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
    require_broker();
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

/// A second client (its own `Net`) that publishes one QoS 1 message and waits for the PUBACK.
async fn publish_plain(topic: &str, payload: Vec<u8>) {
    let mut publisher = Publisher::connect("127.0.0.1", 18830)
        .await
        .expect("second client cannot connect");
    publisher
        .send(topic, Bytes::from(payload), Qos::AtLeastOnce)
        .await
        .expect("publish");
    publisher.settle(WAIT).await;
    assert_eq!(
        (publisher.done, &publisher.failed[..]),
        (1, &[][..]),
        "puback"
    );
    publisher.shutdown().await;
}

/// MQTT 5 incoming limit (`max_packet` 1024, announced in CONNECT as Maximum Packet Size): a
/// packet of exactly the limit is delivered. A packet one byte over never reaches the UI: Mosquitto
/// 2.1.2 sends it anyway, and the client ends the session with the size text (the v3 half is
/// `v3_an_incoming_packet_over_the_limit_ends_the_session`). The first copy of a message that Mosquitto forwards
/// has a Topic Alias property: 4 bytes (the length byte is 3 instead of 0, then 0x23 and 2
/// bytes), so each message has its own topic (same length) and the copy is 3 bytes larger than
/// the packet that the publisher sent.
#[tokio::test(flavor = "current_thread")]
async fn oversized_packet_is_not_delivered() {
    const MAX: usize = 1024;
    require_broker();
    let limits = NetLimits {
        max_packet: MAX,
        ..NetLimits::default()
    };
    let mut net = Net::start(limits);
    let p = prefix();
    connect(&mut net, profile(&[&format!("{p}/#")])).await;
    assert!(matches!(
        next_event(&mut net).await,
        Event::Subscribed { result: Ok(_), .. }
    ));

    // The copy: fixed header 1, length 2 bytes, topic 2 + n, packet ID 2, properties 4, payload.
    let copy_overhead = 1 + 2 + 2 + format!("{p}/ok").len() + 2 + 4;
    let exact = MAX - copy_overhead;
    publish_plain(&format!("{p}/ok"), vec![b'x'; exact]).await;
    let first = next_incoming(&mut net).await;
    assert_eq!(
        (&*first.topic, first.payload.len()),
        (&*format!("{p}/ok"), exact)
    );
    assert_eq!(first.protocol, Protocol::V5);
    publish_plain(&format!("{p}/xx"), vec![b'y'; exact + 1]).await;
    // Mosquitto sends it, and the client ends the session with the size text.
    wait_state(&mut net, |s| matches!(s, ConnState::Failed(_))).await;
    assert_eq!(
        net.status.borrow().state,
        ConnState::Failed("The broker sent a packet that is larger than the allowed size.".into())
    );
    assert!(net.incoming.try_recv().is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn full_queue_drops_and_bounds_memory() {
    require_broker();
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

/// Messages with an empty payload and many user properties must count against the byte budget.
#[tokio::test(flavor = "current_thread")]
async fn property_heavy_messages_are_bounded_and_budget_recovers() {
    require_broker();
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
            // One for each connect (S2d).
            Ok(Some(Event::ConnectionDetails { .. })) => {}
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
    require_broker();
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
        // One for each connect (S2d).
        Event::ConnectionDetails { .. } => {}
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
    require_broker();
    let mut net = Net::start(NetLimits::default());
    let p = prefix();
    const FILTERS: usize = 3000;
    const PUBS: u64 = 300;
    let filters: Vec<String> = (0..FILTERS).map(|i| format!("{p}/f/{i}")).collect();
    let refs: Vec<&str> = filters.iter().map(String::as_str).collect();
    connect(&mut net, profile(&refs)).await;

    let mut subs = std::collections::HashSet::new();
    let mut pubs = vec![0u8; PUBS as usize];
    // This test reads `net.events` itself, so it tells the net thread when it made room.
    let drained = net.events_drained();
    let mut take = |event: Event| {
        drained.notify();
        match event {
            Event::Subscribed {
                filter,
                result: Ok(_),
                ..
            } => assert!(subs.insert(filter), "a Subscribed result came twice"),
            // While the 3000 re-subscribes use the operation slots, a publish can get the
            // documented "Too many messages are waiting" answer. It is that publish's result.
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
            // One for each connect (S2d).
            Event::ConnectionDetails { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
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
                let Ok(upstream) = std::net::TcpStream::connect("127.0.0.1:18830") else {
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
    require_broker();
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
    require_broker();
    let mut net = Net::start(NetLimits::default());
    let mut a = profile(&[]);
    a.port = 1; // nothing listens here: the Connect A never gets a CONNACK
    let ga = net.connect(a.into()).expect("connect A");
    net.send(Command::Subscribe {
        filter: "a/only".into(),
        qos: Qos::AtMostOnce,
        options: Default::default(),
        req: 0,
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
            | Event::ConnectionDetails { generation, .. }
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
    removal_while_offline(Protocol::V5).await;
}

/// The same test on MQTT 3.1.1: Clean Session off keeps the session (D3).
#[tokio::test(flavor = "current_thread")]
async fn v3_a_removal_while_offline_reaches_a_resumed_session() {
    removal_while_offline(Protocol::V311).await;
}

async fn removal_while_offline(protocol: Protocol) {
    require_broker();
    let p = prefix();
    let topic = format!("{p}/kept");
    let proxy = CuttableProxy::start();
    let port = proxy.port;
    let mut net = Net::start(NetLimits::default());
    let mut v = profile(&[&topic]);
    v.port = port;
    v.clean_start = false;
    v.session_expiry_secs = 60;
    let mut config = ConnectConfig::from(v);
    config.protocol = protocol;
    let generation = connect(&mut net, config).await;
    // The subscription is granted, and the session is reported for the profile (v3 too: Clean
    // Session off has no session expiry, D3).
    let (mut granted, mut reported) = (false, false);
    while !(granted && reported) {
        match next_event_raw(&mut net).await {
            Event::Subscribed { result: Ok(_), .. } => granted = true,
            Event::SessionFilters { filters, .. } if filters == [topic.as_str()] => {
                reported = true;
            }
            _ => {}
        }
    }

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
fn persistent(subs: &[&str], port: u16, protocol: Protocol) -> Profile {
    Profile {
        port,
        protocol,
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

/// A clean session holds nothing, so the same steps need no removal and still work. (MQTT 3.1.1
/// shares the code: `v3_clean_session_keeps_nothing` covers its Clean Session flag.)
#[tokio::test(flavor = "current_thread")]
async fn a_clean_session_forgets_the_filters() {
    require_broker();
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
        options: Default::default(),
        req: 0,
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

/// The removal of a filter reaches a persistent session that the broker keeps (v5; MQTT 3.1.1 has
/// the same code, and `v3_a_removal_while_offline_reaches_a_resumed_session` covers it). One
/// client ID and one cuttable forwarder serve four steps, each with its own topic:
/// 1. the user removes the filter after a manual Disconnect;
/// 2. an UNSUBSCRIBE is lost when the connection is cut, and the next connection retries it;
/// 3. the app restarts after the removal: the saved session filters seed a new `Net`;
/// 4. a new profile with the same client ID gets the cached set, so a later restart still works.
#[tokio::test(flavor = "current_thread")]
async fn a_removal_reaches_the_resumed_session() {
    require_broker();
    let p = prefix();
    let [t1, t2, t3, t4] = ["one", "two", "three", "four"].map(|n| format!("{p}/{n}"));
    let mut proxy = CuttableProxy::start();
    let port = proxy.port;
    let base = persistent(&[], port, Protocol::V5);
    let with = |topic: &str| Profile {
        subscriptions: vec![(topic.to_owned(), Qos::AtLeastOnce)],
        ..base.clone()
    };
    let mut net = Net::start(NetLimits::default());

    // 1. A removal after a manual Disconnect.
    connect(&mut net, with(&t1)).await;
    assert!(matches!(
        next_event(&mut net).await,
        Event::Subscribed { result: Ok(_), .. }
    ));
    net.disconnect();
    wait_state(&mut net, |s| s == &ConnState::Disconnected).await;
    // Nothing is connected: the net thread answers with an error, and the UI keeps the removal.
    net.send(Command::Unsubscribe { filter: t1.clone() })
        .expect("unsubscribe");
    assert!(matches!(
        next_event(&mut net).await,
        Event::Unsubscribed { result: Err(_), .. }
    ));
    connect(&mut net, base.clone()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    publish_plain(&t1, b"must not arrive".to_vec()).await;
    expect_silence(&mut net).await;

    // 2. A live UNSUBSCRIBE that never reaches the broker.
    let generation = connect(&mut net, with(&t2)).await;
    assert!(matches!(
        next_event(&mut net).await,
        Event::Subscribed { result: Ok(_), .. }
    ));
    proxy.cut();
    net.send(Command::Unsubscribe { filter: t2.clone() })
        .expect("unsubscribe");
    let _ = next_event(&mut net).await; // Ok (offline) or the error of the lost request
    proxy = CuttableProxy::start_on(port);
    // First wait for the loss. The error of the lost request can arrive before the status leaves
    // `Connected`, so waiting for `Connected` alone can return at once, and the publish below
    // would then reach the broker while the session is still offline and subscribed.
    tokio::time::timeout(
        WAIT * 2,
        net.status.wait_for(|s| {
            s.generation == generation && matches!(s.state, ConnState::Reconnecting { .. })
        }),
    )
    .await
    .expect("the loss is noticed")
    .expect("status");
    tokio::time::timeout(
        WAIT * 2,
        net.status
            .wait_for(|s| s.generation == generation && s.state == ConnState::Connected),
    )
    .await
    .expect("reconnects")
    .expect("status");
    tokio::time::sleep(Duration::from_millis(500)).await;
    publish_plain(&t2, b"must not arrive".to_vec()).await;
    expect_silence(&mut net).await;

    // 3. The app starts again between the removal and the reconnect: a NEW `Net` has no memory.
    // The saved set (from `Event::SessionFilters`) seeds it, so it still removes the filter.
    connect(&mut net, with(&t3)).await;
    // The report comes after the SUBSCRIBE was sent. The app would save this set.
    let saved = next_session_filters(&mut net, |f| f.contains(&t3)).await;
    net.shutdown().await;
    let mut fresh = Net::start(NetLimits::default());
    let config = ConnectConfig {
        session_filters: saved,
        ..ConnectConfig::from(base.clone())
    };
    fresh
        .send(Command::Connect(Box::new(config)))
        .expect("connect");
    wait_state(&mut fresh, |s| s == &ConnState::Connected).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    publish_plain(&t3, b"must not arrive".to_vec()).await;
    expect_silence(&mut fresh).await;
    // The set is empty now: the UNSUBACK was reported.
    next_session_filters(&mut fresh, |f| f.is_empty()).await;

    // 4. A new profile shares the broker session of an older one (an imported copy with the same
    // client ID). The net thread has the set in its cache and nothing changes, but the new
    // profile must still get the set to save it.
    connect(&mut fresh, with(&t4)).await;
    next_session_filters(&mut fresh, |f| f.contains(&t4)).await;
    fresh.disconnect();
    wait_state(&mut fresh, |s| s == &ConnState::Disconnected).await;
    let copy = Profile {
        id: uuid::Uuid::new_v4(),
        session_filters: vec![],
        ..with(&t4)
    };
    connect(&mut fresh, copy).await;
    let saved = next_session_filters(&mut fresh, |f| f.contains(&t4)).await;
    fresh.shutdown().await;
    // The user removes the filter from the copy, and the app restarts with the saved set.
    let mut last = Net::start(NetLimits::default());
    let config = ConnectConfig {
        session_filters: saved,
        ..ConnectConfig::from(base)
    };
    last.send(Command::Connect(Box::new(config)))
        .expect("connect");
    wait_state(&mut last, |s| s == &ConnState::Connected).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    publish_plain(&t4, b"must not arrive".to_vec()).await;
    expect_silence(&mut last).await;
    last.shutdown().await;
    drop(proxy);
}

// -------------------------------------------------------------------------------------------
// MQTT 3.1.1 (S2b)
// -------------------------------------------------------------------------------------------

/// A v3 profile. Clean Session off (`persistent`) gives a session that the broker keeps.
fn v3(subs: &[&str], client_id: &str, persistent: bool) -> ConnectConfig {
    let mut c = ConnectConfig::from(Profile {
        client_id: client_id.into(),
        clean_start: !persistent,
        ..profile(subs)
    });
    c.protocol = Protocol::V311;
    c
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

/// The control: with Clean Session on, the broker keeps nothing.
#[tokio::test(flavor = "current_thread")]
async fn v3_clean_session_keeps_nothing() {
    require_broker();
    let p = prefix();
    let topic = format!("{p}/clean");
    let id = format!("v3-clean-{}", std::process::id());

    let mut first = Net::start(NetLimits::default());
    connect(&mut first, v3(&[&topic], &id, false)).await;
    assert!(matches!(
        next_event(&mut first).await,
        Event::Subscribed { result: Ok(_), .. }
    ));
    first.shutdown().await;

    let mut other = Net::start(NetLimits::default());
    connect(&mut other, v3(&[], "v3-clean-other", false)).await;
    other
        .send(out(1, &topic, b"lost", Qos::AtLeastOnce))
        .expect("send");
    assert_eq!(published(&mut other, 1).await, PublishResult::Acknowledged);
    other.shutdown().await;

    let mut again = Net::start(NetLimits::default());
    connect(&mut again, v3(&[], &id, false)).await;
    expect_silence(&mut again).await;
    again.shutdown().await;
}

/// A QoS 1 message with a whole packet of `total` bytes (v3, one-byte QoS 1 topic prefix
/// included in `topic`).
fn v3_message(op_id: u64, topic: &str, total: usize) -> Command {
    // Fixed header 1, length bytes 3 (for sizes of 16 KiB to 2 MiB), topic length 2, topic,
    // packet ID 2.
    let payload = total - (1 + 3 + 2 + topic.len() + 2);
    Command::Publish(Box::new(Outgoing {
        op_id,
        topic: topic.into(),
        payload: Bytes::from(vec![b'x'; payload]),
        qos: Qos::AtLeastOnce,
        retain: false,
        props: MsgProps::default(),
    }))
}

/// Size boundaries on the real broker, v3: a packet of exactly 1 MiB goes out and comes back;
/// one byte more is refused before the send, with the text of the plan.
#[tokio::test(flavor = "current_thread")]
async fn v3_a_packet_of_one_mib_round_trips_and_one_byte_more_is_refused() {
    const MIB: usize = 1024 * 1024;
    require_broker();
    let p = prefix();
    let topic = format!("{p}/mib");
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, v3(&[&topic], "v3-mib", false)).await;
    assert!(matches!(
        next_event(&mut net).await,
        Event::Subscribed { result: Ok(_), .. }
    ));
    net.send(v3_message(1, &topic, MIB)).expect("send");
    let m = next_incoming(&mut net).await;
    assert_eq!(m.payload.len(), MIB - (1 + 3 + 2 + topic.len() + 2));
    assert_eq!(published(&mut net, 1).await, PublishResult::Acknowledged);

    net.send(v3_message(2, &topic, MIB + 1)).expect("send");
    assert_eq!(
        published(&mut net, 2).await,
        PublishResult::Rejected("MQTT 3.1.1 messages can be at most 1 MiB in this app.".into())
    );
    let nothing = tokio::time::timeout(Duration::from_secs(1), net.incoming.recv()).await;
    assert!(
        nothing.is_err(),
        "the refused message was sent: {nothing:?}"
    );
    net.shutdown().await;
}

/// Incoming v3 boundary on the real broker: a packet of exactly the limit is delivered. One byte
/// more (sent by a publisher with a larger limit) ends the session with the size text.
#[tokio::test(flavor = "current_thread")]
async fn v3_an_incoming_packet_over_the_limit_ends_the_session() {
    const LIMIT: usize = 64 * 1024;
    require_broker();
    let p = prefix();
    let topic = format!("{p}/limit");
    let mut sub = Net::start(NetLimits {
        max_packet: LIMIT,
        ..NetLimits::default()
    });
    connect(&mut sub, v3(&[&topic], "v3-limit-sub", false)).await;
    assert!(matches!(
        next_event(&mut sub).await,
        Event::Subscribed { result: Ok(_), .. }
    ));
    let mut publisher = Net::start(NetLimits {
        max_packet: 2 * LIMIT,
        ..NetLimits::default()
    });
    connect(&mut publisher, v3(&[], "v3-limit-pub", false)).await;

    publisher.send(v3_message(1, &topic, LIMIT)).expect("send");
    assert_eq!(
        published(&mut publisher, 1).await,
        PublishResult::Acknowledged
    );
    let m = next_incoming(&mut sub).await;
    assert_eq!(m.payload.len(), LIMIT - (1 + 3 + 2 + topic.len() + 2));
    assert_eq!(m.protocol, Protocol::V311);

    publisher
        .send(v3_message(2, &topic, LIMIT + 1))
        .expect("send");
    assert_eq!(
        published(&mut publisher, 2).await,
        PublishResult::Acknowledged
    );
    wait_state(&mut sub, |s| matches!(s, ConnState::Failed(_))).await;
    let state = sub.status.borrow().state.clone();
    assert_eq!(
        state,
        ConnState::Failed("The broker sent a packet that is larger than the allowed size.".into())
    );
    assert!(
        sub.incoming.try_recv().is_err(),
        "the packet over the limit was delivered"
    );
    publisher.shutdown().await;
    sub.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// S2d: MQTT 5 subscription options (plan, "MQTT 5 features"). Mosquitto applies them.
// ---------------------------------------------------------------------------------------------

/// A v5 profile whose subscriptions (QoS 1) have the given options.
fn profile_with_options(subs: &[(&str, SubOptions)]) -> Profile {
    let mut p = profile(&[]);
    for (filter, options) in subs {
        p.subscriptions
            .push(((*filter).to_owned(), Qos::AtLeastOnce));
        p.subscription_options
            .insert((*filter).to_owned(), *options);
    }
    p
}

/// Waits for `n` results of SUBSCRIBE. Each must be granted QoS 1.
async fn subscribed(net: &mut Net, n: usize) {
    for _ in 0..n {
        match next_event(net).await {
            Event::Subscribed { result, .. } => assert_eq!(result, Ok(Qos::AtLeastOnce)),
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn retained(op_id: u64, topic: &str, payload: &'static [u8]) -> Command {
    Command::Publish(Box::new(Outgoing {
        op_id,
        topic: topic.into(),
        payload: Bytes::from_static(payload),
        qos: Qos::AtLeastOnce,
        retain: true,
        props: MsgProps::default(),
    }))
}

/// No message arrives within a short time.
async fn nothing_arrives(net: &mut Net) {
    let got = tokio::time::timeout(Duration::from_millis(700), net.incoming.recv()).await;
    assert!(
        got.is_err(),
        "a message arrived: {:?}",
        got.map(|m| m.map(|m| m.topic))
    );
}

/// No Local: the broker does not send our own publish back. A second subscription without it
/// gets the same publish (the control), and a publish of another client reaches both.
#[tokio::test(flavor = "current_thread")]
async fn no_local_hides_our_own_publish() {
    require_broker();
    let p = prefix();
    let (hidden, shown) = (format!("{p}/nl"), format!("{p}/plain"));
    let no_local = SubOptions {
        no_local: true,
        ..SubOptions::default()
    };
    let mut net = Net::start(NetLimits::default());
    connect(
        &mut net,
        profile_with_options(&[(&hidden, no_local), (&shown, SubOptions::default())]),
    )
    .await;
    subscribed(&mut net, 2).await;
    net.send(out(1, &hidden, b"own", Qos::AtLeastOnce))
        .expect("send");
    assert_eq!(published(&mut net, 1).await, PublishResult::Acknowledged);
    net.send(out(2, &shown, b"own", Qos::AtLeastOnce))
        .expect("send");
    assert_eq!(published(&mut net, 2).await, PublishResult::Acknowledged);
    // Only the plain subscription gets our own message.
    let m = next_incoming(&mut net).await;
    assert_eq!((&*m.topic, &m.payload[..]), (shown.as_str(), &b"own"[..]));
    nothing_arrives(&mut net).await;

    // Another client publishes to the topic with No Local: it arrives.
    let mut other = Net::start(NetLimits::default());
    connect(&mut other, profile(&[])).await;
    other
        .send(out(1, &hidden, b"theirs", Qos::AtLeastOnce))
        .expect("send");
    assert_eq!(published(&mut other, 1).await, PublishResult::Acknowledged);
    let m = next_incoming(&mut net).await;
    assert_eq!(
        (&*m.topic, &m.payload[..]),
        (hidden.as_str(), &b"theirs"[..])
    );
    other.disconnect();
    net.disconnect();
    wait_state(&mut net, |s| s == &ConnState::Disconnected).await;
}

/// Retain Handling 2: no retained message at subscribe. Handling 0 gets it (the control), and
/// handling 1 gets it for a new subscription.
#[tokio::test(flavor = "current_thread")]
async fn retain_handling_decides_whether_a_retained_message_comes_at_subscribe() {
    require_broker();
    let p = prefix();
    let topic = format!("{p}/ret");
    let mut publisher = Net::start(NetLimits::default());
    connect(&mut publisher, profile(&[])).await;
    publisher.send(retained(1, &topic, b"kept")).expect("send");
    assert_eq!(
        published(&mut publisher, 1).await,
        PublishResult::Acknowledged
    );

    for (handling, arrives) in [
        (RetainHandling::Never, false),
        (RetainHandling::OnSubscribe, true),
        (RetainHandling::OnNewSubscription, true),
    ] {
        let options = SubOptions {
            retain_handling: handling,
            ..SubOptions::default()
        };
        let mut net = Net::start(NetLimits::default());
        connect(&mut net, profile_with_options(&[(&topic, options)])).await;
        subscribed(&mut net, 1).await;
        if arrives {
            let m = next_incoming(&mut net).await;
            assert_eq!(
                (&*m.topic, &m.payload[..], m.retain),
                (topic.as_str(), &b"kept"[..], true)
            );
        } else {
            nothing_arrives(&mut net).await;
        }
        net.disconnect();
        wait_state(&mut net, |s| s == &ConnState::Disconnected).await;
    }
    // The same SUBSCRIBE again on one connection: handling 0 (control) sends the retained message
    // again; handling 1 does not, because the subscription exists.
    for (handling, again) in [
        (RetainHandling::OnSubscribe, true),
        (RetainHandling::OnNewSubscription, false),
    ] {
        let options = SubOptions {
            retain_handling: handling,
            ..SubOptions::default()
        };
        let mut net = Net::start(NetLimits::default());
        connect(&mut net, profile_with_options(&[(&topic, options)])).await;
        subscribed(&mut net, 1).await;
        next_incoming(&mut net).await;
        net.send(Command::Subscribe {
            filter: topic.clone(),
            qos: Qos::AtLeastOnce,
            options,
            req: 5,
        })
        .expect("send");
        subscribed(&mut net, 1).await;
        if again {
            next_incoming(&mut net).await;
        } else {
            nothing_arrives(&mut net).await;
        }
        net.disconnect();
        wait_state(&mut net, |s| s == &ConnState::Disconnected).await;
    }
    // Clear the retained message.
    publisher.send(retained(2, &topic, b"")).expect("send");
    assert_eq!(
        published(&mut publisher, 2).await,
        PublishResult::Acknowledged
    );
    publisher.disconnect();
}

/// Retain As Published: a live message keeps its retain flag. Without the option the broker
/// clears the flag on a live message.
#[tokio::test(flavor = "current_thread")]
async fn retain_as_published_keeps_the_retain_flag_of_a_live_message() {
    require_broker();
    let p = prefix();
    let (kept, cleared) = (format!("{p}/rap"), format!("{p}/norap"));
    let rap = SubOptions {
        retain_as_published: true,
        ..SubOptions::default()
    };
    let mut net = Net::start(NetLimits::default());
    connect(
        &mut net,
        profile_with_options(&[(&kept, rap), (&cleared, SubOptions::default())]),
    )
    .await;
    subscribed(&mut net, 2).await;
    let mut publisher = Net::start(NetLimits::default());
    connect(&mut publisher, profile(&[])).await;
    publisher.send(retained(1, &kept, b"x")).expect("send");
    assert_eq!(
        published(&mut publisher, 1).await,
        PublishResult::Acknowledged
    );
    publisher.send(retained(2, &cleared, b"x")).expect("send");
    assert_eq!(
        published(&mut publisher, 2).await,
        PublishResult::Acknowledged
    );
    let mut seen = vec![];
    for _ in 0..2 {
        let m = next_incoming(&mut net).await;
        seen.push((m.topic.to_string(), m.retain));
    }
    seen.sort();
    assert_eq!(seen, [(cleared.clone(), false), (kept.clone(), true)]);
    // Clear both retained messages.
    publisher.send(retained(3, &kept, b"")).expect("send");
    publisher.send(retained(4, &cleared, b"")).expect("send");
    assert_eq!(
        published(&mut publisher, 3).await,
        PublishResult::Acknowledged
    );
    assert_eq!(
        published(&mut publisher, 4).await,
        PublishResult::Acknowledged
    );
    publisher.disconnect();
    net.disconnect();
    wait_state(&mut net, |s| s == &ConnState::Disconnected).await;
}

/// No event comes within a short time.
async fn no_event(net: &mut Net) {
    let got = tokio::time::timeout(Duration::from_millis(500), net.events.recv()).await;
    assert!(got.is_err(), "unexpected event: {got:?}");
}

/// Reads the next raw event and requires `ConnectionDetails` of `generation`. The status must
/// already be Connected: the details never come before it.
async fn expect_details(net: &mut Net, generation: u64) {
    match next_event_raw(net).await {
        Event::ConnectionDetails {
            generation: g,
            attempt,
            details,
        } => {
            assert_eq!(g, generation);
            let status = net.status.borrow().clone();
            assert_eq!(status.state, ConnState::Connected);
            // These are the details of the connected attempt, not of an older one.
            assert_eq!(attempt, status.attempt);
            assert_eq!(details.protocol, Protocol::V5);
            assert!(!details.session_present);
        }
        other => panic!("expected ConnectionDetails, got {other:?}"),
    }
}

/// One `ConnectionDetails` for each connect, in raw event order, with the right generation:
/// the first connect, a reconnect of the same generation (the way to the broker is cut), and a
/// replacement by a second Connect. A reconnect has `Reconnected` after the details.
#[tokio::test(flavor = "current_thread")]
async fn connection_details_come_once_for_each_connect() {
    require_broker();
    let proxy = CuttableProxy::start();
    let port = proxy.port;
    let mut p = profile(&[]);
    p.port = port;
    let mut net = Net::start(NetLimits::default());
    let g1 = connect(&mut net, p.clone()).await;
    expect_details(&mut net, g1).await;
    no_event(&mut net).await;

    proxy.cut();
    wait_state(&mut net, |s| matches!(s, ConnState::Reconnecting { .. })).await;
    let _proxy2 = CuttableProxy::start_on(port);
    tokio::time::timeout(
        WAIT * 2,
        net.status
            .wait_for(|s| s.generation == g1 && s.state == ConnState::Connected),
    )
    .await
    .expect("reconnects")
    .expect("status");
    expect_details(&mut net, g1).await;
    assert!(matches!(
        next_event_raw(&mut net).await,
        Event::Reconnected { generation, .. } if generation == g1
    ));
    no_event(&mut net).await;

    // A second Connect replaces the session: one new details, with the new generation.
    let g2 = connect(&mut net, profile(&[])).await;
    assert!(g2 > g1);
    expect_details(&mut net, g2).await;
    no_event(&mut net).await;
    net.disconnect();
    wait_state(&mut net, |s| s == &ConnState::Disconnected).await;
    net.shutdown().await;
}

/// A live Subscribe with the given options and `req`.
fn live_subscribe(net: &Net, filter: &str, options: SubOptions, req: u64) {
    net.send(Command::Subscribe {
        filter: filter.into(),
        qos: Qos::AtLeastOnce,
        options,
        req,
    })
    .expect("send");
}

/// The next `Subscribed` result as (filter, req, granted). Skips `Reconnected`.
async fn next_subscribed(net: &mut Net) -> (String, u64, bool) {
    loop {
        match next_event(net).await {
            Event::Subscribed {
                filter,
                req,
                result,
                ..
            } => return (filter, req, result.is_ok()),
            Event::Reconnected { .. } => {}
            other => panic!("expected Subscribed, got {other:?}"),
        }
    }
}

/// A lost connection and the retry, with the real broker behind a cuttable forwarder. Steps:
/// 1. Live subscribes before the cut: a local refusal (No Local on a shared filter), the same
///    filter twice (each result has its own `req`), and changed options (No Local).
/// 2. Subscribe and Unsubscribe while the net thread waits for the retry change the next
///    resubscribe.
/// 3. After the reconnect the broker has exactly the latest state: each filter has the `req`
///    of its latest accepted Subscribe, the changed options stay (No Local hides our own
///    message), and the removed filter delivers nothing.
/// 4. A Disconnect in the backoff cancels it at once, and nothing starts again.
#[tokio::test(flavor = "current_thread")]
async fn a_reconnect_resubscribes_the_latest_filters_options_and_reqs() {
    require_broker();
    let p = prefix();
    let [a, b, c] = ["a", "b", "c"].map(|n| format!("{p}/{n}"));
    let no_local = SubOptions {
        no_local: true,
        ..SubOptions::default()
    };
    let mut proxy = CuttableProxy::start();
    let port = proxy.port;
    let mut net = Net::start(NetLimits::default());
    let mut first = profile(&[&a]);
    first.port = port;
    let generation = connect(&mut net, first).await;

    // 1. Before the cut.
    let mut seen = vec![next_subscribed(&mut net).await];
    live_subscribe(&net, "$share/g/t", no_local, 7);
    seen.push(next_subscribed(&mut net).await);
    live_subscribe(&net, &a, SubOptions::default(), 11);
    seen.push(next_subscribed(&mut net).await);
    live_subscribe(&net, &a, no_local, 12);
    seen.push(next_subscribed(&mut net).await);
    // The same filter again with the same options: its result has its own `req`.
    live_subscribe(&net, &a, no_local, 14);
    seen.push(next_subscribed(&mut net).await);
    live_subscribe(&net, &b, SubOptions::default(), 9);
    seen.push(next_subscribed(&mut net).await);
    assert_eq!(
        seen,
        [
            (a.clone(), 0, true),
            ("$share/g/t".to_owned(), 7, false),
            (a.clone(), 11, true),
            (a.clone(), 12, true),
            (a.clone(), 14, true),
            (b.clone(), 9, true),
        ]
    );

    // 2. The cable is pulled. The net thread waits in the backoff.
    proxy.cut();
    wait_state(&mut net, |s| matches!(s, ConnState::Reconnecting { .. })).await;
    live_subscribe(&net, &c, SubOptions::default(), 13);
    net.send(Command::Unsubscribe { filter: b.clone() })
        .expect("unsubscribe");
    proxy = CuttableProxy::start_on(port);

    // 3. The resubscribe. The local refusal and the SUBACKs come in any order. The last result
    // of each filter counts (a Subscribe made while offline can also have a result at once).
    tokio::time::timeout(
        WAIT * 2,
        net.status
            .wait_for(|s| s.generation == generation && s.state == ConnState::Connected),
    )
    .await
    .expect("reconnects")
    .expect("status");
    let mut last = std::collections::BTreeMap::new();
    let mut unsubscribed = vec![];
    while let Ok(event) =
        tokio::time::timeout(Duration::from_millis(700), next_event_raw(&mut net)).await
    {
        match event {
            Event::Subscribed {
                filter,
                req,
                result,
                ..
            } => {
                last.insert(filter, (req, result.is_ok()));
            }
            Event::Unsubscribed { filter, result, .. } => {
                assert_eq!(result, Ok(()));
                unsubscribed.push(filter);
            }
            Event::Reconnected { .. }
            | Event::SessionFilters { .. }
            | Event::ConnectionDetails { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(unsubscribed, std::slice::from_ref(&b));
    assert_eq!(
        last,
        [
            ("$share/g/t".to_owned(), (7, false)),
            (a.clone(), (14, true)),
            (c.clone(), (13, true)),
        ]
        .into_iter()
        .collect()
    );
    // What the broker holds: a (No Local), c. Our own message on `a` is hidden, one from another
    // client arrives; the removed `b` delivers nothing; our own message on `c` arrives.
    net.send(out(1, &a, b"own-a", Qos::AtLeastOnce))
        .expect("send");
    assert_eq!(published(&mut net, 1).await, PublishResult::Acknowledged);
    publish_plain(&a, b"other-a".to_vec()).await;
    publish_plain(&b, b"other-b".to_vec()).await;
    net.send(out(2, &c, b"own-c", Qos::AtLeastOnce))
        .expect("send");
    assert_eq!(published(&mut net, 2).await, PublishResult::Acknowledged);
    let m = next_incoming(&mut net).await;
    assert_eq!((&*m.topic, &m.payload[..]), (&*a, &b"other-a"[..]));
    let m = next_incoming(&mut net).await;
    assert_eq!((&*m.topic, &m.payload[..]), (&*c, &b"own-c"[..]));
    expect_silence(&mut net).await;

    // 4. Disconnect in the backoff.
    proxy.cut();
    wait_state(&mut net, |s| matches!(s, ConnState::Reconnecting { .. })).await;
    let started = std::time::Instant::now();
    net.disconnect();
    let ended = wait_state(&mut net, |s| s == &ConnState::Disconnected).await;
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the cancel waited for the backoff"
    );
    assert_eq!(ended, generation);
    // The first retry would come after 1 s. Nothing starts again.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(net.status.borrow().state, ConnState::Disconnected);
    net.shutdown().await;
}

/// After `Disconnect` the net thread queues nothing, though the broker still sends: a second
/// client floods the topic with QoS 0 messages before, during and after the disconnect. The queue
/// is empty at `Disconnected`, stays empty, and the byte count is 0.
#[tokio::test(flavor = "current_thread")]
async fn nothing_is_queued_after_disconnect() {
    require_broker();
    let p = prefix();
    let topic = format!("{p}/flood");
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, profile(&[&topic])).await;
    assert!(matches!(
        next_event(&mut net).await,
        Event::Subscribed { result: Ok(_), .. }
    ));
    let mut publisher = Publisher::connect("127.0.0.1", 18830)
        .await
        .expect("second client cannot connect");
    // Sends a burst, and lets the net thread run.
    async fn burst(publisher: &mut Publisher, topic: &str) {
        for _ in 0..20 {
            publisher
                .send(topic, Bytes::from(vec![b'x'; 1024]), Qos::AtMostOnce)
                .await
                .expect("publish");
        }
        tokio::task::yield_now().await;
    }
    let state = |net: &Net| net.status.borrow().state.clone();
    for _ in 0..50 {
        burst(&mut publisher, &topic).await;
    }
    let first = next_incoming(&mut net).await;
    assert_eq!(&*first.topic, topic);
    net.disconnect();
    // From `Disconnecting` on, the net thread queues nothing. Empty the queue then. The flood
    // goes on until the end.
    while !matches!(
        state(&net),
        ConnState::Disconnecting | ConnState::Disconnected
    ) {
        burst(&mut publisher, &topic).await;
    }
    while let Ok(m) = net.incoming.try_recv() {
        net.release(&m);
    }
    let end = tokio::time::Instant::now() + WAIT;
    while state(&net) != ConnState::Disconnected {
        assert!(tokio::time::Instant::now() < end, "no Disconnected");
        burst(&mut publisher, &topic).await;
    }
    // The flood goes on after the end too.
    let after = tokio::time::Instant::now() + Duration::from_millis(200);
    while tokio::time::Instant::now() < after {
        burst(&mut publisher, &topic).await;
    }
    assert!(
        net.incoming.try_recv().is_err(),
        "a message came after Disconnected"
    );
    assert_eq!(net.queued_bytes(), 0);
    publisher.shutdown().await;
    net.shutdown().await;
}
