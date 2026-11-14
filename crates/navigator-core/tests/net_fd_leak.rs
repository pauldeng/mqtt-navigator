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

/// Five cancelled dials on each transport, for both versions.
#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_black_hole_dial_leaves_no_file_descriptor() {
    let (listener, _fill) = black_hole().await;
    let port = listener.local_addr().expect("addr").port();
    let mut net = Net::start(NetLimits::default());
    for protocol in [Protocol::V311, Protocol::V5] {
        for transport in [
            Transport::Tcp,
            Transport::Tls,
            Transport::Ws,
            Transport::Wss,
        ] {
            // One dial first, so that lazy set-up (DNS thread, TLS context) is not counted.
            let before = {
                let mut warm = 0;
                for round in 0..6 {
                    let mut c = ConnectConfig::from(Profile {
                        host: "127.0.0.1".into(),
                        port,
                        transport,
                        ws_path: "/mqtt".into(),
                        subscriptions: vec![],
                        protocol,
                        ..Profile::default()
                    });
                    c.protocol = protocol;
                    net.send(Command::Connect(Box::new(c))).expect("send");
                    wait(&mut net, |s| *s == ConnState::Connecting).await;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    net.disconnect();
                    wait(&mut net, |s| *s == ConnState::Disconnected).await;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    if round == 0 {
                        warm = fds();
                    }
                }
                warm
            };
            assert_eq!(
                fds(),
                before,
                "{protocol:?} {transport:?}: a file descriptor leaked"
            );
        }
    }
    net.shutdown().await;
}
