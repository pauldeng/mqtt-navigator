//! S1 spike: does ntex-mqtt 10.0.0 work inside our runtime set-up?
//!
//! The runtime is the one of the net thread: `current_thread` + `LocalSet` +
//! `ntex_net::with_reactor(tokio Reactor)`, and no ntex `System`.
//!
//! Run: `tests/broker/up.sh mosquitto && cargo run --release -p navigator-core --example ntex_spike`.
//! `SPIKE_FULL=1` runs the frame read rate cases with the plan values (10 s, 60 s).
//! `SPIKE_ONLY=text` runs only the cases whose name contains `text`.

#[path = "ntex_spike/cases.rs"]
mod cases;
#[path = "ntex_spike/dialer.rs"]
mod dialer;
#[path = "ntex_spike/fake.rs"]
mod fake;
#[path = "ntex_spike/fakecases.rs"]
mod fakecases;
#[path = "ntex_spike/wire.rs"]
mod wire;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use wire::{Tr, Ver};

struct Row {
    name: String,
    ok: bool,
    detail: String,
    secs: f64,
}

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, r: &log::Record<'_>) {
        if std::env::var_os("SPIKE_LOG").is_some() {
            eprintln!("log {} {}", r.target(), r.args());
        }
        if r.args().to_string().contains("stopping keep-alive task") {
            fakecases::KA_STOPS.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn flush(&self) {}
}

fn main() {
    log::set_logger(&Capture).expect("logger");
    log::set_max_level(if std::env::var_os("SPIKE_LOG").is_some() {
        log::LevelFilter::Trace
    } else {
        log::LevelFilter::Debug
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let reactor: Box<dyn ntex_net::Reactor> = Box::new(ntex_net::tokio::Reactor);
    let failed = ntex_net::with_reactor(&reactor, || {
        tokio::task::LocalSet::new().block_on(&rt, run())
    });
    std::process::exit(i32::from(failed));
}

async fn case(rows: &mut Vec<Row>, name: &str, f: impl Future<Output = Result<String, String>>) {
    if let Ok(only) = std::env::var("SPIKE_ONLY")
        && !name.contains(&only)
    {
        return;
    }
    let t = Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(200), f)
        .await
        .unwrap_or_else(|_| Err("case hung for 200 s".into()));
    let secs = t.elapsed().as_secs_f64();
    let (ok, detail) = match r {
        Ok(d) => (true, d),
        Err(d) => (false, d),
    };
    eprintln!("{} {name} ({secs:.2} s)", if ok { "PASS" } else { "FAIL" });
    rows.push(Row {
        name: name.into(),
        ok,
        detail,
        secs,
    });
}

async fn run() -> bool {
    let mut rows = Vec::new();
    let r = &mut rows;
    let vs = [Ver::V3, Ver::V5];

    // a: Mosquitto matrix.
    for v in vs {
        let n = |l: &str| format!("a {v:?} {l}");
        case(
            r,
            &n("TCP"),
            cases::matrix_one(v, Tr::Tcp, 18830, "localhost", false, false, None),
        )
        .await;
        case(
            r,
            &n("TCP, user and password"),
            cases::matrix_one(
                v,
                Tr::Tcp,
                18831,
                "localhost",
                false,
                false,
                Some(("test", "test-pass")),
            ),
        )
        .await;
        case(
            r,
            &n("TLS (localhost)"),
            cases::matrix_one(v, Tr::Tls, 18832, "localhost", true, false, None),
        )
        .await;
        case(
            r,
            &n("TLS (127.0.0.1, IP in SAN)"),
            cases::matrix_one(v, Tr::Tls, 18832, "127.0.0.1", true, false, None),
        )
        .await;
        case(
            r,
            &n("mTLS"),
            cases::matrix_one(v, Tr::Tls, 18833, "localhost", true, true, None),
        )
        .await;
        case(
            r,
            &n("WS"),
            cases::matrix_one(v, Tr::Ws, 18834, "localhost", false, false, None),
        )
        .await;
        case(
            r,
            &n("WSS"),
            cases::matrix_one(v, Tr::Wss, 18835, "localhost", true, false, None),
        )
        .await;
    }

    // b: TLS failures.
    let expired = "The server certificate expired on Jan  2 00:00:00 2020 GMT. Renew it, or turn off verification.";
    for v in vs {
        case(
            r,
            &format!("b {v:?} TLS expired certificate"),
            cases::tls_fail(v, 18836, true, expired),
        )
        .await;
        case(
            r,
            &format!("b {v:?} TLS wrong host name"),
            cases::tls_fail(
                v,
                18837,
                true,
                "The server certificate is for another host name.",
            ),
        )
        .await;
        case(
            r,
            &format!("b {v:?} TLS unknown CA (no CA file)"),
            cases::tls_fail(
                v,
                18832,
                false,
                "The server certificate is not trusted. Add its CA file, or turn off verification.",
            ),
        )
        .await;
        case(
            r,
            &format!("b {v:?} mTLS without client certificate"),
            cases::mtls_refused(v, Tr::Tls, 18833),
        )
        .await;
    }
    case(
        r,
        "b local key texts (encrypted, wrong passphrase, mismatch)",
        async { cases::local_key_texts() },
    )
    .await;

    // c: sizes.
    case(r, "c v3 varint boundaries (helper unit check)", async {
        cases::varint_boundaries()
    })
    .await;
    for v in vs {
        for (tr, port) in [(Tr::Tcp, 18830), (Tr::Ws, 18834), (Tr::Wss, 18835)] {
            case(
                r,
                &format!("c {v:?} {tr:?} exactly 1 MiB packet out and in"),
                cases::size_exact(v, tr, port),
            )
            .await;
        }
        case(
            r,
            &format!("c {v:?} 1 MiB + 1 byte outgoing (v3 refused, v5 sent as today)"),
            cases::size_plus_one_out(v),
        )
        .await;
        case(
            r,
            &format!("c {v:?} incoming exactly 1 MiB (fake)"),
            cases::size_incoming(v, 0),
        )
        .await;
        case(
            r,
            &format!("c {v:?} incoming 1 MiB + 1 byte (fake)"),
            cases::size_incoming(v, 1),
        )
        .await;
    }
    case(
        r,
        "c V3 non-minimal length encoding of r passes (fake)",
        cases::size_nonminimal(0),
    )
    .await;
    case(
        r,
        "c V3 non-minimal length encoding of r + 1 fails (fake)",
        cases::size_nonminimal(1),
    )
    .await;
    case(
        r,
        "c V5 CONNACK announces Maximum Packet Size 1 MiB: +1 byte refused before the send (fake)",
        cases::size_announced_limit(),
    )
    .await;

    // d: fake broker.
    for v in vs {
        let n = |l: &str| format!("d {v:?} {l}");
        case(
            r,
            &n("stalled write, write timeout 10 s, keep alive 0"),
            fakecases::stalled_write(v, 0, 10, 12.5),
        )
        .await;
        case(
            r,
            &n("blocked write buffer + missing PINGRESP (ka 2 s)"),
            fakecases::stalled_write(v, 2, 10, 2.0 * 2.0 * 1.125 + 12.5),
        )
        .await;
        case(
            r,
            &n("PINGREQ never answered, no publishes (ka 2 s)"),
            fakecases::ping_unanswered(v, false, 2),
        )
        .await;
        case(
            r,
            &n("PINGREQ never answered, with publishes (ka 2 s)"),
            fakecases::ping_unanswered(v, true, 2),
        )
        .await;
        case(
            r,
            &n("old keep-alive tasks end (20 connects)"),
            fakecases::keepalive_tasks(v),
        )
        .await;
        case(r, &n("2 MiB header, 1 MiB limit"), fakecases::big_header(v)).await;
        case(
            r,
            &n("incoming QoS 2 never delivered"),
            fakecases::incoming_qos2(v),
        )
        .await;
        for (tls_on, tn) in [(false, "TCP"), (true, "TLS")] {
            case(
                r,
                &n(&format!("{tn} half fixed header, ka 0, frame read rate")),
                fakecases::half_frame(v, false, tls_on),
            )
            .await;
            case(
                r,
                &n(&format!("{tn} half PUBLISH payload, ka 0, frame read rate")),
                fakecases::half_frame(v, true, tls_on),
            )
            .await;
        }
        for tr in [Tr::Tcp, Tr::Tls, Tr::Ws, Tr::Wss] {
            case(
                r,
                &n(&format!("{tr:?} disconnect while a half frame waits")),
                fakecases::disconnect_in_half_frame(v, tr),
            )
            .await;
        }
        case(
            r,
            &n("WS half binary frame, ka 2 s"),
            fakecases::ws_half_frame(v),
        )
        .await;
        case(
            r,
            &n("WS upgrade never answered (WebSocket handshake timeout)"),
            fakecases::stalled_ws_upgrade(v),
        )
        .await;
        case(
            r,
            &n("WSS half binary frame, ka 2 s"),
            fakecases::wss_half_frame(v),
        )
        .await;
        case(
            r,
            &n("TCP trickling payload (rate met, never completes), ka 0"),
            fakecases::trickle(v),
        )
        .await;
        case(
            r,
            &n("WS fragmented packet, Ping, Pong, Text"),
            fakecases::ws_frames(v),
        )
        .await;
        case(
            r,
            &n("handler panic after await, second call pending"),
            fakecases::handler_panic(v),
        )
        .await;
        case(
            r,
            &n("disconnect during a stalled send (force_close)"),
            fakecases::disconnect_stalled_send(v),
        )
        .await;
    }
    case(
        r,
        "d V5 broker DISCONNECT reason kept",
        fakecases::broker_disconnect(),
    )
    .await;
    case(
        r,
        "d V5 Server Keep Alive 5 s followed",
        fakecases::server_keep_alive(5),
    )
    .await;
    case(
        r,
        "d V5 Server Keep Alive 0 followed (no pings)",
        fakecases::server_keep_alive(0),
    )
    .await;
    case(
        r,
        "d PINGREQ never answered, keep alive 60 s and 70 s, with and without publishes, v3 and v5 (8 runs together)",
        fakecases::long_pings(),
    )
    .await;
    case(
        r,
        "d deadlines: silent CONNACK, stalled TLS, black-hole dial",
        fakecases::deadlines(),
    )
    .await;
    case(
        r,
        "d refused CONNACK (v3 code 5, v5 0x87 + reason + reference)",
        fakecases::refused_connack(),
    )
    .await;
    case(
        r,
        "d version errors as raw bytes",
        fakecases::version_errors(),
    )
    .await;
    case(
        r,
        "d first address refuses, second works",
        fakecases::first_address_refuses(),
    )
    .await;
    case(
        r,
        "d stalled DNS, 10 cancelled attempts",
        fakecases::stalled_dns(),
    )
    .await;
    case(
        r,
        "d black-hole dial cancelled leaves no fd",
        fakecases::blackhole_fd(),
    )
    .await;

    let failed = rows.iter().filter(|x| !x.ok).count();
    println!("\n| Case | Result | Time | Detail |\n|---|---|---|---|");
    for x in &rows {
        println!(
            "| {} | {} | {:.2} s | {} |",
            x.name,
            if x.ok { "PASS" } else { "FAIL" },
            x.secs,
            x.detail.replace('|', "/").replace('\n', " ")
        );
    }
    println!(
        "\nincoming messages dropped by the bounded queue: {}",
        wire::DROPPED.load(Ordering::Relaxed)
    );
    println!(
        "\n{} cases, {} passed, {failed} failed",
        rows.len(),
        rows.len() - failed
    );
    failed > 0
}
