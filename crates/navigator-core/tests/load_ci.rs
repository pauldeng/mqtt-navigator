//! Headless load test: 10 000 msg/s over 10 000 topics through `Net` into `TopicStore`, with a
//! consumer that works like the UI scheduler. It uses the anonymous Docker listener (18830).
//!
//! Run: `tests/broker/up.sh && cargo test --release -p navigator-core --features broker-tests
//! --test load_ci -- --nocapture && tests/broker/down.sh`.
//! Time: `LOAD_WARMUP_SECS` (default 60) and `LOAD_SECS` (default 300), for example 10 and 60.
//! The RSS interval starts after the warm-up and after the store is saturated. The publisher
//! runs until the interval has lasted `LOAD_SECS`. The test fails if the store is not saturated
//! within `LOAD_WARMUP_SECS` + `LOAD_SECS`. In a release build, the test also checks the merge
//! slices: p99 at most 4 ms and the longest at most 8 ms.
#![cfg(feature = "broker-tests")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use ms_mqtt_client::client::{ClientOptions, ConnectResult, KeepAliveConfig, new_client};
use ms_mqtt_client::packet::{ConnectProperties, PublishProperties};
use ms_mqtt_client::topic::TopicName;
use ms_mqtt_client::transport::{ConnectionTransportConfig, ConnectionTransportType};
use navigator_core::model::Qos;
use navigator_core::net::{Command, ConnState, Event, Net, NetLimits, Profile, Transport};
use navigator_core::store::{Limits, TopicStore};

const PORT: u16 = 18830;
const RATE: u64 = 10_000;
const TOPICS: usize = 10_000;
const SIZE: usize = 100;
/// The UI merges at most this long in each frame, then waits one frame.
const MERGE_SLICE: Duration = Duration::from_millis(4);
const FRAME: Duration = Duration::from_millis(33);
const RSS_LIMIT: i64 = 10 * 1024 * 1024;
/// Limits of the merge slice (release build only): p99 and maximum.
const SLICE_P99: Duration = Duration::from_millis(4);
const SLICE_MAX: Duration = Duration::from_millis(8);

fn env_secs(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Resident set size in bytes. `None` when the platform has no `/proc`.
/// A smaller store budget than the app default, so that the store reaches saturation (and
/// eviction runs) during the warm-up.
fn limits() -> Limits {
    Limits {
        budget: 64 * 1024 * 1024,
        node_budget: 16 * 1024 * 1024,
        ..Limits::default()
    }
}

fn rss() -> Option<i64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: i64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

/// Publishes QoS 0 messages at `RATE` until `stop` is set (or `hard_cap` has passed). The first
/// 8 payload bytes are the sequence number. `sent` counts the messages that the client accepted.
fn publisher(
    start: Instant,
    hard_cap: Duration,
    stop: Arc<AtomicBool>,
    sent: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let result = rt.block_on(async {
        let (client, connect_handle, mut receiver) = new_client(ClientOptions::default());
        let connection = match connect_handle
            .connect(
                ConnectionTransportConfig {
                    transport_type: ConnectionTransportType::Tcp {
                        hostname: "127.0.0.1".into(),
                        port: PORT,
                    },
                    timeout: Some(Duration::from_secs(10)),
                    proxy: None,
                    tcp_nodelay: true,
                },
                true,
                KeepAliveConfig::Infinite,
                None,
                None,
                None,
                ConnectProperties::default(),
                Some(Duration::from_secs(10)),
            )
            .await
        {
            ConnectResult::Success(connection, _, _handle) => connection,
            ConnectResult::Failure(_, e) => return Err(format!("publisher connect: {e:?}")),
        };
        let topics: Vec<TopicName> = (0..TOPICS)
            .map(|i| TopicName::new(format!("load/t{i}")).expect("topic"))
            .collect();
        let batch = RATE / 100;
        let sender = async {
            let mut tick = tokio::time::interval(Duration::from_millis(10));
            let mut seq = 0u64;
            let mut last = None;
            while !stop.load(Ordering::Acquire) && start.elapsed() < hard_cap {
                tick.tick().await;
                for _ in 0..batch {
                    let mut payload = vec![b'x'; SIZE];
                    payload[..8].copy_from_slice(&seq.to_le_bytes());
                    // The client queue is bounded, so this waits when the broker is slower.
                    let token = client
                        .publish_qos0(
                            topics[seq as usize % TOPICS].clone(),
                            Bytes::from(payload),
                            false,
                            PublishProperties::default(),
                        )
                        .await
                        .map_err(|e| format!("publish: {e:?}"))?;
                    last = Some(token);
                    seq += 1;
                    sent.store(seq, Ordering::Relaxed);
                }
            }
            // Let the client write the queued messages before it closes.
            if let Some(token) = last {
                let _ = token.await;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok::<(), String>(())
        };
        // The connection makes progress only while it is polled.
        tokio::select! {
            (_, event) = connection.run_until_disconnect() => Err(format!("publisher connection ended: {event:?}")),
            _ = receiver.recv() => Ok(()),
            result = sender => result,
        }
    });
    done.store(true, Ordering::Release);
    if let Err(e) = result {
        panic!("{e}");
    }
}

#[test]
fn ten_thousand_messages_per_second_through_net_and_store() {
    let warmup = Duration::from_secs(env_secs("LOAD_WARMUP_SECS", 60));
    let measure = Duration::from_secs(env_secs("LOAD_SECS", 300));
    // The consumer ends the run. This is only a safety stop.
    let hard_cap = warmup + 2 * measure + Duration::from_secs(60);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(Profile {
            host: "127.0.0.1".into(),
            port: PORT,
            transport: Transport::Tcp,
            subscriptions: vec![("load/#".into(), Qos::AtMostOnce)],
            ..Profile::default()
        })))
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
        match tokio::time::timeout(Duration::from_secs(10), net.events.recv()).await {
            Ok(Some(Event::Subscribed { result: Ok(_), .. })) => {}
            other => panic!("subscribe failed: {other:?}"),
        }

        let (sent, done) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
        let stop = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        let thread = {
            let (stop, sent, done) = (stop.clone(), sent.clone(), done.clone());
            std::thread::spawn(move || publisher(start, hard_cap, stop, sent, done))
        };

        let mut store = TopicStore::new(limits());
        let (mut received, mut duplicates, mut bad) = (0u64, 0u64, 0u64);
        // One flag for each sequence number. 10 000 msg/s for 6 min is 3.6 MB.
        let mut seen: Vec<bool> = Vec::new();
        let mut rss_warm = None;
        // The measured interval: from the baseline until `stop_at`.
        let mut baseline_at: Option<Instant> = None;
        let mut stop_at: Option<Instant> = None;
        let mut rss_end = None;
        let mut slices: Vec<Duration> = Vec::new();
        let mut done_at: Option<Instant> = None;
        loop {
            // The RSS baseline is taken after the warm-up AND once the store is nearly full, so
            // that the check measures growth at saturation, not the store filling its budget.
            if baseline_at.is_none()
                && stop_at.is_none()
                && start.elapsed() >= warmup
                && store.stats().used_bytes >= limits().budget / 10 * 9
            {
                rss_warm = rss();
                baseline_at = Some(Instant::now());
                eprintln!("load: store saturated at {:.0}s", start.elapsed().as_secs_f64());
            }
            // The publisher goes on until the interval has its full length.
            if stop_at.is_none() {
                let end = match baseline_at {
                    Some(t0) => t0.elapsed() >= measure,
                    None => start.elapsed() >= warmup + measure, // never saturated: give up
                };
                if end {
                    rss_end = rss();
                    stop_at = Some(Instant::now());
                    stop.store(true, Ordering::Release);
                }
            }
            if done.load(Ordering::Acquire) {
                let done_at = *done_at.get_or_insert_with(Instant::now);
                let all = sent.load(Ordering::Relaxed);
                if received + duplicates >= all || done_at.elapsed() > Duration::from_secs(10) {
                    break;
                }
            }
            // The UI scheduler: wait for a message, merge for at most 4 ms, then wait one frame.
            let first = match tokio::time::timeout(Duration::from_millis(500), net.incoming.recv())
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
            slices.push(slice.elapsed());
            tokio::time::sleep(FRAME).await;
        }
        let elapsed = start.elapsed();
        thread.join().expect("publisher");

        let baseline_at = baseline_at.expect("the store did not reach saturation in time");
        let stop_at = stop_at.expect("the run ended without the stop");
        let measured = stop_at.duration_since(baseline_at);
        assert!(measured >= measure, "measured interval {measured:?} is too short");
        let sent = sent.load(Ordering::Relaxed);
        let rate = sent as f64 / stop_at.duration_since(start).as_secs_f64();
        let rss_growth = rss_warm.zip(rss_end).map(|(a, b)| b - a);
        slices.sort();
        let p99 = slices[(slices.len() * 99).div_ceil(100).saturating_sub(1)];
        let slice_max = slices.last().copied().unwrap_or_default();
        let stats = store.stats();
        let net_drops = net.stats.drops.load(Ordering::Relaxed);
        let event_drops = net.stats.event_drops.load(Ordering::Relaxed);
        let missing = sent.saturating_sub(received);
        println!(
            "load: sent {sent} ({rate:.0} msg/s) received {received} missing {missing} dup {duplicates} \
             net_drops {net_drops} event_drops {event_drops} store_drops {}/{} topics {} used {} B \
             rss_growth {} B over {:.0}s elapsed {:.0}s; merge slices {} p99 {p99:?} max {slice_max:?}",
            stats.drops,
            stats.topic_drops,
            stats.topics,
            stats.used_bytes,
            rss_growth.map_or("n/a".into(), |g| g.to_string()),
            measured.as_secs_f64(),
            elapsed.as_secs_f64(),
            slices.len(),
        );

        assert!(rate >= 0.95 * RATE as f64, "publish rate {rate:.0} msg/s");
        assert_eq!(bad, 0, "messages without a sequence number");
        assert_eq!(duplicates, 0, "duplicate messages");
        assert_eq!(missing, 0, "{missing} of {sent} messages missing");
        assert_eq!(net_drops, 0, "net drops");
        assert_eq!(event_drops, 0, "event drops");
        assert_eq!((stats.drops, stats.topic_drops), (0, 0), "store drops");
        assert!(stats.used_bytes <= limits().budget, "over budget");
        if cfg!(not(debug_assertions)) {
            assert!(p99 <= SLICE_P99, "merge slice p99 {p99:?}");
            assert!(slice_max <= SLICE_MAX, "longest merge slice {slice_max:?}");
        }
        assert!(
            cfg!(not(target_os = "linux")) || rss_warm.is_some(),
            "the store did not reach saturation, so the RSS check did not run"
        );
        let status = net.status.borrow().clone();
        assert_eq!(
            (status.generation, status.state),
            (generation, ConnState::Connected),
            "the connection was lost"
        );
        if let Some(growth) = rss_growth {
            assert!(growth < RSS_LIMIT, "RSS grew by {growth} B after warm-up");
        }
        net.shutdown().await;
    });
}
