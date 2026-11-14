//! A disconnect while a write is blocked. It runs with the production `OP_TIMEOUT` (10 s), so an
//! operation timeout cannot end the attempt before the disconnect does. No broker is needed: the
//! fake broker takes CONNECT and then stops reading.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use bytes::Bytes;
use navigator_core::model::{MsgProps, Qos};
use navigator_core::net::{
    Command, ConnState, ConnectConfig, Event, Net, NetLimits, Outgoing, Profile, PublishResult,
    Transport,
};

const SENDS: u64 = 24;

#[tokio::test(flavor = "current_thread")]
async fn disconnect_during_a_blocked_write_is_prompt() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let broker = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf); // CONNECT
        let _ = sock.write_all(&[0x20, 0x03, 0x00, 0x00, 0x00]); // CONNACK success
        // Stop reading. Hold the socket until the test is over.
        let _ = done_rx.recv_timeout(Duration::from_secs(30));
    });

    let mut net = Net::start(NetLimits::default());
    let profile = Profile {
        host: "127.0.0.1".into(),
        port,
        transport: Transport::Tcp,
        subscriptions: vec![],
        ..Profile::default()
    };
    net.connect(ConnectConfig::from(profile)).expect("connect");
    tokio::time::timeout(
        Duration::from_secs(5),
        net.status.wait_for(|s| s.state == ConnState::Connected),
    )
    .await
    .expect("connects")
    .expect("status");
    for op_id in 0..SENDS {
        net.send(Command::Publish(Box::new(Outgoing {
            op_id,
            topic: "t/big".into(),
            payload: Bytes::from(vec![7u8; 900_000]),
            qos: Qos::AtMostOnce,
            retain: false,
            props: MsgProps::default(),
        })))
        .expect("send");
    }
    // About 20 MB are far more than the socket buffers hold, so the writes are blocked.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut results = Vec::new();
    while let Ok(event) = net.events.try_recv() {
        if let Event::Published { result, .. } = event {
            results.push(result);
        }
    }
    assert!(
        (results.len() as u64) < SENDS,
        "no send is blocked: {} of {SENDS} are done",
        results.len()
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
    // Every blocked send reports "lost". None times out: the disconnect came first.
    while results.len() < SENDS as usize {
        match tokio::time::timeout(Duration::from_secs(2), net.events.recv()).await {
            Ok(Some(Event::Published { result, .. })) => results.push(result),
            Ok(Some(_)) => {}
            other => panic!("expected a publish result, got {other:?}"),
        }
    }
    assert!(
        results.iter().all(|r| *r != PublishResult::TimedOut),
        "an operation timed out: {results:?}"
    );
    done_tx.send(()).expect("broker thread");
    net.shutdown().await;
    broker.join().expect("broker");
}
