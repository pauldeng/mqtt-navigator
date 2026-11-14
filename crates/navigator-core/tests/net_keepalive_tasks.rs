//! Old keep-alive tasks end (plan, "Old keep-alive tasks end"). ntex-mqtt logs "start mqtt client
//! keep-alive task" (`log`, debug) when a task starts and "stopping keep-alive task" when the task
//! of a closed connection ends, at its first tick (one interval after the start). The test
//! captures the `log` records with their times, so it has its own test binary: the logger is
//! global. It connects and disconnects 20 times with keep alive 2 s against a fake broker. It
//! waits (bounded) for 20 starts, then for 20 stops. Each task is checked against its own
//! lifetime: no task ends before its first tick, and every task ends within a bound.

#[path = "common/fake.rs"]
mod fake;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use fake::{Fake, Options};
use navigator_core::net::{
    Command, ConnState, ConnectConfig, Net, NetLimits, Profile, Protocol, Transport,
};

/// The times of the start and stop records.
static STARTS: Mutex<Vec<Instant>> = Mutex::new(Vec::new());
static STOPS: Mutex<Vec<Instant>> = Mutex::new(Vec::new());

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, r: &log::Record<'_>) {
        let text = r.args().to_string();
        if text.contains("start mqtt client keep-alive task") {
            STARTS.lock().expect("lock").push(Instant::now());
        } else if text.contains("stopping keep-alive task") {
            STOPS.lock().expect("lock").push(Instant::now());
        }
    }
    fn flush(&self) {}
}

fn counts() -> (usize, usize) {
    (
        STARTS.lock().expect("lock").len(),
        STOPS.lock().expect("lock").len(),
    )
}

/// Waits until `done`, at most 10 s.
async fn wait_for(done: impl Fn() -> bool, what: &str) {
    let end = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < end, "timeout: {what} ({:?})", counts());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn phase(v: Protocol) {
    let f = Fake::start(Transport::Tcp, v, Options::default(), |_, mut conn| {
        if conn.accept_connect().is_ok() {
            conn.hold(Duration::from_secs(10));
        }
    });
    // Tasks of the earlier phase must have ended: every start has a stop.
    wait_for(
        || counts().0 == counts().1,
        "the tasks of the earlier phase end",
    )
    .await;
    let (base_starts, base_stops) = counts();
    let mut net = Net::start(NetLimits::default());
    for _ in 0..20 {
        let mut c = ConnectConfig::from(Profile {
            host: "127.0.0.1".into(),
            port: f.port,
            transport: Transport::Tcp,
            keepalive_secs: 2,
            subscriptions: vec![],
            protocol: v,
            ..Profile::default()
        });
        c.protocol = v;
        net.send(Command::Connect(Box::new(c))).expect("send");
        tokio::time::timeout(
            Duration::from_secs(5),
            net.status.wait_for(|s| s.state == ConnState::Connected),
        )
        .await
        .expect("connects")
        .expect("status");
        net.disconnect();
        tokio::time::timeout(
            Duration::from_secs(5),
            net.status.wait_for(|s| s.state == ConnState::Disconnected),
        )
        .await
        .expect("disconnects")
        .expect("status");
    }
    wait_for(|| counts().0 - base_starts >= 20, "20 start records").await;
    wait_for(|| counts().1 - base_stops >= 20, "20 stop records").await;
    // Take the records of this phase. Sorted pairs bound each task: a start and the stop that
    // follows it. The interval is 2 s; the timer can fire a little early or late.
    let starts = STARTS.lock().expect("lock")[base_starts..].to_vec();
    let stops = STOPS.lock().expect("lock")[base_stops..].to_vec();
    assert_eq!((v, starts.len(), stops.len()), (v, 20, 20));
    for (start, stop) in starts.iter().zip(&stops) {
        let lived = stop.duration_since(*start);
        assert!(
            (Duration::from_millis(1_800)..=Duration::from_millis(4_000)).contains(&lived),
            "{v:?}: a keep-alive task lived {lived:?}, not 1.8 s to 4 s"
        );
    }
    net.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn old_keep_alive_tasks_end() {
    log::set_logger(&Capture).expect("logger");
    log::set_max_level(log::LevelFilter::Debug);
    phase(Protocol::V311).await;
    phase(Protocol::V5).await;
}
