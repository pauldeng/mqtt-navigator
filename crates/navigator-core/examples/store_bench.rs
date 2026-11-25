//! Store insert benchmark: 200 000 messages with 10 MQTT 5 user properties each, history and
//! list on. Prints the wall time and the peak RSS.
//!
//! `cargo run --release -p navigator-core --example store_bench [topics]` (default 1000 topics)

use std::sync::Arc;
use std::time::{Instant, SystemTime};

use bytes::Bytes;
use navigator_core::net::Protocol;
use navigator_core::store::{BUDGET_MAX, Limits, TopicStore};
use navigator_core::{Incoming, MsgProps, Qos};

const MESSAGES: usize = 200_000;

fn rss_kib(key: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

fn main() {
    let topic_count: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000);
    // The budget is large: nothing is dropped, so every message pays for all three copies.
    let mut store = TopicStore::new(Limits::with_budget(BUDGET_MAX));
    let topics: Vec<Arc<str>> = (0..topic_count)
        .map(|i| Arc::from(format!("bench/t{i}")))
        .collect();
    let payload = Bytes::from(vec![b'x'; 100]);
    let start = Instant::now();
    for i in 0..MESSAGES {
        let props = MsgProps {
            user: (0..10)
                .map(|k| {
                    (
                        format!("key-{k}-0123456789"),
                        format!("value-{i}-{k}-0123456789"),
                    )
                })
                .collect(),
            ..MsgProps::default()
        };
        store.insert(Incoming {
            generation: 0,
            topic: Arc::clone(&topics[i % topic_count]),
            payload: payload.clone(),
            qos: Qos::AtMostOnce,
            retain: false,
            props,
            received_at: SystemTime::now(),
            protocol: Protocol::V5,
        });
    }
    let st = store.stats();
    println!(
        "inserted {MESSAGES} in {:.0} ms, peak RSS {} MiB, end RSS {} MiB, store counts {} MiB, drops {}, list rows {}",
        start.elapsed().as_secs_f64() * 1000.0,
        rss_kib("VmHWM:") / 1024,
        rss_kib("VmRSS:") / 1024,
        st.used_bytes / (1024 * 1024),
        st.drops,
        st.list_rows,
    );
}
