//! Load generator: publishes QoS 0 messages to `flood/t{i}` at a fixed rate.
//!
//! `cargo run -p navigator-core --release --example flood -- --host localhost --port 1883 \
//!     --rate 10000 --topics 10000 --size 100 --secs 60`

use std::time::{Duration, Instant};

use bytes::Bytes;
use navigator_core::model::Qos;

#[path = "../tests/common/mod.rs"]
mod common;
use common::Publisher;

struct Args {
    host: String,
    port: u16,
    rate: u64,
    topics: usize,
    size: usize,
    secs: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        host: "localhost".into(),
        port: 1883,
        rate: 10_000,
        topics: 10_000,
        size: 100,
        secs: 60,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or(format!("{flag} needs a value"))?;
        let num = |v: &str| v.parse::<u64>().map_err(|e| format!("{flag}: {e}"));
        match flag.as_str() {
            "--host" => a.host = value,
            "--port" => a.port = u16::try_from(num(&value)?).map_err(|e| e.to_string())?,
            "--rate" => a.rate = num(&value)?,
            "--topics" => {
                a.topics = usize::try_from(num(&value)?)
                    .map_err(|e| e.to_string())?
                    .max(1)
            }
            "--size" => a.size = usize::try_from(num(&value)?).map_err(|e| e.to_string())?,
            "--secs" => a.secs = num(&value)?,
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    Ok(a)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a = parse_args()?;
    let mut publisher = Publisher::connect(&a.host, a.port).await?;
    let topics: Vec<String> = (0..a.topics).map(|i| format!("flood/t{i}")).collect();
    let payload = Bytes::from(vec![b'x'; a.size]);
    let batch = (a.rate / 100).max(1);

    let mut tick = tokio::time::interval(Duration::from_millis(10));
    let mut report = Instant::now();
    let (start, mut sent, mut last_sent, mut next) = (Instant::now(), 0u64, 0u64, 0usize);
    while start.elapsed() < Duration::from_secs(a.secs) {
        tick.tick().await;
        if !publisher.connected() {
            return Err("connection ended".into());
        }
        for _ in 0..batch {
            // The queue of `Net` is bounded, so this waits when the broker is slower than the rate.
            publisher
                .send(&topics[next], payload.clone(), Qos::AtMostOnce)
                .await?;
            next = (next + 1) % topics.len();
            sent += 1;
        }
        if report.elapsed() >= Duration::from_secs(1) {
            publisher.drain();
            // Results that are not `Sent` (for example a local rejection) do not count.
            let good = sent - publisher.failed.len() as u64;
            let secs = report.elapsed().as_secs_f64();
            println!(
                "rate: {:.0} msg/s (total {good})",
                (good - last_sent) as f64 / secs
            );
            (report, last_sent) = (Instant::now(), good);
        }
    }
    publisher.settle(Duration::from_secs(5)).await;
    println!(
        "done: sent {sent}, results ok {}, failed {}",
        publisher.done,
        publisher.failed.len()
    );
    let (accepted, done, failed) = (publisher.accepted, publisher.done, publisher.failed.len());
    publisher.shutdown().await;
    if failed > 0 || done != accepted {
        return Err(format!(
            "accepted {accepted}, ok {done}, failed {failed}: the run is not clean"
        )
        .into());
    }
    Ok(())
}
