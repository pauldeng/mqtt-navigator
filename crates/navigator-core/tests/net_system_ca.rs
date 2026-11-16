//! The system CA store against a real broker (Mosquitto TLS, 18832). The checks need
//! `SSL_CERT_FILE`, so the test runs them in a child process of this binary with that variable.
//! Run: `tests/broker/up.sh && cargo test -p navigator-core --features broker-tests --test
//! net_system_ca`.
#![cfg(feature = "broker-tests")]

use std::path::PathBuf;
use std::time::Duration;

use navigator_core::net::{ConnState, Net, NetLimits, Profile, Protocol, Transport};

fn cert(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/broker/certs")
        .join(name)
}

async fn outcome(net: &mut Net, ca_file: Option<PathBuf>) -> ConnState {
    let profile = Profile {
        host: "localhost".into(),
        port: 18832,
        transport: Transport::Tls,
        protocol: Protocol::V311,
        ca_file,
        ..Profile::default()
    };
    let previous = net.status.borrow().generation;
    net.send(navigator_core::net::Command::Connect(Box::new(
        profile.into(),
    )))
    .expect("send");
    let state = tokio::time::timeout(
        Duration::from_secs(10),
        net.status.wait_for(|s| {
            s.generation > previous
                && matches!(s.state, ConnState::Connected | ConnState::Failed(_))
        }),
    )
    .await
    .expect("timeout")
    .expect("status")
    .state
    .clone();
    net.disconnect();
    state
}

const CHILD: &str = "NET_SYSTEM_CA_CHILD";
const CHILD_TEST: &str = "checks_in_child";

/// Starts this binary again with `SSL_CERT_FILE` set. The child output shows the failed check.
#[test]
fn system_store_is_the_default_and_a_custom_ca_replaces_it() {
    let out = std::process::Command::new(std::env::current_exe().expect("exe"))
        .args(["--ignored", "--exact", CHILD_TEST, "--nocapture"])
        .env(CHILD, "1")
        .env("SSL_CERT_FILE", cert("ca.crt"))
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Runs only in the child of the test above.
#[tokio::test(flavor = "current_thread")]
#[ignore = "runs in the child process of system_store_is_the_default_and_a_custom_ca_replaces_it"]
async fn checks_in_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let mut net = Net::start(NetLimits::default());

    // No CA file: the system store (here the test CA) makes the broker trusted.
    assert_eq!(outcome(&mut net, None).await, ConnState::Connected);

    // A CA file that is not the broker CA: only that file is trusted, so the broker is refused,
    // although the system store would trust it.
    match outcome(&mut net, Some(cert("wrong-host.crt"))).await {
        ConnState::Failed(text) => assert!(text.contains("not trusted"), "{text}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    net.shutdown().await;
}
