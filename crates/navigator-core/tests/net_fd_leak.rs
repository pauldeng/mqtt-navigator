//! A dial that a disconnect cancels leaves no file descriptor behind. The file descriptor count is
//! global to the process, so this test has its own test binary: no other test runs beside it. The
//! black hole is a Linux behaviour (a full accept queue drops new SYNs).
#![cfg(target_os = "linux")]

use std::time::Duration;

use navigator_core::net::{
    Command, ConnState, ConnectConfig, Net, NetLimits, Profile, Protocol, Transport,
};

async fn black_hole() -> (tokio::net::TcpListener, Vec<tokio::net::TcpStream>) {
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
    (listener, fill)
}

fn fds() -> usize {
    std::fs::read_dir("/proc/self/fd").expect("fd list").count()
}

async fn wait(net: &mut Net, f: impl Fn(&ConnState) -> bool) {
    tokio::time::timeout(Duration::from_secs(3), net.status.wait_for(|s| f(&s.state)))
        .await
        .expect("the state did not come in time")
        .expect("status");
}

/// Polls the count of file descriptors until `f` is true. Returns the last count. It gives up
/// after 3 s, so a leak shows as a wrong count and a missing dial as a count that stays low.
async fn poll_fds(mut f: impl FnMut(usize) -> bool) -> usize {
    let end = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let n = fds();
        if f(n) || tokio::time::Instant::now() >= end {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Cancelled dials on each transport (round 0 warms the lazy set-up up: the DNS thread, the TLS
/// context; the count is taken after it). MQTT 5 only: the dial code is the same for both
/// versions, because it runs before any MQTT byte.
#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_black_hole_dial_leaves_no_file_descriptor() {
    let (listener, _fill) = black_hole().await;
    let port = listener.local_addr().expect("addr").port();
    let mut net = Net::start(NetLimits::default());
    for transport in [
        Transport::Tcp,
        Transport::Tls,
        Transport::Ws,
        Transport::Wss,
    ] {
        let mut warm = 0;
        for round in 0..3 {
            let mut c = ConnectConfig::from(Profile {
                host: "127.0.0.1".into(),
                port,
                transport,
                ws_path: "/mqtt".into(),
                subscriptions: vec![],
                protocol: Protocol::V5,
                ..Profile::default()
            });
            c.protocol = Protocol::V5;
            net.send(Command::Connect(Box::new(c))).expect("send");
            wait(&mut net, |s| *s == ConnState::Connecting).await;
            if round > 0 {
                // The dial is open: its socket is one more descriptor.
                let open = poll_fds(|n| n > warm).await;
                assert!(open > warm, "{transport:?}: the dial opened no socket");
            }
            net.disconnect();
            wait(&mut net, |s| *s == ConnState::Disconnected).await;
            if round == 0 {
                // Let the descriptors settle: two equal counts in a row.
                let mut last = usize::MAX;
                warm = poll_fds(|n| std::mem::replace(&mut last, n) == n).await;
            } else {
                let after = poll_fds(|n| n == warm).await;
                assert_eq!(after, warm, "{transport:?}: a file descriptor leaked");
            }
        }
    }
    net.shutdown().await;
}
