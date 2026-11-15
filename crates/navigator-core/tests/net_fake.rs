//! Tests of `Net` against a fake broker (no real broker needed): the "Robust connection" rules of
//! the ntex plan for TCP, TLS, WS and WSS, each for MQTT 3.1.1 and MQTT 5. The fake broker sends
//! and withholds bytes on purpose. Every bound is the bound of the plan. The timers are the
//! production values.

#[path = "common/fake.rs"]
mod fake;

use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use bytes::Bytes;
use fake::{Fake, Options, ws_frame};
use navigator_core::model::{MsgProps, Qos};
use navigator_core::net::{
    Command, ConnState, ConnectConfig, Event, Net, NetLimits, Outgoing, Profile, Protocol,
    PublishResult, Status, Transport,
};

/// The profile for a fake broker. TLS trusts the fake's CA.
fn config(f: &Fake, t: Transport, v: Protocol, keepalive_secs: u16) -> ConnectConfig {
    let mut c = ConnectConfig::from(Profile {
        host: "127.0.0.1".into(),
        port: f.port,
        transport: t,
        ws_path: "/mqtt".into(),
        ca_file: Some(f.certs.ca_file.clone()),
        verify_tls: true,
        keepalive_secs,
        subscriptions: vec![],
        protocol: v,
        ..Profile::default()
    });
    c.protocol = v;
    c
}

async fn status_where(net: &mut Net, limit: Duration, f: impl Fn(&Status) -> bool) -> Status {
    tokio::time::timeout(limit, net.status.wait_for(|s| f(s)))
        .await
        .expect("the state did not come in time")
        .expect("status")
        .clone()
}

async fn connect(net: &mut Net, c: ConnectConfig) {
    net.send(Command::Connect(Box::new(c))).expect("send");
    status_where(net, Duration::from_secs(10), |s| {
        s.state == ConnState::Connected
    })
    .await;
}

fn big_publish(op_id: u64, qos: Qos) -> Command {
    Command::Publish(Box::new(Outgoing {
        op_id,
        topic: "t/big".into(),
        payload: Bytes::from(vec![7u8; 900_000]),
        qos,
        retain: false,
        props: MsgProps::default(),
    }))
}

/// The next publish result. Other events are skipped.
async fn next_published(net: &mut Net) -> (u64, PublishResult) {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), net.events.recv()).await {
            Ok(Some(Event::Published { op_id, result, .. })) => return (op_id, result),
            Ok(Some(_)) => {}
            other => panic!("expected a publish result, got {other:?}"),
        }
    }
}

/// Takes events until none comes for 500 ms. With a broker that reads nothing, the publish
/// results stop when the write buffer is full: the sends that have no result wait for the write.
async fn drain_until_quiet(
    net: &mut Net,
    started: Instant,
    results: &mut Vec<(Duration, PublishResult)>,
) {
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(500), net.events.recv()).await
    {
        if let Event::Published { result, .. } = event {
            results.push((started.elapsed(), result));
        }
    }
}

/// Makes the 8 tests of one case: MQTT 3.1.1 and MQTT 5 on TCP, TLS, WS and WSS.
macro_rules! all_wires {
    ($case:ident) => {
        mod $case {
            use super::*;
            #[tokio::test(flavor = "current_thread")]
            async fn v3_tcp() {
                super::$case(Transport::Tcp, Protocol::V311).await
            }
            #[tokio::test(flavor = "current_thread")]
            async fn v3_tls() {
                super::$case(Transport::Tls, Protocol::V311).await
            }
            #[tokio::test(flavor = "current_thread")]
            async fn v3_ws() {
                super::$case(Transport::Ws, Protocol::V311).await
            }
            #[tokio::test(flavor = "current_thread")]
            async fn v3_wss() {
                super::$case(Transport::Wss, Protocol::V311).await
            }
            #[tokio::test(flavor = "current_thread")]
            async fn v5_tcp() {
                super::$case(Transport::Tcp, Protocol::V5).await
            }
            #[tokio::test(flavor = "current_thread")]
            async fn v5_tls() {
                super::$case(Transport::Tls, Protocol::V5).await
            }
            #[tokio::test(flavor = "current_thread")]
            async fn v5_ws() {
                super::$case(Transport::Ws, Protocol::V5).await
            }
            #[tokio::test(flavor = "current_thread")]
            async fn v5_wss() {
                super::$case(Transport::Wss, Protocol::V5).await
            }
        }
    };
}

const OP_TIMEOUT_END_TEXT: &str = "The broker did not answer for 10 s. Connect again.";
const WRITE_TIMEOUT_TEXT: &str = "The broker did not read our data for 10 s.";
const CONNECTION_CLOSED_TEXT: &str =
    "The connection closed (the broker did not answer, or the network failed).";

/// The broker takes CONNECT and then stops reading. Our sends fill the buffers and block. Plan,
/// "A peer that stops reading cannot block us": a blocked publish ends with `TimedOut` in at most
/// 11 s, and the attempt ends at most write timeout + 2.5 s = 12.5 s after the send began. The
/// operation timer (10 s from the submit, final) and the write timeout (10 s, temporary) race.
/// Either one is right, so the test accepts exactly these two ends.
async fn stalled_write(t: Transport, v: Protocol) {
    stalled_write_with(t, v, 0).await;
}

/// With keep alive 2 s the PINGREQ sits behind the blocked data. The bound is 2 x keep alive x
/// 1.125 + write timeout + 2.5 s = 17 s (plan, row "A dead peer is found"); the same ends are
/// right.
async fn stalled_write_with(t: Transport, v: Protocol, keep_alive: u16) {
    let bound = if keep_alive == 0 { 12.5 } else { 17.0 };
    let f = Fake::start(t, v, Options::default(), |_, mut conn| {
        if conn.accept_connect().is_ok() {
            conn.stop_reading(Duration::from_secs(30));
        }
    });
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, t, v, keep_alive)).await;
    const SENDS: usize = 24;
    let started = Instant::now();
    for op_id in 0..SENDS as u64 {
        net.send(big_publish(op_id, Qos::AtMostOnce)).expect("send");
    }
    // About 20 MB are far more than the socket buffers hold: a send must still be blocked.
    let mut results = Vec::new();
    drain_until_quiet(&mut net, started, &mut results).await;
    // QoS 0 results come only when the write buffer takes the message. The broker reads nothing,
    // so fewer results than sends means that at least one send waits for the blocked write.
    assert!(results.len() < SENDS, "no send is blocked: {results:?}");
    assert!(
        results.iter().all(|(_, r)| *r == PublishResult::Sent),
        "{results:?}"
    );

    let ended = status_where(&mut net, Duration::from_secs_f64(bound + 0.5), |s| {
        s.state != ConnState::Connected
    })
    .await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(9) && took <= Duration::from_secs_f64(bound),
        "the attempt ended after {took:?}, not between 9 s and {bound} s"
    );
    let by_write_timeout = match &ended.state {
        ConnState::Failed(text) if text == OP_TIMEOUT_END_TEXT => false,
        ConnState::Reconnecting { attempt: 1, reason } if reason == WRITE_TIMEOUT_TEXT => true,
        other => panic!("unexpected end of the attempt: {other:?}"),
    };
    while results.len() < SENDS {
        match tokio::time::timeout(Duration::from_secs(2), net.events.recv()).await {
            Ok(Some(Event::Published { result, .. })) => results.push((started.elapsed(), result)),
            Ok(Some(_)) => {}
            other => panic!("expected a publish result, got {other:?}"),
        }
    }
    let timed_out: Vec<_> = results
        .iter()
        .filter(|(_, r)| *r == PublishResult::TimedOut)
        .collect();
    if by_write_timeout {
        assert!(timed_out.is_empty(), "{results:?}");
        assert!(
            results
                .iter()
                .any(|(_, r)| *r == PublishResult::LostOnDisconnect),
            "no blocked send reported LostOnDisconnect: {results:?}"
        );
    } else {
        // A blocked publish reports `TimedOut` in at most 11 s. More than one can be blocked.
        assert!(!timed_out.is_empty(), "{results:?}");
        assert!(
            timed_out
                .iter()
                .all(|(at, _)| *at <= Duration::from_secs(11)),
            "{timed_out:?}"
        );
    }
    assert!(
        results.iter().all(|(_, r)| matches!(
            r,
            PublishResult::Sent | PublishResult::TimedOut | PublishResult::LostOnDisconnect
        )),
        "{results:?}"
    );
    net.disconnect();
    net.shutdown().await;
}

all_wires!(stalled_write);

/// The write timeout alone. ntex starts its write timeout (10 s) when the write buffer goes above
/// the high watermark and does not drain. The operation timer (10 s) of a send starts at its
/// submit. The fake broker reads nothing after CONNECT. The test sends QoS 0 messages one at a
/// time. After each result it waits 4.5 s, so the write timeout of the stalled buffer is already
/// running. The first send that gets no result in 0.7 s is the blocked one.
///
/// Let T0 be the time of the stall. The write timeout ends the attempt at T0 + 10 s to T0 + 12 s
/// (the timer can be up to 1 s late, then up to 1 s of I/O shutdown). The blocked send comes at
/// about T0 + 4.5 s, so the write timeout ends the attempt 5.5 s to 7.5 s after it. The operation
/// timer of the blocked send ends it at 10 s after the send. So only the write timeout can end
/// the attempt less than 9.8 s after that send: the end is temporary
/// (`Reconnecting`) with the write timeout text, and no send reports `TimedOut`. Without the write
/// timeout, the operation timer ends the attempt with the final operation text and the test fails
/// (mutation check in the S3 report).
async fn write_timeout_alone(v: Protocol) {
    let f = Fake::start(Transport::Tcp, v, Options::default(), |_, mut conn| {
        if conn.accept_connect().is_ok() {
            conn.stop_reading(Duration::from_secs(60));
        }
    });
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, Transport::Tcp, v, 0)).await;
    let mut blocked_at = None;
    'sends: for op_id in 0..60u64 {
        let submitted = Instant::now();
        net.send(big_publish(op_id, Qos::AtMostOnce)).expect("send");
        let deadline = tokio::time::Instant::now() + Duration::from_millis(700);
        loop {
            match tokio::time::timeout_at(deadline, net.events.recv()).await {
                Ok(Some(Event::Published { result, .. })) => {
                    assert_eq!(result, PublishResult::Sent);
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => panic!("events closed"),
                Err(_) => {
                    blocked_at = Some(submitted);
                    break 'sends;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(4_500)).await;
    }
    let blocked_at = blocked_at.expect("the write never blocked");
    let ended = status_where(&mut net, Duration::from_secs(12), |s| {
        s.state != ConnState::Connected
    })
    .await;
    let took = blocked_at.elapsed();
    assert!(
        took < Duration::from_millis(9_800),
        "the attempt ended {took:?} after the blocked send ({ended:?}): not before its operation timer"
    );
    assert!(
        matches!(&ended.state, ConnState::Reconnecting { attempt: 1, reason } if reason == WRITE_TIMEOUT_TEXT),
        "{ended:?}"
    );
    let mut results = Vec::new();
    while results.is_empty() {
        match tokio::time::timeout(Duration::from_secs(2), net.events.recv()).await {
            Ok(Some(Event::Published { result, .. })) => results.push(result),
            Ok(Some(_)) => {}
            other => panic!("expected the result of the blocked send, got {other:?}"),
        }
    }
    assert_eq!(results, [PublishResult::LostOnDisconnect]);
    net.disconnect();
    net.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn v3_write_timeout_alone_is_a_temporary_failure() {
    write_timeout_alone(Protocol::V311).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v5_write_timeout_alone_is_a_temporary_failure() {
    write_timeout_alone(Protocol::V5).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v3_stalled_write_with_keep_alive_2_s() {
    stalled_write_with(Transport::Tcp, Protocol::V311, 2).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v5_stalled_write_with_keep_alive_2_s() {
    stalled_write_with(Transport::Tcp, Protocol::V5, 2).await;
}

/// Like `all_wires!`, for TCP and TLS only (the cases of the frame read rate).
macro_rules! stream_wires {
    ($case:ident $(, #[$attr:meta])*) => {
        mod $case {
            use super::*;
            $(#[$attr])*
            #[tokio::test(flavor = "current_thread")]
            async fn v3_tcp() {
                super::$case(Transport::Tcp, Protocol::V311).await
            }
            $(#[$attr])*
            #[tokio::test(flavor = "current_thread")]
            async fn v3_tls() {
                super::$case(Transport::Tls, Protocol::V311).await
            }
            $(#[$attr])*
            #[tokio::test(flavor = "current_thread")]
            async fn v5_tcp() {
                super::$case(Transport::Tcp, Protocol::V5).await
            }
            $(#[$attr])*
            #[tokio::test(flavor = "current_thread")]
            async fn v5_tls() {
                super::$case(Transport::Tls, Protocol::V5).await
            }
        }
    };
}

fn small_publish(op_id: u64) -> Command {
    Command::Publish(Box::new(Outgoing {
        op_id,
        topic: "t/small".into(),
        payload: Bytes::from_static(b"x"),
        qos: Qos::AtMostOnce,
        retain: false,
        props: MsgProps::default(),
    }))
}

/// The broker reads PINGREQ and never answers. Keep alive ends the attempt: exactly one PINGREQ
/// at about the keep alive, and the end within 2 x keep alive x 1.125 + 1 s (plan, "A dead peer
/// is found"; over WS and WSS the close waits up to 1 s more for the peer: + 2 s).
async fn missing_pingresp(t: Transport, v: Protocol) {
    missing_pingresp_with(t, v, 2).await;
}

async fn missing_pingresp_with(t: Transport, v: Protocol, keep_alive: u16) {
    missing_pingresp_full(t, v, keep_alive, false).await;
}

/// `publishes`: the client also sends a QoS 0 message every 300 ms. They do not hide the dead
/// peer: the end comes at the same time and there is still one PINGREQ.
async fn missing_pingresp_full(t: Transport, v: Protocol, keep_alive: u16, publishes: bool) {
    let ka = f64::from(keep_alive);
    let extra = if matches!(t, Transport::Ws | Transport::Wss) {
        2.0
    } else {
        1.0
    };
    let bound = 2.0 * ka * 1.125 + extra;
    let pings = Arc::new(Mutex::new(Vec::<Instant>::new()));
    let seen = pings.clone();
    let hold = Duration::from_secs_f64(bound + 10.0);
    let f = Fake::start(t, v, Options::default(), move |_, mut conn| {
        if conn.accept_connect().is_ok() {
            conn.read_until_close(hold, |head, _| {
                if head == 0xc0 {
                    seen.lock().expect("lock").push(Instant::now());
                }
            });
        }
    });
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, t, v, keep_alive)).await;
    let up = Instant::now();
    let lost = async {
        loop {
            if publishes {
                let _ = net.send(small_publish(1));
            }
            if matches!(net.status.borrow().state, ConnState::Reconnecting { .. }) {
                return net.status.borrow().clone();
            }
            assert!(up.elapsed().as_secs_f64() < bound + 2.0, "no end in time");
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    .await;
    let took = up.elapsed().as_secs_f64();
    // Lower bound: the second tick, 2 x keep alive, less 0.1 s because `up` is taken after the
    // status event. Upper bound: the plan bound, plus the 0.3 s step of the poll loop above.
    assert!(
        took <= bound + 0.3 && took >= 2.0 * ka - 0.1,
        "{took} s, bound {bound} s"
    );
    assert!(
        matches!(&lost.state, ConnState::Reconnecting { attempt: 1, reason } if reason == CONNECTION_CLOSED_TEXT),
        "{lost:?}"
    );
    let pings = pings.lock().expect("lock").clone();
    assert_eq!(pings.len(), 1, "exactly one PINGREQ before the end");
    let first = pings[0].duration_since(up).as_secs_f64();
    // One timer step (up to 1/8 of the delay) can come on top of the keep alive. 0.2 s below
    // and 0.5 s above cover the status event delay and the fake broker read delay.
    assert!(
        (ka - 0.2..=ka * 1.125 + 0.5).contains(&first),
        "the PINGREQ came after {first} s"
    );
    net.disconnect();
    net.shutdown().await;
}

all_wires!(missing_pingresp);

/// Keep alive 60 s and 70 s (70 s is in the 8 s timer step range and checks the rounding): the
/// plan bound 2 x keep alive x 1.125 + 1 s is 136 s and 158.5 s. These run more than 2 minutes,
/// so they are `#[ignore]`: run them with `--ignored`.
macro_rules! long_keep_alive {
    ($name:ident, $t:expr, $v:expr, $ka:expr) => {
        #[tokio::test(flavor = "current_thread")]
        #[ignore = "runs 2 to 2.5 minutes"]
        async fn $name() {
            missing_pingresp_with($t, $v, $ka).await;
        }
    };
}

#[tokio::test(flavor = "current_thread")]
async fn v3_missing_pingresp_with_publishes() {
    missing_pingresp_full(Transport::Tcp, Protocol::V311, 2, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v5_missing_pingresp_with_publishes() {
    missing_pingresp_full(Transport::Tcp, Protocol::V5, 2, true).await;
}

long_keep_alive!(v3_keep_alive_60_s, Transport::Tcp, Protocol::V311, 60);
long_keep_alive!(v5_keep_alive_60_s, Transport::Tcp, Protocol::V5, 60);
long_keep_alive!(v3_keep_alive_70_s, Transport::Tcp, Protocol::V311, 70);
long_keep_alive!(v5_keep_alive_70_s, Transport::Tcp, Protocol::V5, 70);

const READ_TIMEOUT_TEXT: &str = "The broker stopped in the middle of a packet.";

/// TCP and TLS, keep alive 0: the broker sends a started packet and stops. A half packet ends at
/// the first rate check of the frame read rate (10 s), plus timer step, shutdown and rounding: in
/// at most 13 s (plan, "A started packet that stops cannot block us"). `first` is what the broker
/// sends: half a fixed header, or a PUBLISH header and 10 bytes of its payload.
async fn half_frame(t: Transport, v: Protocol, first: Vec<u8>) {
    let f = Fake::start(t, v, Options::default(), move |_, mut conn| {
        if conn.accept_connect().is_ok() && conn.send_raw(&first).is_ok() {
            conn.hold(Duration::from_secs(20));
        }
    });
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, t, v, 0)).await;
    let started = Instant::now();
    let lost = status_where(&mut net, Duration::from_secs(14), |s| {
        s.state != ConnState::Connected
    })
    .await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(9) && took <= Duration::from_secs(13),
        "ended after {took:?}"
    );
    assert!(
        matches!(&lost.state, ConnState::Reconnecting { attempt: 1, reason } if reason == READ_TIMEOUT_TEXT),
        "{lost:?}"
    );
    net.disconnect();
    net.shutdown().await;
}

async fn half_fixed_header(t: Transport, v: Protocol) {
    // The first byte of a PUBLISH, and a length byte with the continuation bit.
    half_frame(t, v, vec![0x30, 0x80]).await;
}
stream_wires!(half_fixed_header);

async fn half_payload(t: Transport, v: Protocol) {
    // A PUBLISH of 1000 bytes: the header and 10 bytes (topic "t/a" and 5 payload bytes).
    // MQTT 5 adds an empty property length (0x00) before the payload.
    let mut first = vec![0x30, 0xe8, 0x07, 0x00, 0x03, b't', b'/', b'a'];
    if v == Protocol::V5 {
        first.push(0x00);
    }
    first.extend([b'x'; 5]);
    half_frame(t, v, first).await;
}
stream_wires!(half_payload);

/// WS and WSS: the WebSocket layer keeps an incomplete frame until it is whole, so the frame read
/// rate does not run. Keep alive finds the stall: a binary frame that stops in the middle ends in
/// at most 2 x keep alive x 1.125 + 2 s = 6.5 s (plan, same table row).
async fn half_binary_frame(t: Transport, v: Protocol) {
    const KEEP_ALIVE: u16 = 2;
    let f = Fake::start(t, v, Options::default(), move |_, mut conn| {
        if conn.accept_connect().is_ok() {
            // A frame header for 100 bytes, and 10 of them.
            let mut part = ws_frame(true, 2, &[0u8; 100]);
            part.truncate(2 + 10);
            if conn.send_raw(&part).is_ok() {
                conn.hold(Duration::from_secs(12));
            }
        }
    });
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, t, v, KEEP_ALIVE)).await;
    let started = Instant::now();
    let lost = status_where(&mut net, Duration::from_secs(8), |s| {
        s.state != ConnState::Connected
    })
    .await;
    let took = started.elapsed().as_secs_f64();
    assert!((3.5..=6.5).contains(&took), "ended after {took} s");
    assert!(
        matches!(&lost.state, ConnState::Reconnecting { attempt: 1, reason } if reason == "The connection timed out."),
        "{lost:?}"
    );
    net.disconnect();
    net.shutdown().await;
}

macro_rules! ws_wires {
    ($case:ident) => {
        mod $case {
            use super::*;
            #[tokio::test(flavor = "current_thread")]
            async fn v3_ws() {
                super::$case(Transport::Ws, Protocol::V311).await
            }
            #[tokio::test(flavor = "current_thread")]
            async fn v3_wss() {
                super::$case(Transport::Wss, Protocol::V311).await
            }
            #[tokio::test(flavor = "current_thread")]
            async fn v5_ws() {
                super::$case(Transport::Ws, Protocol::V5).await
            }
            #[tokio::test(flavor = "current_thread")]
            async fn v5_wss() {
                super::$case(Transport::Wss, Protocol::V5).await
            }
        }
    };
}
ws_wires!(half_binary_frame);

/// A payload that arrives at more than 256 bytes in each 10 s and never completes ends at the 60 s
/// cap (plan, same table row: at most 6 x 11 + 2 = 68 s). It runs 60 s, so it is `#[ignore]`: run
/// it with `--ignored`.
async fn trickle(t: Transport, v: Protocol) {
    let f = Fake::start(t, v, Options::default(), move |_, mut conn| {
        if conn.accept_connect().is_err() {
            return;
        }
        // A PUBLISH of 100 000 bytes: topic "t/a", for MQTT 5 an empty property list, then
        // payload bytes that arrive 512 at a time every 5 s.
        let mut head = vec![0x30, 0xa0, 0x8d, 0x06, 0x00, 0x03, b't', b'/', b'a'];
        if v == Protocol::V5 {
            head.push(0x00);
        }
        if conn.send_raw(&head).is_err() {
            return;
        }
        for _ in 0..16 {
            std::thread::sleep(Duration::from_secs(5));
            if conn.send_raw(&[b'x'; 512]).is_err() {
                return;
            }
        }
        conn.hold(Duration::from_secs(5));
    });
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, t, v, 0)).await;
    let started = Instant::now();
    let lost = status_where(&mut net, Duration::from_secs(70), |s| {
        s.state != ConnState::Connected
    })
    .await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(59) && took <= Duration::from_secs(68),
        "ended after {took:?}"
    );
    assert!(
        matches!(&lost.state, ConnState::Reconnecting { attempt: 1, reason } if reason == READ_TIMEOUT_TEXT),
        "{lost:?}"
    );
    net.disconnect();
    net.shutdown().await;
}
stream_wires!(trickle, #[ignore = "runs 60 s: the cap of the frame read rate"]);

/// The broker takes a QoS 1 PUBLISH and never answers. A disconnect ends the wait at once: the
/// publish is `LostOnDisconnect` and the state is `Disconnected` in at most 3 s (plan, "Disconnect
/// and stop are prompt").
async fn ack_wait_cancelled_by_close(t: Transport, v: Protocol) {
    let (tx, rx) = mpsc::channel::<()>();
    let tx = Mutex::new(tx);
    let f = Fake::start(t, v, Options::default(), move |_, mut conn| {
        if conn.accept_connect().is_ok() {
            conn.read_until_close(Duration::from_secs(15), |head, _| {
                if head & 0xf0 == 0x30 {
                    let _ = tx.lock().expect("lock").send(());
                }
            });
        }
    });
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, t, v, 0)).await;
    net.send(Command::Publish(Box::new(Outgoing {
        op_id: 7,
        topic: "t/ack".into(),
        payload: Bytes::from_static(b"x"),
        qos: Qos::AtLeastOnce,
        retain: false,
        props: MsgProps::default(),
    })))
    .expect("send");
    rx.recv_timeout(Duration::from_secs(5))
        .expect("the broker got the PUBLISH");
    let started = Instant::now();
    net.disconnect();
    status_where(&mut net, Duration::from_secs(3), |s| {
        s.state == ConnState::Disconnected
    })
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    loop {
        match tokio::time::timeout(Duration::from_secs(2), net.events.recv()).await {
            Ok(Some(Event::Published { op_id, result, .. })) => {
                assert_eq!((op_id, result), (7, PublishResult::LostOnDisconnect));
                break;
            }
            Ok(Some(_)) => {}
            other => panic!("expected the publish result, got {other:?}"),
        }
    }
    net.shutdown().await;
}
all_wires!(ack_wait_cancelled_by_close);

/// A server that finishes the TLS handshake and never answers the WebSocket upgrade. ntex ends
/// the upgrade after 5 s: a temporary timeout (plan, row "One deadline"; S1 finding 9). The WS
/// case without TLS is `a_stalled_websocket_upgrade_is_a_temporary_timeout` in `net.rs`.
async fn stalled_upgrade_over_wss(v: Protocol) {
    let f = Fake::start(Transport::Wss, v, Options::default(), |_, mut conn| {
        conn.hold(Duration::from_secs(12));
    });
    let mut net = Net::start(NetLimits::default());
    net.send(Command::Connect(Box::new(config(&f, Transport::Wss, v, 0))))
        .expect("send");
    let started = Instant::now();
    let lost = status_where(&mut net, Duration::from_secs(11), |s| {
        matches!(
            s.state,
            ConnState::Reconnecting { .. } | ConnState::Failed(_)
        )
    })
    .await;
    assert!(
        matches!(&lost.state, ConnState::Reconnecting { attempt: 1, reason } if reason == "The connection timed out."),
        "{lost:?}"
    );
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    net.disconnect();
    net.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn v3_a_stalled_upgrade_over_wss_is_a_temporary_timeout() {
    stalled_upgrade_over_wss(Protocol::V311).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v5_a_stalled_upgrade_over_wss_is_a_temporary_timeout() {
    stalled_upgrade_over_wss(Protocol::V5).await;
}

const CLIENT_CERT_TEXT: &str =
    "The broker needs a client certificate. Add your certificate and key.";

/// The Mosquitto config has no mTLS listener over WebSocket, so the fake serves one. With the
/// client certificate the connect works; without it the attempt fails with the client-certificate
/// text.
async fn mtls_over_wss(v: Protocol) {
    let f = Fake::start(
        Transport::Wss,
        v,
        Options {
            require_client_cert: true,
            ..Options::default()
        },
        |_, mut conn| {
            if conn.accept_connect().is_ok() {
                conn.hold(Duration::from_secs(5));
            }
        },
    );
    let mut net = Net::start(NetLimits::default());
    let mut with = config(&f, Transport::Wss, v, 0);
    with.profile.client_cert = Some(f.certs.client_cert.clone());
    with.profile.client_key = Some(f.certs.client_key.clone());
    connect(&mut net, with).await;
    net.disconnect();
    status_where(&mut net, Duration::from_secs(3), |s| {
        s.state == ConnState::Disconnected
    })
    .await;

    net.send(Command::Connect(Box::new(config(&f, Transport::Wss, v, 0))))
        .expect("send");
    let s = status_where(&mut net, Duration::from_secs(10), |s| {
        matches!(s.state, ConnState::Failed(_) | ConnState::Connected)
    })
    .await;
    assert_eq!(s.state, ConnState::Failed(CLIENT_CERT_TEXT.into()), "{s:?}");
    net.shutdown().await;
}

const SETUP_CLOSE_TEXT: &str = "The broker closed the connection during set-up. If the broker needs a client certificate, add one to the connection.";

/// A broker that needs a client certificate resets the connection after it rejects the client
/// (the data of the client is unread), so the alert can be lost. The text must name the likely
/// cause, and the attempt must stay temporary (a broker restart recovers).
async fn missing_cert_with_reset(t: Transport, v: Protocol) {
    let f = Fake::start(
        t,
        v,
        Options {
            require_client_cert: true,
            reset_on_handshake_failure: true,
        },
        |_, _| {},
    );
    let mut net = Net::start(NetLimits::default());
    net.send(Command::Connect(Box::new(config(&f, t, v, 0))))
        .expect("send");
    let s = status_where(&mut net, Duration::from_secs(10), |s| {
        matches!(
            s.state,
            ConnState::Failed(_) | ConnState::Reconnecting { .. }
        )
    })
    .await;
    match &s.state {
        ConnState::Reconnecting { reason, .. } => assert_eq!(reason, SETUP_CLOSE_TEXT),
        other => panic!("{other:?}"),
    }
    net.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn v3_tls_missing_cert_with_reset() {
    missing_cert_with_reset(Transport::Tls, Protocol::V311).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v5_tls_missing_cert_with_reset() {
    missing_cert_with_reset(Transport::Tls, Protocol::V5).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v3_wss_missing_cert_with_reset() {
    missing_cert_with_reset(Transport::Wss, Protocol::V311).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v5_wss_missing_cert_with_reset() {
    missing_cert_with_reset(Transport::Wss, Protocol::V5).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v3_mtls_over_wss() {
    mtls_over_wss(Protocol::V311).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v5_mtls_over_wss() {
    mtls_over_wss(Protocol::V5).await;
}

/// A disconnect while a half frame waits is prompt: `Disconnected` in at most 3 s (graceful close,
/// at most 2 s, then `force_close()`), and the attempt did not fail (plan, "Disconnect and stop are
/// prompt").
async fn disconnect_while_a_half_frame_waits(t: Transport, v: Protocol) {
    // `go`: the test saw `Connected`. `sent`: the broker wrote the half frame.
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    let (sent_tx, sent_rx) = mpsc::channel::<()>();
    let sent_tx = Mutex::new(sent_tx);
    let f = Fake::start(t, v, Options::default(), move |_, mut conn| {
        if conn.accept_connect().is_ok() {
            if go_rx
                .lock()
                .expect("lock")
                .recv_timeout(Duration::from_secs(5))
                .is_err()
            {
                return;
            }
            let half = if matches!(t, Transport::Ws | Transport::Wss) {
                let mut part = ws_frame(true, 2, &[0u8; 100]);
                part.truncate(2 + 10);
                part
            } else {
                vec![0x30, 0x80]
            };
            if conn.send_raw(&half).is_ok() {
                let _ = sent_tx.lock().expect("lock").send(());
                conn.hold(Duration::from_secs(10));
            }
        }
    });
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, t, v, 0)).await;
    go_tx.send(()).expect("broker thread");
    sent_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the broker wrote the half frame");
    let started = Instant::now();
    net.disconnect();
    let mut failed = false;
    while net.status.borrow().state != ConnState::Disconnected {
        failed |= matches!(net.status.borrow().state, ConnState::Failed(_));
        tokio::time::timeout(Duration::from_secs(3), net.status.changed())
            .await
            .expect("Disconnected comes in 3 s")
            .expect("status");
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert!(!failed, "the attempt failed before it was disconnected");
    net.shutdown().await;
}
all_wires!(disconnect_while_a_half_frame_waits);

/// A PUBLISH whose Remaining Length is 2 MiB, with a 1 MiB limit: the connection ends at the fixed
/// header, at once, before the payload arrives (plan, "Bounded memory"). The text is the size
/// text. The S1 spike (deleted in S5) measured the RSS; this test checks the time and the reason.
async fn big_header(t: Transport, v: Protocol) {
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    let f = Fake::start(t, v, Options::default(), move |_, mut conn| {
        if conn.accept_connect().is_err() {
            return;
        }
        // The test sends `go` after it saw `Connected`: the end then comes at once.
        if go_rx
            .lock()
            .expect("lock")
            .recv_timeout(Duration::from_secs(5))
            .is_err()
        {
            return;
        }
        // A valid variable header (topic "t/b", for MQTT 5 an empty property list), so only the
        // size check can close the connection.
        let mut hdr = vec![0x30, 0x80, 0x80, 0x80, 0x01]; // Remaining Length 2 MiB
        hdr.extend([0, 3, b't', b'/', b'b']);
        if v == Protocol::V5 {
            hdr.push(0);
        }
        hdr.extend([7u8; 64]);
        if conn.send_raw(&hdr).is_ok() {
            conn.hold(Duration::from_secs(5));
        }
    });
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, t, v, 0)).await;
    let started = Instant::now();
    go_tx.send(()).expect("broker thread");
    let ended = status_where(&mut net, Duration::from_secs(2), |s| {
        s.state != ConnState::Connected
    })
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        ended.state,
        ConnState::Failed("The broker sent a packet that is larger than the allowed size.".into())
    );
    net.disconnect();
    net.shutdown().await;
}
stream_wires!(big_header);

/// WS and WSS framing. A packet in three frames (binary, continuation, continuation) is
/// delivered. A Ping gets one Pong with the same payload; an unsolicited Pong is ignored. A Text
/// frame ends the connection (S1 finding 6).
async fn websocket_frames(t: Transport, v: Protocol) {
    let (tx, rx) = mpsc::channel::<Vec<(u8, Vec<u8>)>>();
    let tx = Mutex::new(tx);
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    let f = Fake::start(t, v, Options::default(), move |_, mut conn| {
        if conn.accept_connect().is_err() {
            return;
        }
        // PUBLISH QoS 0, topic "ws/1", payload "fragments", in three frames.
        let mut body = vec![0, 4, b'w', b's', b'/', b'1'];
        if v == Protocol::V5 {
            body.push(0);
        }
        body.extend(b"fragments");
        let packet = fake::pkt(0x30, &body);
        let (a, rest) = packet.split_at(3);
        let (b, c) = rest.split_at(4);
        let mut frames = ws_frame(false, 2, a);
        frames.extend(ws_frame(false, 0, b));
        // A Ping between the fragments is allowed.
        frames.extend(ws_frame(true, 9, b"ping"));
        frames.extend(ws_frame(true, 0, c));
        // An unsolicited Pong.
        frames.extend(ws_frame(true, 10, b"unasked"));
        if conn.send_raw(&frames).is_err() {
            return;
        }
        // Collect what the client sends: Pong frames, then a Close after the Text frame.
        let mut got = Vec::new();
        // The test sends `go` after it got the packet, so the Pong frames are already on the way.
        let _ = go_rx
            .lock()
            .expect("lock")
            .recv_timeout(Duration::from_secs(5));
        let _ = conn.send_raw(&ws_frame(true, 1, b"text"));
        while let Ok(frame) = conn.ws_read() {
            let close = frame.0 == 8;
            got.push(frame);
            if close {
                break;
            }
        }
        let _ = tx.lock().expect("lock").send(got);
    });
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, t, v, 0)).await;
    let m = tokio::time::timeout(Duration::from_secs(5), net.incoming.recv())
        .await
        .expect("the fragmented packet is delivered")
        .expect("open");
    net.release(&m);
    assert_eq!((&*m.topic, &m.payload[..]), ("ws/1", &b"fragments"[..]));
    go_tx.send(()).expect("broker thread");
    // The Text frame ends the session. ntex reports it as an I/O error without a retry (S1
    // finding 6). The text is the one that ntex gives.
    let ended = status_where(&mut net, Duration::from_secs(5), |s| {
        s.state != ConnState::Connected
    })
    .await;
    assert_eq!(
        ended.state,
        ConnState::Failed("Network error: WebSockets Text frames are not supported".into())
    );
    let frames = rx.recv_timeout(Duration::from_secs(5)).expect("frames");
    let pongs: Vec<_> = frames.iter().filter(|(op, _)| *op == 10).collect();
    assert_eq!(pongs.len(), 1, "{frames:?}");
    assert_eq!(pongs[0].1, b"ping");
    net.disconnect();
    net.shutdown().await;
}
ws_wires!(websocket_frames);

/// Silent CONNACK: the broker reads CONNECT and never answers. Our connect deadline (10 s) ends
/// it in at most 11 s (plan, row "One deadline"), with the text for a late CONNACK, as a
/// temporary error.
async fn silent_connack(t: Transport, v: Protocol) {
    let f = Fake::start(t, v, Options::default(), |_, mut conn| {
        let _ = conn.read_packet();
        conn.hold(Duration::from_secs(14));
    });
    let mut net = Net::start(NetLimits::default());
    let started = Instant::now();
    net.send(Command::Connect(Box::new(config(&f, t, v, 0))))
        .expect("send");
    let s = status_where(&mut net, Duration::from_secs(12), |s| {
        matches!(
            s.state,
            ConnState::Reconnecting { .. } | ConnState::Failed(_)
        )
    })
    .await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(9) && took <= Duration::from_secs(11),
        "{took:?}"
    );
    assert!(
        matches!(&s.state, ConnState::Reconnecting { attempt: 1, reason } if reason == "The broker did not answer the connection request."),
        "{s:?}"
    );
    net.disconnect();
    net.shutdown().await;
}
stream_wires!(silent_connack);

/// A server that accepts the TCP connection and never answers the TLS handshake. The TLS
/// handshake timeout of `ntex-tls` (5 s) ends it before our deadline: a temporary timeout (S1
/// finding 9).
async fn stalled_tls(t: Transport, v: Protocol) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let holder = std::thread::spawn(move || {
        let (_sock, _) = listener.accept().expect("accept");
        std::thread::sleep(Duration::from_secs(8));
    });
    let f = Fake::start(t, v, Options::default(), |_, _| {});
    let mut c = config(&f, t, v, 0);
    c.profile.port = port;
    let mut net = Net::start(NetLimits::default());
    let started = Instant::now();
    net.send(Command::Connect(Box::new(c))).expect("send");
    let s = status_where(&mut net, Duration::from_secs(9), |s| {
        matches!(
            s.state,
            ConnState::Reconnecting { .. } | ConnState::Failed(_)
        )
    })
    .await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(4) && took <= Duration::from_secs(7),
        "{took:?}"
    );
    assert!(
        matches!(&s.state, ConnState::Reconnecting { attempt: 1, reason } if reason == "The connection timed out."),
        "{s:?}"
    );
    net.disconnect();
    net.shutdown().await;
    holder.join().expect("holder");
}

macro_rules! tls_wires {
    ($case:ident) => {
        mod $case {
            use super::*;
            #[tokio::test(flavor = "current_thread")]
            async fn v3_tls() {
                super::$case(Transport::Tls, Protocol::V311).await
            }
            #[tokio::test(flavor = "current_thread")]
            async fn v3_wss() {
                super::$case(Transport::Wss, Protocol::V311).await
            }
            #[tokio::test(flavor = "current_thread")]
            async fn v5_tls() {
                super::$case(Transport::Tls, Protocol::V5).await
            }
            #[tokio::test(flavor = "current_thread")]
            async fn v5_wss() {
                super::$case(Transport::Wss, Protocol::V5).await
            }
        }
    };
}
tls_wires!(stalled_tls);

/// A loopback listener whose accept queue is full, so new SYNs are dropped: a dial hangs. Linux
/// behaviour; the tests that use it run on Linux only.
#[cfg(target_os = "linux")]
mod black_hole {
    use super::*;

    pub struct BlackHole {
        pub port: u16,
        _listener: tokio::net::TcpListener,
        _fill: Vec<tokio::net::TcpStream>,
    }

    pub async fn make() -> BlackHole {
        let sock = tokio::net::TcpSocket::new_v4().expect("socket");
        sock.bind("127.0.0.1:0".parse().expect("addr"))
            .expect("bind");
        let listener = sock.listen(1).expect("listen");
        let port = listener.local_addr().expect("addr").port();
        let mut fill = Vec::new();
        for _ in 0..8 {
            match tokio::time::timeout(
                Duration::from_millis(300),
                tokio::net::TcpStream::connect(("127.0.0.1", port)),
            )
            .await
            {
                Ok(Ok(s)) => fill.push(s),
                _ => break,
            }
        }
        let hangs = tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .is_err();
        assert!(hangs, "could not make a black hole: a connect still works");
        BlackHole {
            port,
            _listener: listener,
            _fill: fill,
        }
    }

    /// A dial to a black hole ends with our 10 s deadline, in at most 11 s.
    #[tokio::test(flavor = "current_thread")]
    async fn a_black_hole_dial_ends_with_the_connect_deadline() {
        let hole = make().await;
        let f = Fake::start(Transport::Tcp, Protocol::V5, Options::default(), |_, _| {});
        let mut c = config(&f, Transport::Tcp, Protocol::V5, 0);
        c.profile.port = hole.port;
        let mut net = Net::start(NetLimits::default());
        let started = Instant::now();
        net.send(Command::Connect(Box::new(c))).expect("send");
        let s = status_where(&mut net, Duration::from_secs(12), |s| {
            matches!(
                s.state,
                ConnState::Reconnecting { .. } | ConnState::Failed(_)
            )
        })
        .await;
        let took = started.elapsed();
        assert!(
            took >= Duration::from_secs(9) && took <= Duration::from_secs(11),
            "{took:?}"
        );
        assert!(
            matches!(&s.state, ConnState::Reconnecting { attempt: 1, reason } if reason == "The connection timed out."),
            "{s:?}"
        );
        net.disconnect();
        net.shutdown().await;
    }
}

/// MQTT 5: the client follows the Server Keep Alive of the CONNACK, not the 60 s of the profile.
/// 5 s (profile 60 s): the first PINGREQ comes at about 5 s. 0 (profile 2 s): no PINGREQ at all (plan, row "A dead
/// peer is found": "Server Keep Alive 0 and 5 s: the client follows it").
async fn server_keep_alive(server_secs: u16) {
    let pings = Arc::new(Mutex::new(Vec::<Instant>::new()));
    let seen = pings.clone();
    let f = Fake::start(
        Transport::Tcp,
        Protocol::V5,
        Options::default(),
        move |_, mut conn| {
            let [hi, lo] = server_secs.to_be_bytes();
            // CONNACK: flags, reason 0, 3 bytes of properties: Server Keep Alive (0x13).
            if conn
                .accept_connect_with(&[0x20, 6, 0, 0, 3, 0x13, hi, lo])
                .is_ok()
            {
                conn.read_until_close(Duration::from_secs(12), |head, _| {
                    if head == 0xc0 {
                        conn_ping(&seen);
                    }
                });
            }
        },
    );
    let mut net = Net::start(NetLimits::default());
    // For 0 the profile asks 2 s, so a client that ignored the broker would ping within 8 s.
    let profile_secs = if server_secs == 0 { 2 } else { 60 };
    connect(
        &mut net,
        config(&f, Transport::Tcp, Protocol::V5, profile_secs),
    )
    .await;
    let up = Instant::now();
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert_eq!(net.status.borrow().state, ConnState::Connected);
    let pings = pings.lock().expect("lock").clone();
    if server_secs == 0 {
        assert!(pings.is_empty(), "{pings:?}");
    } else {
        let first = pings.first().expect("a PINGREQ").duration_since(up);
        assert!(
            (Duration::from_millis(4_500)..=Duration::from_millis(6_500)).contains(&first),
            "{first:?}"
        );
    }
    net.disconnect();
    net.shutdown().await;
}

fn conn_ping(seen: &Arc<Mutex<Vec<Instant>>>) {
    seen.lock().expect("lock").push(Instant::now());
}

#[tokio::test(flavor = "current_thread")]
async fn v5_server_keep_alive_5_s_is_followed() {
    server_keep_alive(5).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v5_server_keep_alive_0_sends_no_ping() {
    server_keep_alive(0).await;
}

/// MQTT 5: the CONNACK announces a Maximum Packet Size. A PUBLISH of exactly that size is sent;
/// one byte more is refused before the send, with the text of `publish::validate_outgoing`. A v5
/// broker that announces none gets any size, as before (plan, row "Publish size", S1 finding 4).
#[tokio::test(flavor = "current_thread")]
async fn a_v5_publish_over_the_announced_maximum_packet_size_is_refused_before_the_send() {
    const LIMIT: usize = 4096;
    let (tx, rx) = mpsc::channel::<u8>();
    let tx = Mutex::new(tx);
    let f = Fake::start(
        Transport::Tcp,
        Protocol::V5,
        Options::default(),
        move |_, mut conn| {
            // CONNACK with Maximum Packet Size (0x27) = 4096.
            let ack = [0x20, 8, 0, 0, 5, 0x27, 0, 0, 0x10, 0];
            if conn.accept_connect_with(&ack).is_err() {
                return;
            }
            while let Ok((head, body)) = conn.read_packet() {
                let _ = tx.lock().expect("lock").send(head);
                if head & 0xf0 == 0x30 {
                    // PUBACK for the packet ID after the topic "t/m".
                    let _ = conn.send(&[0x40, 2, body[5], body[6]]);
                }
            }
        },
    );
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, Transport::Tcp, Protocol::V5, 0)).await;
    // The limit is published with the `Connected` state.
    assert_eq!(net.broker_max_packet(), Some(LIMIT as u32));
    let message = |op_id: u64, total: usize| {
        Command::Publish(Box::new(Outgoing {
            op_id,
            topic: "t/m".into(),
            // Fixed header 1, length 2, topic 2 + 3, packet ID 2, empty properties 1.
            payload: Bytes::from(vec![b'x'; total - (1 + 2 + 2 + 3 + 2 + 1)]),
            qos: Qos::AtLeastOnce,
            retain: false,
            props: MsgProps::default(),
        }))
    };
    net.send(message(1, LIMIT)).expect("send");
    net.send(message(2, LIMIT + 1)).expect("send");
    let mut results = Vec::new();
    while results.len() < 2 {
        match tokio::time::timeout(Duration::from_secs(3), net.events.recv()).await {
            Ok(Some(Event::Published { op_id, result, .. })) => results.push((op_id, result)),
            Ok(Some(_)) => {}
            other => panic!("expected a publish result, got {other:?}"),
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
    // The broker saw exactly one PUBLISH (QoS 1, first byte 0x32) and nothing else.
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok(0x32));
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
    net.disconnect();
    net.shutdown().await;
}

/// MQTT 5, the CONNACK has no Maximum Packet Size: the outgoing size rule is the MQTT maximum
/// (plan, row "Incoming size": "else the MQTT maximum"). The local incoming limit (`max_packet`,
/// 1 MiB) must not apply. A PUBLISH one byte over `max_packet`, and one of 2 x `max_packet`, go
/// out whole and get a PUBACK. The fake measures the whole packets that it reads. A message over
/// the MQTT maximum (256 MiB) is refused before the send.
#[tokio::test(flavor = "current_thread")]
async fn a_v5_publish_over_the_local_limit_is_sent_when_the_broker_announces_none() {
    let limit = NetLimits::default().max_packet;
    let (tx, rx) = mpsc::channel::<usize>();
    let tx = Mutex::new(tx);
    let f = Fake::start(
        Transport::Tcp,
        Protocol::V5,
        Options::default(),
        move |_, mut conn| {
            // The CONNACK has no properties.
            if conn.accept_connect().is_err() {
                return;
            }
            while let Ok((head, body)) = conn.read_packet() {
                if head & 0xf0 == 0x30 {
                    // The whole packet: fixed header, length bytes, body.
                    let total = 1 + fake::varint(body.len()).len() + body.len();
                    let _ = tx.lock().expect("lock").send(total);
                    // PUBACK for the packet ID after the topic "t/m".
                    let _ = conn.send(&[0x40, 2, body[5], body[6]]);
                }
            }
        },
    );
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, Transport::Tcp, Protocol::V5, 0)).await;
    assert_eq!(net.broker_max_packet(), None);
    let message = |op_id: u64, total: usize| {
        Command::Publish(Box::new(Outgoing {
            op_id,
            topic: "t/m".into(),
            // Fixed header 1, length bytes, topic 2 + 3, packet ID 2, properties 1. Zero bytes:
            // the huge message costs no memory.
            payload: Bytes::from(vec![0u8; total - (1 + len_bytes(total) + 2 + 3 + 2 + 1)]),
            qos: Qos::AtLeastOnce,
            retain: false,
            props: MsgProps::default(),
        }))
    };
    for (op_id, total) in [(1u64, limit + 1), (2, 2 * limit)] {
        net.send(message(op_id, total)).expect("send");
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the broker got the PUBLISH");
        assert_eq!(got, total);
        assert_eq!(
            next_published(&mut net).await,
            (op_id, PublishResult::Acknowledged)
        );
    }
    // The MQTT maximum is 268 435 455 + 5 bytes. One byte more is refused at once.
    let huge = 268_435_460 + 1;
    net.send(message(3, huge)).expect("send");
    assert_eq!(
        next_published(&mut net).await,
        (
            3,
            PublishResult::Rejected("The message is larger than the MQTT limit (256 MiB).".into())
        )
    );
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "the broker got a third PUBLISH"
    );
    net.disconnect();
    net.shutdown().await;
}

/// Length bytes of a packet of `total` bytes (the Remaining Length is `total - 1 - n`).
fn len_bytes(total: usize) -> usize {
    (1..=4)
        .find(|n| fake::varint(total - 1 - n).len() == *n)
        .unwrap_or(4)
}

/// A disconnect while a write is blocked is prompt: `Disconnected` in at most 3 s, no failure, and
/// every blocked send reports `LostOnDisconnect` (none `TimedOut`: the disconnect comes before the
/// 10 s timers). The v5 TCP case is also in `net_blocked_write.rs`.
async fn disconnect_during_a_blocked_write(t: Transport, v: Protocol) {
    const SENDS: usize = 24;
    let f = Fake::start(t, v, Options::default(), |_, mut conn| {
        if conn.accept_connect().is_ok() {
            conn.stop_reading(Duration::from_secs(20));
        }
    });
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, t, v, 0)).await;
    for op_id in 0..SENDS as u64 {
        net.send(big_publish(op_id, Qos::AtMostOnce)).expect("send");
    }
    let mut results = Vec::new();
    drain_until_quiet(&mut net, Instant::now(), &mut results).await;
    let mut results: Vec<PublishResult> = results.into_iter().map(|(_, r)| r).collect();
    // The broker reads nothing: fewer QoS 0 results than sends means a send waits for the write.
    assert!(results.len() < SENDS, "no send is blocked: {results:?}");
    assert!(
        results.iter().all(|r| *r == PublishResult::Sent),
        "{results:?}"
    );
    let started = Instant::now();
    net.disconnect();
    let mut failed = false;
    while net.status.borrow().state != ConnState::Disconnected {
        failed |= matches!(net.status.borrow().state, ConnState::Failed(_));
        tokio::time::timeout(Duration::from_secs(3), net.status.changed())
            .await
            .expect("Disconnected comes in 3 s")
            .expect("status");
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert!(!failed, "the attempt failed before it was disconnected");
    while results.len() < SENDS {
        match tokio::time::timeout(Duration::from_secs(2), net.events.recv()).await {
            Ok(Some(Event::Published { result, .. })) => results.push(result),
            Ok(Some(_)) => {}
            other => panic!("expected a publish result, got {other:?}"),
        }
    }
    assert!(
        results
            .iter()
            .all(|r| matches!(r, PublishResult::Sent | PublishResult::LostOnDisconnect)),
        "{results:?}"
    );
    assert!(
        results.contains(&PublishResult::LostOnDisconnect),
        "no blocked send reported LostOnDisconnect: {results:?}"
    );
    net.shutdown().await;
}
all_wires!(disconnect_during_a_blocked_write);

/// MQTT 5, Receive Maximum 2 in the CONNACK: 1000 QoS 1 publishes go out with at most 2 unanswered
/// at any time, and the results come in the order of the sends (plan, row "Bounded outgoing").
/// The fake counts the PUBLISH packets that it has read and not yet answered. The RSS part of the
/// plan row is not measured here.
#[tokio::test(flavor = "current_thread")]
async fn a_thousand_publishes_at_receive_maximum_2_keep_to_two_in_flight() {
    const COUNT: u64 = 1000;
    let max_in_flight = Arc::new(Mutex::new(0usize));
    let seen = max_in_flight.clone();
    let f = Fake::start(
        Transport::Tcp,
        Protocol::V5,
        Options::default(),
        move |_, mut conn| {
            // CONNACK with Receive Maximum (0x21) = 2.
            if conn
                .accept_connect_with(&[0x20, 6, 0, 0, 3, 0x21, 0, 2])
                .is_err()
            {
                return;
            }
            // Hold the PUBLISH packets until the client is idle: a client that keeps to Receive
            // Maximum 2 sends two and waits for the PUBACK.
            let mut open: Vec<[u8; 2]> = Vec::new();
            loop {
                match conn.read_packet_idle(Duration::from_millis(5)) {
                    Ok(Some((head, body))) if head & 0xf0 == 0x30 => {
                        // The packet ID follows the topic "t/r".
                        open.push([body[5], body[6]]);
                        let mut max = seen.lock().expect("lock");
                        *max = (*max).max(open.len());
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        for id in open.drain(..) {
                            if conn.send(&[0x40, 2, id[0], id[1]]).is_err() {
                                return;
                            }
                        }
                    }
                    Err(_) => return,
                }
            }
        },
    );
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, Transport::Tcp, Protocol::V5, 0)).await;
    let mut sent = 0u64;
    let mut results = Vec::new();
    while (results.len() as u64) < COUNT {
        // At most 100 results are open, below the 256 operations that `Net` allows.
        while sent < COUNT && sent - (results.len() as u64) < 100 {
            let mut o = small_publish(sent);
            if let Command::Publish(p) = &mut o {
                p.topic = "t/r".into();
                p.qos = Qos::AtLeastOnce;
            }
            match net.send(o) {
                Ok(()) => sent += 1,
                Err(_) => break,
            }
        }
        match tokio::time::timeout(Duration::from_secs(10), net.events.recv()).await {
            Ok(Some(Event::Published { op_id, result, .. })) => results.push((op_id, result)),
            Ok(Some(_)) => {}
            other => panic!("expected a publish result, got {other:?}"),
        }
    }
    let want: Vec<_> = (0..COUNT)
        .map(|i| (i, PublishResult::Acknowledged))
        .collect();
    assert_eq!(results, want);
    assert!(
        *max_in_flight.lock().expect("lock") <= 2,
        "more than 2 PUBLISH in flight"
    );
    net.disconnect();
    net.shutdown().await;
}

/// Plan, "Incoming topic aliases": an invalid alias must not be delivered. `Net` announces Topic
/// Alias Maximum 16. The broker first sends a valid PUBLISH (topic "a/ok", alias 1) as a control,
/// then one PUBLISH with `alias` and `topic`. Returns the status after the invalid packet and the
/// topics that reached `Net::incoming`.
async fn invalid_topic_alias(topic: &'static str, alias: u16) -> (Status, Vec<String>) {
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    let f = Fake::start(
        Transport::Tcp,
        Protocol::V5,
        Options::default(),
        move |_, mut conn| {
            if conn.accept_connect().is_err() {
                return;
            }
            let publish = |topic: &str, alias: u16| {
                let mut b = (topic.len() as u16).to_be_bytes().to_vec();
                b.extend(topic.as_bytes());
                b.extend([3, 0x23]);
                b.extend(alias.to_be_bytes());
                b.extend(b"payload");
                fake::pkt(0x30, &b)
            };
            if conn.send(&publish("a/ok", 1)).is_err() {
                return;
            }
            if go_rx
                .lock()
                .expect("lock")
                .recv_timeout(Duration::from_secs(5))
                .is_err()
            {
                return;
            }
            if conn.send(&publish(topic, alias)).is_ok() {
                conn.hold(Duration::from_secs(3));
            }
        },
    );
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, Transport::Tcp, Protocol::V5, 0)).await;
    let mut topics = Vec::new();
    let first = tokio::time::timeout(Duration::from_secs(5), net.incoming.recv())
        .await
        .expect("the control message did not come")
        .expect("incoming");
    topics.push(first.topic.to_string());
    go_tx.send(()).expect("broker thread");
    let ended = status_where(&mut net, Duration::from_secs(3), |s| {
        s.state != ConnState::Connected
    })
    .await;
    while let Ok(m) = net.incoming.try_recv() {
        topics.push(m.topic.to_string());
    }
    net.disconnect();
    net.shutdown().await;
    (ended, topics)
}

#[tokio::test(flavor = "current_thread")]
async fn a_v5_publish_with_an_invalid_topic_alias_is_not_delivered() {
    for (name, topic, alias) in [
        ("alias 0", "a/zero", 0u16),
        ("alias above our maximum", "a/high", 17),
        ("unknown alias, empty topic", "", 2),
    ] {
        let (ended, topics) = invalid_topic_alias(topic, alias).await;
        assert_eq!(
            ended.state,
            ConnState::Failed("The broker sent data that is not valid MQTT 5.".into()),
            "{name}"
        );
        assert_eq!(topics, ["a/ok"], "{name}: a message was delivered");
    }
}
