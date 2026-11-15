//! Tests of `Net` against a fake broker (no real broker needed): the "Robust connection" rules of
//! the ntex plan. Each transport (TCP, TLS, WS, WSS) and each MQTT version (3.1.1, 5) is used at
//! least once. A case runs for another transport or version only where the code differs; the
//! comment of the case names that code. The fake broker sends and withholds bytes on purpose.
//!
//! Timers: every time limit of `Net` has one case with the real (production) value; the comment
//! of that case says so. The other cases use `short()` and scale the bounds of the plan from the
//! timer value.

#[path = "common/fake.rs"]
mod fake;

use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use bytes::Bytes;
use fake::{Fake, Options, ws_frame};
use navigator_core::model::{MsgProps, Qos};
use navigator_core::net::{
    Command, ConnState, ConnectConfig, Event, Net, NetLimits, Outgoing, Profile, Protocol,
    PublishResult, Status, Timers, Transport,
};

/// Short timers for the cases that need not wait for the real values. ntex counts the write,
/// TLS handshake and frame read-rate limits in whole seconds, so none is below 1 s.
fn short() -> Timers {
    Timers {
        connect: Duration::from_secs(2),
        op: Duration::from_secs(2),
        write: Duration::from_secs(2),
        tls_handshake: Duration::from_secs(1),
        ws_upgrade: Duration::from_secs(1),
        frame_check: Duration::from_secs(1),
        frame_cap: Duration::from_secs(3),
    }
}

fn start(timers: Timers) -> Net {
    Net::start(NetLimits {
        timers,
        ..NetLimits::default()
    })
}

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

/// Makes the tests of one case, each for one transport and one version. The cases are the
/// diagonal: every transport and every version is used, and the code that differs between the
/// two versions or the four transports is reached at least once.
macro_rules! wires {
    ($case:ident: $($name:ident $t:ident $v:ident),+ $(,)?) => {
        mod $case {
            use super::*;
            $(
                #[tokio::test(flavor = "current_thread")]
                async fn $name() {
                    super::$case(Transport::$t, Protocol::$v).await
                }
            )+
        }
    };
}

const OP_TIMEOUT_END_TEXT: &str = "The broker did not answer for 10 s. Connect again.";
const WRITE_TIMEOUT_TEXT: &str = "The broker did not read our data for 10 s.";
const CONNECTION_CLOSED_TEXT: &str =
    "The connection closed (the broker did not answer, or the network failed).";

/// The broker takes CONNECT and then stops reading. Our sends fill the buffers and block. Plan,
/// "A peer that stops reading cannot block us": a blocked publish ends with `TimedOut` in at most
/// operation timeout + 1 s, and the attempt ends at most write timeout + 2.5 s after the send
/// began (with keep alive 2 s: + 2 x keep alive x 1.125, as the PINGREQ sits behind the blocked
/// data; plan, row "A dead peer is found"). The operation timer (from the submit, final) and the
/// write timeout (temporary) race when they are equal. Either one is right, so the test accepts
/// exactly these two ends. When one timer is shorter, only it can end the attempt, and the test
/// expects its end. The bounds scale with the shorter timer.
async fn stalled_write(t: Transport, v: Protocol, keep_alive: u16, timers: Timers) {
    let (write, op) = (timers.write.as_secs_f64(), timers.op.as_secs_f64());
    let first = write.min(op);
    let ka_part = if keep_alive == 0 {
        0.0
    } else {
        2.0 * f64::from(keep_alive) * 1.125
    };
    let bound = first + 2.5 + ka_part;
    let f = Fake::start(t, v, Options::default(), |_, mut conn| {
        if conn.accept_connect().is_ok() {
            conn.stop_reading(Duration::from_secs(30));
        }
    });
    let mut net = start(timers);
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
        took >= Duration::from_secs_f64(first - 1.0) && took <= Duration::from_secs_f64(bound),
        "the attempt ended after {took:?}, not between {} s and {bound} s",
        first - 1.0
    );
    let by_write_timeout = match &ended.state {
        ConnState::Failed(text) if text == OP_TIMEOUT_END_TEXT => false,
        ConnState::Reconnecting { attempt: 1, reason } if reason == WRITE_TIMEOUT_TEXT => true,
        other => panic!("unexpected end of the attempt: {other:?}"),
    };
    if write < op {
        assert!(
            by_write_timeout,
            "the write timeout is the shorter one: {ended:?}"
        );
    } else if op < write {
        assert!(
            !by_write_timeout,
            "the operation timeout is the shorter one: {ended:?}"
        );
    }
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
        // A blocked publish reports `TimedOut` in at most operation timeout + 1 s. More than
        // one can be blocked.
        assert!(!timed_out.is_empty(), "{results:?}");
        assert!(
            timed_out
                .iter()
                .all(|(at, _)| *at <= Duration::from_secs_f64(op + 1.0)),
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

/// REAL VALUE of the operation timeout (10 s). The write timeout is 60 s, so only the operation
/// timer can end the attempt: the final end text, `TimedOut` in at most 11 s, the end in at most
/// 12.5 s.
#[tokio::test(flavor = "current_thread")]
async fn v5_tcp_stalled_write_with_the_real_operation_timeout() {
    let timers = Timers {
        write: Duration::from_secs(60),
        ..Timers::default()
    };
    stalled_write(Transport::Tcp, Protocol::V5, 0, timers).await;
}

// The short cases (write timeout = operation timeout = 2 s: the two race): TLS, WS and WSS carry
// the data through other layers (the TLS record buffer, the WebSocket frame sink), so the blocked
// write must end there too. v3 and v5 differ in the codec (`wire.rs`, v3 and v5 sinks).
#[tokio::test(flavor = "current_thread")]
async fn v3_tls_stalled_write() {
    stalled_write(Transport::Tls, Protocol::V311, 0, short()).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v5_ws_stalled_write() {
    stalled_write(Transport::Ws, Protocol::V5, 0, short()).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v3_wss_stalled_write() {
    stalled_write(Transport::Wss, Protocol::V311, 0, short()).await;
}

/// With keep alive 2 s the PINGREQ sits behind the blocked data (the keep-alive timer and the
/// write timeout meet; the same ends are right).
#[tokio::test(flavor = "current_thread")]
async fn v5_stalled_write_with_keep_alive_2_s() {
    stalled_write(Transport::Tcp, Protocol::V5, 2, short()).await;
}

/// REAL VALUE of the write timeout (10 s). The write timeout alone: the operation timeout is 60 s,
/// so only the write timeout can end the attempt. ntex starts its write timeout when the write
/// buffer goes above the high watermark and does not drain. The fake broker reads nothing after
/// CONNECT. The test sends a burst of QoS 0 messages; some get no result, so the buffer is full
/// and the stall began during the burst. The write timeout ends the attempt at 10 s to 12 s after
/// the stall (the timer can be up to 1 s late, then up to
/// 1 s of I/O shutdown). The end is temporary (`Reconnecting`) with the write timeout text, and
/// a blocked send is `LostOnDisconnect`, not `TimedOut`. Without the write timeout, nothing
/// ends the attempt in that time and the test fails (mutation check in the S3 report).
#[tokio::test(flavor = "current_thread")]
async fn v5_write_timeout_alone_is_a_temporary_failure() {
    let timers = Timers {
        op: Duration::from_secs(60),
        ..Timers::default()
    };
    let write = timers.write.as_secs_f64();
    let f = Fake::start(
        Transport::Tcp,
        Protocol::V5,
        Options::default(),
        |_, mut conn| {
            if conn.accept_connect().is_ok() {
                conn.stop_reading(Duration::from_secs(60));
            }
        },
    );
    let mut net = start(timers);
    connect(&mut net, config(&f, Transport::Tcp, Protocol::V5, 0)).await;
    // Like `stalled_write`: send a burst that is far more than the buffers hold, and take the
    // results until none comes for 500 ms. A slow runner cannot make a send look blocked: fewer
    // results than sends means that at least one send waits for the blocked write. The stall
    // began during the burst, so the bound is measured from before it.
    const SENDS: usize = 24;
    let started = Instant::now();
    for op_id in 0..SENDS as u64 {
        net.send(big_publish(op_id, Qos::AtMostOnce)).expect("send");
    }
    let mut results = Vec::new();
    drain_until_quiet(&mut net, started, &mut results).await;
    assert!(results.len() < SENDS, "no send is blocked: {results:?}");
    assert!(
        results.iter().all(|(_, r)| *r == PublishResult::Sent),
        "{results:?}"
    );
    // Keep taking the results while we wait for the end: a late `Sent` can start the last write
    // period, so the upper bound must count from the last `Sent` before the end.
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(write + 4.0);
    let ended = loop {
        let now = net.status.borrow_and_update().clone();
        if now.state != ConnState::Connected {
            break now;
        }
        tokio::select! {
            changed = net.status.changed() => changed.expect("status"),
            event = net.events.recv() => match event {
                Some(Event::Published { result, .. }) => results.push((started.elapsed(), result)),
                Some(_) => {}
                None => panic!("events closed"),
            },
            () = tokio::time::sleep_until(deadline) => panic!("the attempt did not end in time"),
        }
    };
    // The stall starts between the burst and the last `Sent`: the buffers took that message, so
    // the write timer cannot start before the burst or long after it. The OS sets how long the
    // buffers take bytes (Windows CI: the attempt ended 12.9 s after the burst), so the upper bound
    // counts from the last `Sent`.
    let took = started.elapsed().as_secs_f64();
    let last_sent = results
        .iter()
        .rfind(|(_, r)| *r == PublishResult::Sent)
        .map_or(0.0, |(at, _)| at.as_secs_f64());
    assert!(
        took >= write - 1.0 && took - last_sent <= write + 2.5,
        "the attempt ended {took} s after the burst, {} s after the last Sent ({ended:?})",
        took - last_sent
    );
    assert!(
        matches!(&ended.state, ConnState::Reconnecting { attempt: 1, reason } if reason == WRITE_TIMEOUT_TEXT),
        "{ended:?}"
    );
    let mut results: Vec<PublishResult> = results.into_iter().map(|(_, r)| r).collect();
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
    net.disconnect();
    net.shutdown().await;
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

// The close of WS and WSS waits up to 1 s for the peer (`wire.rs`, WebSocket sink), so each
// transport needs its own run; the versions differ in the keep-alive path of the two codecs.
wires!(missing_pingresp: v5_tcp Tcp V5, v3_tls Tls V311, v5_ws Ws V5, v3_wss Wss V311);

/// REAL VALUES of the keep alive: 60 s (v5) and 70 s (v3; 70 s is in the 8 s timer step range and
/// checks the rounding). The plan bound 2 x keep alive x 1.125 + 1 s is 136 s and 158.5 s. These
/// run more than 2 minutes, so they are `#[ignore]`: run them with `--ignored`.
macro_rules! long_keep_alive {
    ($name:ident, $t:expr, $v:expr, $ka:expr) => {
        #[tokio::test(flavor = "current_thread")]
        #[ignore = "runs 2 to 2.5 minutes"]
        async fn $name() {
            missing_pingresp_with($t, $v, $ka).await;
        }
    };
}

/// Publishes do not hide a dead peer (the keep-alive timer sees only what the broker sends).
#[tokio::test(flavor = "current_thread")]
async fn v5_missing_pingresp_with_publishes() {
    missing_pingresp_full(Transport::Tcp, Protocol::V5, 2, true).await;
}

long_keep_alive!(v5_keep_alive_60_s, Transport::Tcp, Protocol::V5, 60);
long_keep_alive!(v3_keep_alive_70_s, Transport::Tcp, Protocol::V311, 70);

const READ_TIMEOUT_TEXT: &str = "The broker stopped in the middle of a packet.";

/// TCP and TLS, keep alive 0: the broker sends a started packet and stops. A half packet ends at
/// the first rate check of the frame read rate, plus timer step, shutdown and rounding: in at
/// most check time + 3 s (plan, "A started packet that stops cannot block us"). `first` is what
/// the broker sends: half a fixed header, or a PUBLISH header and 10 bytes of its payload.
async fn half_frame(t: Transport, v: Protocol, first: Vec<u8>, timers: Timers) {
    let check = timers.frame_check.as_secs_f64();
    let (lo, hi) = ((check - 1.0).max(check / 2.0), check + 3.0);
    let f = Fake::start(t, v, Options::default(), move |_, mut conn| {
        if conn.accept_connect().is_ok() && conn.send_raw(&first).is_ok() {
            conn.hold(Duration::from_secs(20));
        }
    });
    let mut net = start(timers);
    connect(&mut net, config(&f, t, v, 0)).await;
    let started = Instant::now();
    let lost = status_where(&mut net, Duration::from_secs_f64(hi + 1.0), |s| {
        s.state != ConnState::Connected
    })
    .await;
    let took = started.elapsed().as_secs_f64();
    assert!((lo..=hi).contains(&took), "ended after {took} s");
    assert!(
        matches!(&lost.state, ConnState::Reconnecting { attempt: 1, reason } if reason == READ_TIMEOUT_TEXT),
        "{lost:?}"
    );
    net.disconnect();
    net.shutdown().await;
}

/// Half a fixed header: the first byte of a PUBLISH, and a length byte with the continuation bit.
/// The decoder keeps the bytes, so the frame read rate (the same one for each version and for
/// TCP and TLS; TLS only adds the record layer) must end the wait.
async fn half_fixed_header(t: Transport, v: Protocol) {
    half_frame(t, v, vec![0x30, 0x80], short()).await;
}
wires!(half_fixed_header: v5_tcp Tcp V5, v3_tls Tls V311);

/// A PUBLISH of 1000 bytes: the header and 10 bytes (topic "t/a" and 5 payload bytes). MQTT 5
/// adds an empty property length (0x00) before the payload.
fn half_payload_bytes(v: Protocol) -> Vec<u8> {
    let mut first = vec![0x30, 0xe8, 0x07, 0x00, 0x03, b't', b'/', b'a'];
    if v == Protocol::V5 {
        first.push(0x00);
    }
    first.extend([b'x'; 5]);
    first
}

/// REAL VALUE of the frame read-rate check (10 s): the half payload ends in at most 13 s.
#[tokio::test(flavor = "current_thread")]
async fn v5_tcp_half_payload_with_the_real_frame_check() {
    half_frame(
        Transport::Tcp,
        Protocol::V5,
        half_payload_bytes(Protocol::V5),
        Timers::default(),
    )
    .await;
}

/// The v3 payload has no property length: its own decoder path (`ntex-mqtt` v3 codec).
#[tokio::test(flavor = "current_thread")]
async fn v3_tls_half_payload() {
    half_frame(
        Transport::Tls,
        Protocol::V311,
        half_payload_bytes(Protocol::V311),
        short(),
    )
    .await;
}

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
wires!(half_binary_frame: v5_ws Ws V5, v3_wss Wss V311);

/// A payload that arrives at more than 256 bytes in each check and never completes ends at the
/// cap of the frame read rate (plan, same table row: at most (cap / check) x (check + 1) + 2 s).
/// The broker sends 512 bytes every half check time.
async fn trickle(t: Transport, v: Protocol, timers: Timers) {
    let (check, cap) = (
        timers.frame_check.as_secs_f64(),
        timers.frame_cap.as_secs_f64(),
    );
    let (lo, hi) = (cap - 1.0, (cap / check) * (check + 1.0) + 2.0);
    let step = Duration::from_secs_f64(check / 2.0);
    let rounds = (hi / (check / 2.0)).ceil() as usize + 2;
    let f = Fake::start(t, v, Options::default(), move |_, mut conn| {
        if conn.accept_connect().is_err() {
            return;
        }
        // A PUBLISH of 100 000 bytes: topic "t/a", for MQTT 5 an empty property list, then
        // payload bytes that arrive 512 at a time.
        let mut head = vec![0x30, 0xa0, 0x8d, 0x06, 0x00, 0x03, b't', b'/', b'a'];
        if v == Protocol::V5 {
            head.push(0x00);
        }
        if conn.send_raw(&head).is_err() {
            return;
        }
        for _ in 0..rounds {
            std::thread::sleep(step);
            if conn.send_raw(&[b'x'; 512]).is_err() {
                return;
            }
        }
        conn.hold(Duration::from_secs(5));
    });
    let mut net = start(timers);
    connect(&mut net, config(&f, t, v, 0)).await;
    let started = Instant::now();
    let lost = status_where(&mut net, Duration::from_secs_f64(hi + 2.0), |s| {
        s.state != ConnState::Connected
    })
    .await;
    let took = started.elapsed().as_secs_f64();
    assert!((lo..=hi).contains(&took), "ended after {took} s");
    assert!(
        matches!(&lost.state, ConnState::Reconnecting { attempt: 1, reason } if reason == READ_TIMEOUT_TEXT),
        "{lost:?}"
    );
    net.disconnect();
    net.shutdown().await;
}

/// REAL VALUE of the frame read-rate cap (60 s, check 10 s): ends at the cap, in at most 68 s. It
/// runs 60 s, so it is `#[ignore]`: run it with `--ignored`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "runs 60 s: the cap of the frame read rate"]
async fn v5_tcp_trickle_with_the_real_cap() {
    trickle(Transport::Tcp, Protocol::V5, Timers::default()).await;
}

#[tokio::test(flavor = "current_thread")]
async fn v3_tls_trickle() {
    trickle(Transport::Tls, Protocol::V311, short()).await;
}

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
wires!(ack_wait_cancelled_by_close: v5_tcp Tcp V5, v3_tls Tls V311, v5_ws Ws V5, v3_wss Wss V311);

/// REAL VALUE of the WebSocket upgrade timeout (5 s). A server that finishes the TLS handshake and
/// never answers the WebSocket upgrade: ntex ends the upgrade after 5 s, a temporary timeout (plan,
/// row "One deadline"; S1 finding 9). The upgrade timer is the same for WS and WSS and for both
/// versions: it runs before any MQTT byte.
#[tokio::test(flavor = "current_thread")]
async fn v5_wss_stalled_upgrade_is_a_temporary_timeout_with_the_real_timer() {
    let f = Fake::start(
        Transport::Wss,
        Protocol::V5,
        Options::default(),
        |_, mut conn| {
            conn.hold(Duration::from_secs(12));
        },
    );
    let mut net = Net::start(NetLimits::default());
    net.send(Command::Connect(Box::new(config(
        &f,
        Transport::Wss,
        Protocol::V5,
        0,
    ))))
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
    // Between the upgrade timer (5 s) and the connect deadline (10 s), with rounding.
    assert!(
        (Duration::from_secs(4)..=Duration::from_secs(7)).contains(&started.elapsed()),
        "{:?}",
        started.elapsed()
    );
    net.disconnect();
    net.shutdown().await;
}

const CLIENT_CERT_TEXT: &str =
    "The broker needs a client certificate. Add your certificate and key.";

/// A deliberate twin of `broker_matrix::mtls_over_secure_websocket` (real Mosquitto, port 18838):
/// the broker tests run only on Linux until step 9, and this case found the Windows TLS reset
/// bug, so it stays as the Windows and macOS smoke test for client certificates over WSS with
/// vendored OpenSSL. With the client certificate the connect works; without it the attempt fails
/// with the client-certificate text.
#[tokio::test(flavor = "current_thread")]
async fn v5_mtls_over_wss() {
    let (t, v) = (Transport::Wss, Protocol::V5);
    let f = Fake::start(
        t,
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
    let mut with = config(&f, t, v, 0);
    with.profile.client_cert = Some(f.certs.client_cert.clone());
    with.profile.client_key = Some(f.certs.client_key.clone());
    connect(&mut net, with).await;
    net.disconnect();
    status_where(&mut net, Duration::from_secs(3), |s| {
        s.state == ConnState::Disconnected
    })
    .await;

    net.send(Command::Connect(Box::new(config(&f, t, v, 0))))
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

// TLS and WSS once each: the reset reaches the TLS set-up of both (`wire.rs`, `SETUP_CLOSE_TEXT`).
wires!(missing_cert_with_reset: v5_tls Tls V5, v3_wss Wss V311);

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
wires!(disconnect_while_a_half_frame_waits: v5_tcp Tcp V5, v3_tls Tls V311, v5_ws Ws V5, v3_wss Wss V311);

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
wires!(big_header: v5_tcp Tcp V5, v3_tls Tls V311);

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
wires!(websocket_frames: v5_ws Ws V5, v3_wss Wss V311);

/// Silent CONNACK: the broker reads CONNECT and never answers. Our connect deadline ends it in at
/// most deadline + 1 s (plan, row "One deadline"), with the text for a late CONNACK, as a
/// temporary error.
async fn silent_connack(t: Transport, v: Protocol, timers: Timers) {
    let deadline = timers.connect.as_secs_f64();
    let f = Fake::start(t, v, Options::default(), move |_, mut conn| {
        let _ = conn.read_packet();
        conn.hold(Duration::from_secs_f64(deadline + 4.0));
    });
    let mut net = start(timers);
    let started = Instant::now();
    net.send(Command::Connect(Box::new(config(&f, t, v, 0))))
        .expect("send");
    let s = status_where(&mut net, Duration::from_secs_f64(deadline + 2.0), |s| {
        matches!(
            s.state,
            ConnState::Reconnecting { .. } | ConnState::Failed(_)
        )
    })
    .await;
    let took = started.elapsed().as_secs_f64();
    assert!(
        (deadline - 1.0..=deadline + 1.0).contains(&took),
        "{took} s"
    );
    assert!(
        matches!(&s.state, ConnState::Reconnecting { attempt: 1, reason } if reason == "The broker did not answer the connection request."),
        "{s:?}"
    );
    net.disconnect();
    net.shutdown().await;
}

/// Short connect deadline (the real value is `a_black_hole_dial_ends_with_the_real_connect_deadline`).
#[tokio::test(flavor = "current_thread")]
async fn v5_tcp_silent_connack() {
    silent_connack(Transport::Tcp, Protocol::V5, short()).await;
}

/// TLS, v3: the deadline also covers the MQTT 3.1.1 handshake (`wire.rs`, v3 connect).
#[tokio::test(flavor = "current_thread")]
async fn v3_tls_silent_connack() {
    silent_connack(Transport::Tls, Protocol::V311, short()).await;
}

/// A server that accepts the TCP connection and never answers the TLS handshake. The TLS
/// handshake timeout ends it before our deadline: a temporary timeout (S1 finding 9). It ends in
/// at most handshake timeout + 2 s.
async fn stalled_tls(t: Transport, v: Protocol, timers: Timers) {
    let tls = timers.tls_handshake.as_secs_f64();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let holder = std::thread::spawn(move || {
        let (_sock, _) = listener.accept().expect("accept");
        std::thread::sleep(Duration::from_secs_f64(tls + 3.0));
    });
    let f = Fake::start(t, v, Options::default(), |_, _| {});
    let mut c = config(&f, t, v, 0);
    c.profile.port = port;
    let mut net = start(timers);
    let started = Instant::now();
    net.send(Command::Connect(Box::new(c))).expect("send");
    let s = status_where(&mut net, Duration::from_secs_f64(tls + 4.0), |s| {
        matches!(
            s.state,
            ConnState::Reconnecting { .. } | ConnState::Failed(_)
        )
    })
    .await;
    let took = started.elapsed().as_secs_f64();
    assert!(
        ((tls - 1.0).max(tls / 2.0)..=tls + 2.0).contains(&took),
        "{took} s"
    );
    assert!(
        matches!(&s.state, ConnState::Reconnecting { attempt: 1, reason } if reason == "The connection timed out."),
        "{s:?}"
    );
    net.disconnect();
    net.shutdown().await;
    holder.join().expect("holder");
}

/// REAL VALUE of the TLS handshake timeout (5 s).
#[tokio::test(flavor = "current_thread")]
async fn v5_tls_stalled_handshake_with_the_real_timer() {
    stalled_tls(Transport::Tls, Protocol::V5, Timers::default()).await;
}

/// WSS: the TLS handshake runs below the WebSocket upgrade, so the same timer must end it. The
/// connect deadline is 5 s, above the bound of the handshake timer (3 s), so only the handshake
/// timer can end the attempt in time.
#[tokio::test(flavor = "current_thread")]
async fn v3_wss_stalled_handshake() {
    let timers = Timers {
        connect: Duration::from_secs(5),
        ..short()
    };
    stalled_tls(Transport::Wss, Protocol::V311, timers).await;
}

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

    /// REAL VALUE of the outer connect deadline (10 s): a dial to a black hole ends with it, in
    /// at most 11 s. No other timer can end the dial, so the end shows the outer deadline.
    #[tokio::test(flavor = "current_thread")]
    async fn a_black_hole_dial_ends_with_the_real_connect_deadline() {
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

/// MQTT 5: the CONNACK has Server Keep Alive 0, so the client sends no PINGREQ, whatever the
/// profile says (plan, row "A dead peer is found": "Server Keep Alive 0 and 5 s: the client
/// follows it"). Mosquitto cannot send 0, so this stays a fake; the 5 s case is real
/// (`broker_matrix::v5_server_keep_alive_5_s_is_followed`). The profile asks 1 s, so a client
/// that ignored the broker would ping within 1.2 s.
#[tokio::test(flavor = "current_thread")]
async fn v5_server_keep_alive_0_sends_no_ping() {
    let pings = Arc::new(Mutex::new(0usize));
    let seen = pings.clone();
    let f = Fake::start(
        Transport::Tcp,
        Protocol::V5,
        Options::default(),
        move |_, mut conn| {
            // CONNACK: flags, reason 0, 3 bytes of properties: Server Keep Alive (0x13) = 0.
            if conn
                .accept_connect_with(&[0x20, 6, 0, 0, 3, 0x13, 0, 0])
                .is_ok()
            {
                conn.read_until_close(Duration::from_secs(6), |head, _| {
                    if head == 0xc0 {
                        *seen.lock().expect("lock") += 1;
                    }
                });
            }
        },
    );
    let mut net = Net::start(NetLimits::default());
    connect(&mut net, config(&f, Transport::Tcp, Protocol::V5, 1)).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(net.status.borrow().state, ConnState::Connected);
    assert_eq!(*pings.lock().expect("lock"), 0);
    net.disconnect();
    net.shutdown().await;
}

/// MQTT 5, the CONNACK has no Maximum Packet Size: the outgoing size rule is the MQTT maximum
/// (plan, row "Incoming size": "else the MQTT maximum"). The local incoming limit (`max_packet`,
/// 1 MiB) must not apply. A PUBLISH one byte over `max_packet`, and one of 2 x `max_packet`, go
/// out whole and get a PUBACK. The fake measures the whole packets that it reads. A message over
/// the MQTT maximum (256 MiB) is refused before the send. This stays a fake: Mosquitto 2.1 always
/// announces a Maximum Packet Size (2 000 000 bytes by default; the minimum for `max_packet_size`
/// is 20), so Mosquitto cannot answer without one.
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
/// timers).
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
wires!(disconnect_during_a_blocked_write: v5_tcp Tcp V5, v3_tls Tls V311, v5_ws Ws V5, v3_wss Wss V311);

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
