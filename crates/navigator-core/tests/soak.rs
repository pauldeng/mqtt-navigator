//! Soak test: `Net` and `TopicStore` (default limits) with a consumer that works like the UI
//! scheduler, 500 msg/s over 2 000 topics, and 6 broker restarts spread over the run. It uses the
//! anonymous Docker listener (18830) and restarts the `mosquitto` service of tests/broker.
//!
//! Run (1 h): `tests/broker/up.sh mosquitto && cargo test --release -p navigator-core --features
//! broker-tests --test soak -- --ignored --nocapture && tests/broker/down.sh`.
//! Env: `SOAK_SECS` (default 3600) and `SOAK_RATE` (default 500). The RSS baseline is taken 300 s
//! after the start, so a run of 600 s or more has a window to measure.
//!
//! Checks: after each restart the session is `Connected` again with the same generation and
//! messages flow; RSS grows by less than 50 MiB from the baseline to the end; the store stays
//! under its hard limit; the net and event drops are 0 outside the restart windows. Messages
//! that the publisher sent while the broker was down are lost (clean sessions). The test counts
//! them and fails only if there are more than the restart windows can explain.
#![cfg(feature = "broker-tests")]

use std::path::PathBuf;
use std::process::Command as Process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use ms_mqtt_client::client::{ClientOptions, ConnectResult, KeepAliveConfig, new_client};
use ms_mqtt_client::packet::{ConnectProperties, PublishProperties};
use ms_mqtt_client::topic::TopicName;
use ms_mqtt_client::transport::{ConnectionTransportConfig, ConnectionTransportType};
use navigator_core::model::Qos;
use navigator_core::net::{
    Command, ConnState, ConnectConfig, Event, Net, NetLimits, Profile, Transport,
};
use navigator_core::store::{Limits, TopicStore};

const PORT: u16 = 18830;
const TOPICS: usize = 2_000;
const SIZE: usize = 100;
const RESTARTS: u64 = 6;
const MERGE_SLICE: Duration = Duration::from_millis(3);
const FRAME_BUDGET: Duration = Duration::from_millis(4);
const FRAME: Duration = Duration::from_millis(33);
const BASELINE_AT: Duration = Duration::from_secs(300);
const RSS_LIMIT: i64 = 50 * 1024 * 1024;
/// A restart window ends when the session is back and this many messages came after it.
const FLOW_MESSAGES: u64 = 200;
/// The longest time that one restart may take until messages flow again.
const WINDOW_MAX: Duration = Duration::from_secs(90);
/// Time after the restart window in which a drop is still tolerated.
const GRACE: Duration = Duration::from_secs(5);

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Resident set size in bytes. `None` when the platform has no `/proc`.
fn rss() -> Option<i64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: i64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

/// Publishes QoS 0 messages at `rate` until `stop` is set. It connects again after each
/// restart. The first 8 payload bytes are the sequence number. `sent` counts the messages that
/// the client accepted.
fn publisher(rate: u64, stop: Arc<AtomicBool>, sent: Arc<AtomicU64>, connects: Arc<AtomicU64>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let topics: Vec<TopicName> = (0..TOPICS)
            .map(|i| TopicName::new(format!("soak/t{i}")).expect("topic"))
            .collect();
        let batch = rate.div_ceil(100);
        let mut seq = 0u64;
        while !stop.load(Ordering::Acquire) {
            let (client, connect_handle, mut receiver) = new_client(ClientOptions::default());
            let connection = match connect_handle
                .connect(
                    ConnectionTransportConfig {
                        transport_type: ConnectionTransportType::Tcp {
                            hostname: "127.0.0.1".into(),
                            port: PORT,
                        },
                        timeout: Some(Duration::from_secs(5)),
                        proxy: None,
                        tcp_nodelay: true,
                    },
                    true,
                    KeepAliveConfig::Infinite,
                    None,
                    None,
                    None,
                    ConnectProperties::default(),
                    Some(Duration::from_secs(5)),
                )
                .await
            {
                ConnectResult::Success(connection, _, _handle) => connection,
                ConnectResult::Failure(..) => {
                    // The broker is down. Try again.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };
            connects.fetch_add(1, Ordering::Relaxed);
            let sender = async {
                let mut tick = tokio::time::interval(Duration::from_millis(10));
                let mut last = None;
                while !stop.load(Ordering::Acquire) {
                    tick.tick().await;
                    for _ in 0..batch {
                        let mut payload = vec![b'x'; SIZE];
                        payload[..8].copy_from_slice(&seq.to_le_bytes());
                        let Ok(token) = client
                            .publish_qos0(
                                topics[seq as usize % TOPICS].clone(),
                                Bytes::from(payload),
                                false,
                                PublishProperties::default(),
                            )
                            .await
                        else {
                            return;
                        };
                        last = Some(token);
                        seq += 1;
                        sent.store(seq, Ordering::Relaxed);
                    }
                }
                // Let the client write the queued messages before it closes.
                if let Some(token) = last {
                    let _ = tokio::time::timeout(Duration::from_secs(2), token).await;
                }
            };
            // The connection makes progress only while it is polled.
            tokio::select! {
                _ = connection.run_until_disconnect() => {}
                _ = receiver.recv() => {}
                () = sender => {}
            }
        }
    });
}

/// One restart that is in progress.
struct Window {
    started: Instant,
    restart: tokio::task::JoinHandle<bool>,
    /// Drop counters at the start.
    drops_at_start: u64,
    saw_down: bool,
    /// Messages received when the session was back.
    received_at_up: Option<u64>,
}

fn restart_broker() -> tokio::task::JoinHandle<bool> {
    let compose = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/broker/compose.yml");
    tokio::task::spawn_blocking(move || {
        Process::new("docker")
            .args(["compose", "-f"])
            .arg(compose)
            .args(["restart", "-t", "1", "mosquitto"])
            .output()
            .expect("run docker")
            .status
            .success()
    })
}

#[test]
#[ignore = "runs for SOAK_SECS (1 h by default); use `-- --ignored`"]
fn soak_with_broker_restarts() {
    let secs = Duration::from_secs(env_u64("SOAK_SECS", 3600));
    let rate = env_u64("SOAK_RATE", 500);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let mut net = Net::start(NetLimits::default());
        let drained = net.events_drained();
        net.send(Command::Connect(Box::new(ConnectConfig::from(Profile {
            host: "127.0.0.1".into(),
            port: PORT,
            transport: Transport::Tcp,
            subscriptions: vec![("soak/#".into(), Qos::AtMostOnce)],
            ..Profile::default()
        }))))
        .expect("send");
        let generation = tokio::time::timeout(
            Duration::from_secs(10),
            net.status
                .wait_for(|s| s.state == ConnState::Connected && s.generation > 0),
        )
        .await
        .expect("connect timeout")
        .expect("status")
        .generation;

        let stop = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU64::new(0));
        let connects = Arc::new(AtomicU64::new(0));
        let start = Instant::now();
        let thread = {
            let (stop, sent, connects) = (stop.clone(), sent.clone(), connects.clone());
            std::thread::spawn(move || publisher(rate, stop, sent, connects))
        };

        let mut store = TopicStore::new(Limits::default());
        let hard_limit = store.hard_limit();
        let (mut received, mut duplicates, mut bad, mut wrong_generation) = (0u64, 0u64, 0u64, 0u64);
        let mut seen: Vec<bool> = Vec::new();
        let (mut subscribed, mut reconnected) = (0u64, 0u64);
        // Events outside the restart windows that must not happen.
        let mut bad_events: Vec<String> = Vec::new();
        let mut errors_in_windows = 0u64;
        let mut rss_base = None;
        let mut rss_end = None;
        let mut rss_max = 0i64;
        let mut max_used = 0usize;
        let mut slices = 0u64;
        let mut slice_max = Duration::ZERO;
        let mut window: Option<Window> = None;
        let mut launched = 0u64;
        // Drops that happened inside the windows (and their grace time).
        let mut window_drops = 0u64;
        let mut window_time = Duration::ZERO;
        let mut windows_done: Vec<Duration> = Vec::new();
        let mut grace_until: Option<Instant> = None;
        let mut next_progress = Duration::from_secs(60);
        let mut stopped_at: Option<Instant> = None;
        let drops_now = |net: &Net| {
            net.stats.drops.load(Ordering::Relaxed) + net.stats.event_drops.load(Ordering::Relaxed)
        };

        loop {
            let elapsed = start.elapsed();
            // Start the next restart: evenly spaced, or at the end when the run is short.
            let due = secs * (launched as u32 + 1) / (RESTARTS as u32 + 1);
            if window.is_none()
                && launched < RESTARTS
                && (elapsed >= due || elapsed >= secs)
            {
                launched += 1;
                eprintln!("soak: restart {launched} at {:.0}s", elapsed.as_secs_f64());
                window = Some(Window {
                    started: Instant::now(),
                    restart: restart_broker(),
                    drops_at_start: drops_now(&net),
                    saw_down: false,
                    received_at_up: None,
                });
            }
            if rss_base.is_none() && elapsed >= BASELINE_AT {
                rss_base = rss();
            }
            if let Some(r) = rss() {
                rss_max = rss_max.max(r);
            }
            if elapsed >= next_progress {
                next_progress += Duration::from_secs(60);
                eprintln!(
                    "soak: {:.0}s sent {} received {} used {} B rss {} MiB",
                    elapsed.as_secs_f64(),
                    sent.load(Ordering::Relaxed),
                    received,
                    store.stats().used_bytes,
                    rss().unwrap_or(0) / (1024 * 1024),
                );
            }

            // Events: the UI drains them in each frame.
            let in_window = window.is_some() || grace_until.is_some_and(|t| Instant::now() < t);
            while let Ok(event) = net.events.try_recv() {
                match event {
                    Event::Subscribed { result, .. } => match result {
                        Ok(_) => subscribed += 1,
                        Err(e) if in_window => {
                            errors_in_windows += 1;
                            eprintln!("soak: subscribe error in a window: {e}");
                        }
                        Err(e) => bad_events.push(format!("subscribe failed: {e}")),
                    },
                    Event::Reconnected { .. } => reconnected += 1,
                    Event::Error { message, .. } if in_window => {
                        errors_in_windows += 1;
                        eprintln!("soak: error in a window: {message}");
                    }
                    Event::Error { message, .. } => bad_events.push(message),
                    _ => {}
                }
            }
            drained.notify();

            // Restart progress.
            if let Some(w) = window.as_mut() {
                let s = net.status.borrow().clone();
                assert_eq!(s.generation, generation, "the generation changed");
                if matches!(s.state, ConnState::Reconnecting { .. }) {
                    w.saw_down = true;
                }
                if let ConnState::Failed(text) = &s.state {
                    panic!("the session failed after a restart: {text}");
                }
                if w.saw_down && w.received_at_up.is_none() && s.state == ConnState::Connected {
                    w.received_at_up = Some(received);
                }
                let flowing = w
                    .received_at_up
                    .is_some_and(|n| received >= n + FLOW_MESSAGES);
                if flowing && w.restart.is_finished() {
                    let took = w.started.elapsed();
                    window_drops += drops_now(&net) - w.drops_at_start;
                    window_time += took;
                    windows_done.push(took);
                    eprintln!("soak: messages flow again after {:.1}s", took.as_secs_f64());
                    grace_until = Some(Instant::now() + GRACE);
                    let w = window.take().expect("window");
                    assert!(w.restart.await.expect("join"), "docker restart failed");
                } else {
                    assert!(
                        w.started.elapsed() < WINDOW_MAX,
                        "no message flow {WINDOW_MAX:?} after a restart (saw_down {}, up {})",
                        w.saw_down,
                        w.received_at_up.is_some()
                    );
                }
            }

            // The end: the time is over and every restart is done.
            if elapsed >= secs && launched == RESTARTS && window.is_none() {
                if stopped_at.is_none() {
                    rss_end = rss();
                    stopped_at = Some(Instant::now());
                    stop.store(true, Ordering::Release);
                }
                // Let the consumer take the last messages.
                if stopped_at.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) {
                    break;
                }
            }

            // The UI scheduler: wait for a message, merge for at most 3 ms, work until 4 ms.
            let first = match tokio::time::timeout(Duration::from_millis(200), net.incoming.recv())
                .await
            {
                Ok(Some(m)) => m,
                Ok(None) => panic!("incoming closed"),
                Err(_) => continue,
            };
            let slice = Instant::now();
            let mut next = Some(first);
            while let Some(msg) = next {
                net.release(&msg);
                if msg.generation != generation {
                    wrong_generation += 1;
                }
                match msg.payload.first_chunk::<8>() {
                    Some(b) => {
                        let seq = u64::from_le_bytes(*b) as usize;
                        if seen.len() <= seq {
                            seen.resize(seq + 1, false);
                        }
                        if std::mem::replace(&mut seen[seq], true) {
                            duplicates += 1;
                        } else {
                            received += 1;
                        }
                    }
                    None => bad += 1,
                }
                store.insert(msg);
                next = if slice.elapsed() < MERGE_SLICE {
                    net.incoming.try_recv().ok()
                } else {
                    None
                };
            }
            if store.has_work() {
                store.work(slice + FRAME_BUDGET);
            }
            let stats = store.stats();
            max_used = max_used.max(stats.used_bytes);
            assert!(
                stats.used_bytes <= hard_limit,
                "store used {} B, over the hard limit {hard_limit} B",
                stats.used_bytes
            );
            slices += 1;
            slice_max = slice_max.max(slice.elapsed());
            tokio::time::sleep(FRAME).await;
        }
        thread.join().expect("publisher");
        let elapsed = start.elapsed();
        let sent = sent.load(Ordering::Relaxed);
        let missing = sent.saturating_sub(received);
        let stats = store.stats();
        let (net_drops, event_drops) = (
            net.stats.drops.load(Ordering::Relaxed),
            net.stats.event_drops.load(Ordering::Relaxed),
        );
        let growth = rss_base.zip(rss_end).map(|(a, b)| b - a);
        println!(
            "soak: {:.0}s sent {sent} received {received} lost_in_restarts {missing} dup {duplicates} \
             restarts {launched} windows {:?} publisher_connects {} subscribed {subscribed} \
             reconnected {reconnected} net_drops {net_drops} event_drops {event_drops} \
             window_drops {window_drops} store_drops {}/{} topics {} max_used {max_used} B \
             (hard limit {hard_limit} B) rss_growth {} B (max rss {} MiB) slices {slices} \
             slice_max {slice_max:?} errors_in_windows {errors_in_windows}",
            elapsed.as_secs_f64(),
            windows_done.iter().map(|d| d.as_secs()).collect::<Vec<_>>(),
            connects.load(Ordering::Relaxed),
            stats.drops,
            stats.topic_drops,
            stats.topics,
            growth.map_or("n/a".into(), |g| g.to_string()),
            rss_max / (1024 * 1024),
        );

        assert_eq!(launched, RESTARTS);
        assert_eq!(windows_done.len() as u64, RESTARTS, "restarts that ended with message flow");
        assert!(subscribed > RESTARTS, "the session did not subscribe again after each restart");
        assert!(bad_events.is_empty(), "events outside the windows: {bad_events:?}");
        assert_eq!(bad, 0, "messages without a sequence number");
        assert_eq!(duplicates, 0, "duplicate messages");
        assert_eq!(wrong_generation, 0, "messages of another generation");
        assert_eq!(
            net_drops + event_drops,
            window_drops,
            "net or event drops outside the restart windows"
        );
        // Loss is only allowed while the broker is down.
        let allowed = (rate as f64 * (window_time + GRACE * RESTARTS as u32).as_secs_f64()) as u64;
        assert!(missing <= allowed, "{missing} messages lost, at most {allowed} explained by restarts");
        assert_eq!((stats.drops, stats.topic_drops), (0, 0), "store drops");
        assert!(
            cfg!(not(target_os = "linux")) || secs <= BASELINE_AT || rss_base.is_some(),
            "no RSS baseline"
        );
        if let Some(growth) = growth {
            assert!(growth < RSS_LIMIT, "RSS grew by {growth} B after the baseline");
        }
        assert_eq!(
            net.status.borrow().state,
            ConnState::Connected,
            "the session is not connected at the end"
        );
        net.shutdown().await;
    });
}
