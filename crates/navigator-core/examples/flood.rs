//! Load generator: publishes QoS 0 messages to `flood/t{i}` at a fixed rate.
//!
//! `cargo run -p navigator-core --release --example flood -- --host localhost --port 1883 \
//!     --rate 10000 --topics 10000 --size 100 --secs 60`

use std::time::{Duration, Instant};

use bytes::Bytes;
use ms_mqtt_client::client::{ClientOptions, ConnectResult, KeepAliveConfig, new_client};
use ms_mqtt_client::packet::{ConnectProperties, PublishProperties};
use ms_mqtt_client::topic::TopicName;
use ms_mqtt_client::transport::{ConnectionTransportConfig, ConnectionTransportType};

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
    let (client, connect_handle, mut receiver) = new_client(ClientOptions::default());
    let connection = match connect_handle
        .connect(
            ConnectionTransportConfig {
                transport_type: ConnectionTransportType::Tcp {
                    hostname: a.host.clone(),
                    port: a.port,
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
        ConnectResult::Failure(_, e) => return Err(e.into()),
    };

    let topics: Vec<TopicName> = (0..a.topics)
        .map(|i| TopicName::new(format!("flood/t{i}")))
        .collect::<Result<_, _>>()?;
    let payload = Bytes::from(vec![b'x'; a.size]);
    let batch = (a.rate / 100).max(1);

    let sender = async {
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        let mut report = Instant::now();
        let (start, mut sent, mut last_sent, mut next) = (Instant::now(), 0u64, 0u64, 0usize);
        while start.elapsed() < Duration::from_secs(a.secs) {
            tick.tick().await;
            for _ in 0..batch {
                // The client queue is bounded, so this waits when the broker is slower than the rate.
                let token = client
                    .publish_qos0(
                        topics[next].clone(),
                        payload.clone(),
                        false,
                        PublishProperties::default(),
                    )
                    .await?;
                drop(token);
                next = (next + 1) % topics.len();
                sent += 1;
            }
            if report.elapsed() >= Duration::from_secs(1) {
                let secs = report.elapsed().as_secs_f64();
                println!(
                    "rate: {:.0} msg/s (total {sent})",
                    (sent - last_sent) as f64 / secs
                );
                (report, last_sent) = (Instant::now(), sent);
            }
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    };
    // The connection makes progress only while it is polled. The receiver stays empty (no subscriptions).
    tokio::select! {
        (_, event) = connection.run_until_disconnect() => Err(format!("connection ended: {event:?}").into()),
        _ = receiver.recv() => Ok(()),
        result = sender => result,
    }
}
