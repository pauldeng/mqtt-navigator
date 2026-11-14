//! Case d: fake-broker cases (raw tokio TCP on loopback).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::time::{sleep, timeout};

use crate::cases::tls;
use crate::fake::{self, Fake, drain, handshake, read_pkt};
use crate::wire::{Conn, End, Opts, PROBE, Tr, Ver, connect};

pub static KA_STOPS: AtomicUsize = AtomicUsize::new(0);

/// True when `SPIKE_FULL=1`: the frame read rate runs at the plan values (10 s, 60 s).
pub fn full() -> bool {
    std::env::var("SPIKE_FULL").is_ok_and(|v| v == "1")
}

fn s(d: Duration) -> String {
    format!("{:.2} s", d.as_secs_f64())
}

pub fn fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd").map_or(0, Iterator::count)
}

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0)
}

async fn end_of(
    h: tokio::task::JoinHandle<End>,
    limit: Duration,
) -> Result<(End, Duration), String> {
    let t = Instant::now();
    match timeout(limit, h).await {
        Ok(Ok(e)) => Ok((e, t.elapsed())),
        Ok(Err(e)) => Err(format!("end task failed: {e}")),
        Err(_) => Err(format!("connection still open after {}", s(limit))),
    }
}

/// Publisher that sends big QoS 0 payloads until the first call that ends badly. It records the
/// start of the last call, the time of the end, and the result text.
struct Flood {
    last_start: Rc<Cell<Instant>>,
    ended: Rc<RefCell<Option<(Instant, String)>>>,
}

fn flood(c: &Conn, op_timeout: Duration) -> Flood {
    let sink = c.sink.clone();
    let last_start = Rc::new(Cell::new(Instant::now()));
    let ended = Rc::new(RefCell::new(None));
    let (ls, en) = (last_start.clone(), ended.clone());
    tokio::task::spawn_local(async move {
        loop {
            ls.set(Instant::now());
            match timeout(op_timeout, sink.publish("big", vec![7; 512 * 1024], 0)).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    *en.borrow_mut() = Some((Instant::now(), e));
                    return;
                }
                Err(_) => {
                    *en.borrow_mut() = Some((Instant::now(), "Timed out".into()));
                    return;
                }
            }
        }
    });
    Flood { last_start, ended }
}

/// Fake broker that accepts, answers CONNACK and never reads again. Returns its port.
async fn deaf_fake(ver: Ver) -> u16 {
    let f = Fake::with_rcvbuf(Some(4096)).await;
    let (l, port) = (f.l, f.port);
    tokio::task::spawn_local(async move {
        let mut st = l.accept().await.expect("accept").0;
        let _ = handshake(&mut st, ver).await;
        std::future::pending::<()>().await;
    });
    port
}

/// Stalled write with backpressure. The peer stops reading. `ka` 0 isolates the write timeout.
pub async fn stalled_write(ver: Ver, ka: u16, wt: u16, bound: f64) -> Result<String, String> {
    let port = deaf_fake(ver).await;
    let mut o = Opts::new(ver, Tr::Tcp, "127.0.0.1", port);
    o.keep_alive = ka;
    o.write_timeout = wt;
    let c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
    let op = Duration::from_secs(u64::from(wt));
    let fl = flood(&c, op);
    let t_conn = Instant::now();
    let (end, _) = end_of(c.end, Duration::from_secs(u64::from(wt) + 30)).await?;
    let t_end = Instant::now();
    sleep(Duration::from_millis(100)).await;
    let blocked = fl.last_start.get();
    let attempt = t_end.duration_since(blocked).as_secs_f64();
    let publish = fl.ended.borrow().clone();
    let pub_txt = publish
        .clone()
        .map_or("publish still pending".into(), |(t, e)| {
            format!("publish ended \"{e}\" at {}", s(t.duration_since(blocked)))
        });
    let detail = format!(
        "attempt ended {attempt:.2} s after the blocked send began (bound {bound:.2} s, total {}); {pub_txt}; stop: {}",
        s(t_end.duration_since(t_conn)),
        end.stop
    );
    // The send must have been pending, and must end "Timed out" at the operation deadline.
    let wtf = f64::from(wt);
    let send_ok = publish.as_ref().is_some_and(|(t, e)| {
        let d = t.duration_since(blocked).as_secs_f64();
        e == "Timed out" && (wtf - 0.3..=wtf + 1.0).contains(&d)
    });
    if attempt <= bound && send_ok && end.stop.contains("WriteTimeout") {
        Ok(detail)
    } else {
        Err(format!(
            "{detail} (checks: attempt <= bound, send \"Timed out\" in [wt-0.3, wt+1], stop WriteTimeout)"
        ))
    }
}

/// PINGREQ is never answered. The bound is 2 x ka x 1.125 + 1 s. The connection must live at
/// least 2 x ka, and the broker must see exactly one PINGREQ, at about ka.
pub async fn ping_unanswered(ver: Ver, with_publish: bool, ka: u16) -> Result<String, String> {
    let f = Fake::new().await;
    let pings = Rc::new(RefCell::new(Vec::<Instant>::new()));
    let (l, port, p2) = (f.l, f.port, pings.clone());
    tokio::task::spawn_local(async move {
        let mut st = l.accept().await.expect("accept").0;
        let _ = handshake(&mut st, ver).await;
        while let Ok((h, _)) = read_pkt(&mut st).await {
            if h == 0xC0 {
                p2.borrow_mut().push(Instant::now());
            }
        }
    });
    let mut o = Opts::new(ver, Tr::Tcp, "127.0.0.1", port);
    o.keep_alive = ka;
    let c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
    let t0 = Instant::now();
    if with_publish {
        let sink = c.sink.clone();
        tokio::task::spawn_local(async move {
            while sink.is_open() {
                let _ = sink.publish("p", vec![1; 100], 0).await;
                sleep(Duration::from_millis(200)).await;
            }
        });
    }
    let kaf = f64::from(ka);
    let (end, took) = end_of(c.end, Duration::from_secs(u64::from(ka) * 3)).await?;
    let bound = 2.0 * kaf * 1.125 + 1.0;
    let n = pings.borrow().len();
    let first_s = pings
        .borrow()
        .first()
        .map(|t| t.duration_since(t0).as_secs_f64());
    let detail = format!(
        "keep alive {ka} s: ended {} (bound {bound:.2} s), {n} PINGREQ seen (first at {first_s:?} s), result {}",
        s(took),
        end.result
    );
    let ping_ok = n == 1 && first_s.is_some_and(|t| (kaf - 0.2..=kaf * 1.125 + 0.5).contains(&t));
    if ping_ok && (2.0 * kaf - 0.1..=bound).contains(&took.as_secs_f64()) {
        Ok(detail)
    } else {
        Err(detail)
    }
}

/// Old keep-alive tasks end: 20 connect/disconnect pairs, count the log records.
pub async fn keepalive_tasks(ver: Ver) -> Result<String, String> {
    let f = Fake::new().await;
    let (l, port) = (f.l, f.port);
    tokio::task::spawn_local(async move {
        loop {
            let Ok((mut st, _)) = l.accept().await else {
                return;
            };
            tokio::task::spawn_local(async move {
                let _ = handshake(&mut st, ver).await;
                while read_pkt(&mut st).await.is_ok() {}
            });
        }
    });
    // Keep-alive tasks of earlier cases end within one interval (2 s). Wait, so the count starts
    // clean.
    sleep(Duration::from_secs(4)).await;
    let base = KA_STOPS.load(Ordering::SeqCst);
    let mut o = Opts::new(ver, Tr::Tcp, "127.0.0.1", port);
    o.keep_alive = 2;
    for _ in 0..20 {
        let c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
        c.sink.close();
        end_of(c.end, Duration::from_secs(3)).await?;
    }
    let t = Instant::now();
    sleep(Duration::from_secs(1)).await;
    let before = KA_STOPS.load(Ordering::SeqCst) - base;
    sleep(Duration::from_secs(3).saturating_sub(t.elapsed())).await;
    let after = KA_STOPS.load(Ordering::SeqCst) - base;
    let d = format!("records before first tick (1 s): {before}, after 3 s: {after}");
    if before == 0 && after == 20 {
        Ok(d)
    } else {
        Err(d)
    }
}

/// 2 MiB declared length with a 1 MiB limit: closes at the fixed header.
pub async fn big_header(ver: Ver) -> Result<String, String> {
    let f = Fake::new().await;
    let (l, port) = (f.l, f.port);
    tokio::task::spawn_local(async move {
        let mut st = l.accept().await.expect("accept").0;
        let _ = handshake(&mut st, ver).await;
        // A valid variable header (topic "t/b", v5 properties 0), so only the size check can
        // close the connection.
        let mut hdr = vec![0x30];
        hdr.extend(fake::varint(2 << 20));
        hdr.extend([0, 3, b't', b'/', b'b']);
        if ver == Ver::V5 {
            hdr.push(0);
        }
        hdr.extend([7u8; 64]);
        let _ = st.write_all(&hdr).await;
        let _ = drain(&mut st, Duration::from_secs(5)).await;
    });
    let o = Opts::new(ver, Tr::Tcp, "127.0.0.1", port);
    let c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
    let rss0 = rss_kb();
    let (end, took) = end_of(c.end, Duration::from_secs(5)).await?;
    let rss1 = rss_kb();
    let d = format!(
        "closed after {}, RSS {rss0} -> {rss1} kB, result {} stop {}",
        s(took),
        end.result,
        end.stop
    );
    if end.stop.contains("MaxSizeExceeded")
        && took < Duration::from_secs(1)
        && rss1.saturating_sub(rss0) < 1024
    {
        Ok(d)
    } else {
        Err(d)
    }
}

/// Black hole: a listener whose accept queue is full, so new SYNs are dropped.
pub async fn blackhole() -> Result<(TcpListener, Vec<TcpStream>, u16), String> {
    let sock = TcpSocket::new_v4().map_err(|e| e.to_string())?;
    sock.bind("127.0.0.1:0".parse().expect("addr"))
        .map_err(|e| e.to_string())?;
    let l = sock.listen(1).map_err(|e| e.to_string())?;
    let port = l.local_addr().map_err(|e| e.to_string())?.port();
    let mut fill = Vec::new();
    for _ in 0..8 {
        match timeout(
            Duration::from_millis(300),
            TcpStream::connect(fake::addr(port)),
        )
        .await
        {
            Ok(Ok(st)) => fill.push(st),
            _ => break,
        }
    }
    match timeout(
        Duration::from_millis(500),
        TcpStream::connect(fake::addr(port)),
    )
    .await
    {
        Err(_) => Ok((l, fill, port)),
        Ok(_) => Err("could not make a black hole (connect still works)".into()),
    }
}

/// Silent CONNACK (v3, v5), stalled TLS and black-hole dial: each fails within the deadline.
pub async fn deadlines() -> Result<String, String> {
    let (bh_l, bh_fill, bh_port) = blackhole().await?;
    let mk = |ver, tr, port| {
        let mut o = Opts::new(ver, tr, "127.0.0.1", port);
        if tr == Tr::Tls {
            o.ssl = Some(tls(false, false, false)?.0);
        }
        Ok::<_, String>(o)
    };
    let silent = || async move {
        let f = Fake::new().await;
        let (l, port) = (f.l, f.port);
        tokio::task::spawn_local(async move {
            let mut st = l.accept().await.expect("accept").0;
            let _ = read_pkt(&mut st).await;
            std::future::pending::<()>().await;
        });
        port
    };
    let (p3, p5, pt) = (silent().await, silent().await, silent().await);
    let run = |name: &'static str, o: Opts| async move {
        let t = Instant::now();
        let r = connect(&o, None).await;
        (name, t.elapsed(), r.err().map(|e| e.text))
    };
    let rs = tokio::join!(
        run("silent CONNACK v3", mk(Ver::V3, Tr::Tcp, p3)?),
        run("silent CONNACK v5", mk(Ver::V5, Tr::Tcp, p5)?),
        run("stalled TLS", mk(Ver::V3, Tr::Tls, pt)?),
        run("black-hole dial", mk(Ver::V3, Tr::Tcp, bh_port)?),
    );
    drop((bh_l, bh_fill));
    // Each case needs its own error and time range. Silent CONNACK and the black hole end at our
    // 10 s deadline. The stalled TLS handshake ends at the 5 s TLS handshake timeout, before it.
    let want: [(&str, f64, f64); 4] = [
        ("Our connect deadline expired", 9.9, 10.6),
        ("Our connect deadline expired", 9.9, 10.6),
        ("TLS Handshake timeout", 4.9, 5.6),
        ("Our connect deadline expired", 9.9, 10.6),
    ];
    let mut ok = true;
    let mut out = Vec::new();
    for ((name, t, e), (text, lo, hi)) in [rs.0, rs.1, rs.2, rs.3].into_iter().zip(want) {
        let e = e.unwrap_or_else(|| "CONNECTED".into());
        let good = e.contains(text) && (lo..=hi).contains(&t.as_secs_f64());
        ok &= good;
        out.push(format!(
            "{name}: {} \"{e}\" (want \"{text}\" in {lo} to {hi} s)",
            s(t)
        ));
    }
    let d = out.join("; ");
    if ok { Ok(d) } else { Err(d) }
}

fn v5_connack_refused() -> Vec<u8> {
    let mut props = vec![0x1F];
    let reason = b"not allowed here";
    props.extend((reason.len() as u16).to_be_bytes());
    props.extend(reason);
    props.push(0x1C);
    let sref = b"other.example:1883";
    props.extend((sref.len() as u16).to_be_bytes());
    props.extend(sref);
    let mut body = vec![0x00, 0x87];
    body.extend(fake::varint(props.len() as u32));
    body.extend(props);
    fake::pkt(0x20, &body)
}

/// A fake that reads CONNECT, writes `reply`, then closes (or only closes when `reply` is empty).
async fn reply_once(reply: Vec<u8>) -> u16 {
    let f = Fake::new().await;
    let (l, port) = (f.l, f.port);
    tokio::task::spawn_local(async move {
        let mut st = l.accept().await.expect("accept").0;
        let _ = read_pkt(&mut st).await;
        if reply.is_empty() {
            return; // closes at once
        }
        let _ = st.write_all(&reply).await;
        let _ = drain(&mut st, Duration::from_secs(2)).await;
    });
    port
}

async fn refused(ver: Ver, reply: Vec<u8>) -> Result<String, String> {
    let port = reply_once(reply).await;
    let o = Opts::new(ver, Tr::Tcp, "127.0.0.1", port);
    let t = Instant::now();
    match connect(&o, None).await {
        Ok(_) => Err("connected, must fail".into()),
        Err(e) => Ok(format!(
            "{}: {}{}",
            s(t.elapsed()),
            e.text,
            e.io_text.map_or(String::new(), |t| format!(" [io: {t}]"))
        )),
    }
}

/// Refused CONNACK: v3 code 5, v5 0x87 with reason string and server reference.
pub async fn refused_connack() -> Result<String, String> {
    let a = refused(Ver::V3, vec![0x20, 2, 0, 5]).await?;
    let b = refused(Ver::V5, v5_connack_refused()).await?;
    let ok = a.contains("NotAuthorized")
        && b.contains("NotAuthorized")
        && b.contains("not allowed here")
        && b.contains("other.example:1883");
    let d = format!("v3 code 5: {a} || v5 0x87: {b}");
    if ok { Ok(d) } else { Err(d) }
}

/// Version errors as raw bytes. Records what ntex returns for each.
pub async fn version_errors() -> Result<String, String> {
    let a = refused(Ver::V5, vec![0x20, 3, 0, 0x84, 0]).await?;
    let b = refused(Ver::V5, vec![0x20, 2, 0, 1]).await?;
    let c = refused(Ver::V3, vec![0x20, 2, 0, 1]).await?;
    let d = refused(Ver::V3, vec![]).await?;
    let e = refused(Ver::V5, vec![]).await?;
    // Exact errors (checked against the real run): v5 0x84 and v3 code 1 are `Ack` values, the
    // 3.1.1 CONNACK to a v5 CONNECT is a decode error, and an early close is `Disconnected(None)`.
    let ok = a.contains("UnsupportedProtocolVersion")
        && b.contains("Protocol(Decode(MalformedPacket))")
        && c.contains("UnacceptableProtocolVersion")
        && d.contains("Disconnected(None)")
        && e.contains("Disconnected(None)");
    let t = format!(
        "v5 CONNACK 0x84: {a} || 3.1.1 CONNACK `20 02 00 01` to a v5 CONNECT: {b} || v3 code 1: {c} || close before CONNACK v3: {d} || v5: {e}"
    );
    if ok { Ok(t) } else { Err(t) }
}

/// Incoming QoS 2 is never delivered. v5: DISCONNECT 0x9B. v3: closes, no PUBREC.
pub async fn incoming_qos2(ver: Ver) -> Result<String, String> {
    let f = Fake::new().await;
    let (l, port) = (f.l, f.port);
    let seen = Rc::new(RefCell::new((Vec::new(), false)));
    let s2 = seen.clone();
    tokio::task::spawn_local(async move {
        let mut st = l.accept().await.expect("accept").0;
        let _ = handshake(&mut st, ver).await;
        let _ = st.write_all(&fake::publish(ver, "q2", 2, 7, b"x")).await;
        *s2.borrow_mut() = drain(&mut st, Duration::from_secs(3)).await;
    });
    let o = Opts::new(ver, Tr::Tcp, "127.0.0.1", port);
    let mut c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
    let (end, took) = end_of(c.end, Duration::from_secs(4)).await?;
    let delivered = c.rx.try_recv().is_ok();
    sleep(Duration::from_millis(100)).await;
    let (bytes, eof) = seen.borrow().clone();
    let hex: String = bytes.iter().map(|b| format!("{b:02x} ")).collect();
    let pubrec = bytes.contains(&0x50);
    let d = format!(
        "closed after {}, delivered={delivered}, bytes from client after PUBLISH: [{}], eof={eof}, result {} stop {}",
        s(took),
        hex.trim(),
        end.result,
        end.stop
    );
    let good = !delivered
        && match ver {
            Ver::V5 => bytes.first() == Some(&0xE0) && bytes.contains(&0x9B),
            Ver::V3 => !pubrec,
        };
    if good { Ok(d) } else { Err(d) }
}

/// The partial bytes a fake sends: half a fixed header, or half a PUBLISH payload.
fn partial(ver: Ver, payload: bool) -> Vec<u8> {
    if payload {
        let mut w = fake::publish(ver, "h/p", 0, 0, &[9u8; 200]);
        w.truncate(30);
        w
    } else {
        vec![0x30]
    }
}

/// A TLS fake (a thread, blocking OpenSSL): handshake, CONNACK, then `part`, then silence.
fn tls_fake(ver: Ver, part: Vec<u8>) -> Result<u16, String> {
    use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};
    use std::io::{Read, Write};
    let certs =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/broker/certs");
    let mut acc =
        SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).map_err(|e| e.to_string())?;
    acc.set_certificate_chain_file(certs.join("server.crt"))
        .map_err(|e| e.to_string())?;
    acc.set_private_key_file(certs.join("server.key"), SslFiletype::PEM)
        .map_err(|e| e.to_string())?;
    let acc = acc.build();
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = l.local_addr().map_err(|e| e.to_string())?.port();
    std::thread::spawn(move || {
        let Ok((sock, _)) = l.accept() else { return };
        let Ok(mut st) = acc.accept(sock) else { return };
        let mut buf = [0u8; 512];
        let _ = st.read(&mut buf); // CONNECT
        let _ = st.write_all(&fake::connack_ok(ver));
        let _ = st.write_all(&part);
        let _ = st.flush();
        std::thread::sleep(Duration::from_secs(90));
    });
    Ok(port)
}

/// A TCP or TLS fake that sends `part` after CONNACK. Returns the options for the client.
async fn partial_fake(
    ver: Ver,
    tls_on: bool,
    part: Vec<u8>,
) -> Result<(Opts, Option<navigator_core::tls::TlsDiag>), String> {
    if tls_on {
        let port = tls_fake(ver, part)?;
        let (ssl, diag) = tls(true, false, true)?;
        let mut o = Opts::new(ver, Tr::Tls, "localhost", port);
        o.ssl = Some(ssl);
        Ok((o, Some(diag)))
    } else {
        let f = Fake::new().await;
        let (l, port) = (f.l, f.port);
        tokio::task::spawn_local(async move {
            let mut st = l.accept().await.expect("accept").0;
            let _ = handshake(&mut st, ver).await;
            let _ = st.write_all(&part).await;
            let _ = drain(&mut st, Duration::from_secs(90)).await;
        });
        Ok((Opts::new(ver, Tr::Tcp, "127.0.0.1", port), None))
    }
}

/// Half a fixed header, or half a PUBLISH payload, with keep alive 0 and a frame read rate (TCP
/// or TLS). The connection must end with `ReadTimeout`, not earlier than the first rate check.
pub async fn half_frame(ver: Ver, payload: bool, tls_on: bool) -> Result<String, String> {
    let rate = if full() { (10, 60, 256) } else { (2, 6, 256) };
    let (mut o, diag) = partial_fake(ver, tls_on, partial(ver, payload)).await?;
    o.keep_alive = 0;
    o.frame_rate = rate;
    let c = connect(&o, diag.as_ref())
        .await
        .map_err(|e| format!("{e:?}"))?;
    let (end, took) = end_of(c.end, Duration::from_secs(90)).await?;
    let d = format!(
        "ended {} (bound {} s{}), result {} stop {}",
        s(took),
        f64::from(rate.0) + 3.0,
        if full() {
            ""
        } else {
            ", SCALED rate 2 s / 6 s / 256"
        },
        end.result,
        end.stop
    );
    let t = took.as_secs_f64();
    if end.stop.contains("ReadTimeout")
        && (f64::from(rate.0) - 0.2..=f64::from(rate.0) + 3.0).contains(&t)
    {
        Ok(d)
    } else {
        Err(format!(
            "{d} (checks: stop ReadTimeout, time in [rate - 0.2, rate + 3]: one period, one timer step, shutdown, rounding)"
        ))
    }
}

/// Disconnect while a half frame waits (TCP, TLS or WS): graceful close, 2 s wait, then force
/// close. The connection must be open when the disconnect starts.
pub async fn disconnect_in_half_frame(ver: Ver, tr: Tr) -> Result<String, String> {
    let (mut o, diag) = match tr {
        Tr::Ws => {
            let port = ws_fake(ver, |mut st| async move {
                let mut h = vec![0x82, 0x7e, 0x01, 0x00];
                h.extend([0u8; 10]);
                let _ = st.write_all(&h).await;
                let _ = drain(&mut st, Duration::from_secs(30)).await;
            })
            .await;
            (Opts::new(ver, Tr::Ws, "127.0.0.1", port), None)
        }
        Tr::Wss => {
            let (port, _) = wss_server(ver)?;
            let (ssl, diag) = tls(true, false, true)?;
            let mut o = Opts::new(ver, Tr::Wss, "localhost", port);
            o.ssl = Some(ssl);
            (o, Some(diag))
        }
        _ => partial_fake(ver, tr == Tr::Tls, vec![0x30]).await?,
    };
    o.keep_alive = 0;
    let c = connect(&o, diag.as_ref())
        .await
        .map_err(|e| format!("{e:?}"))?;
    sleep(Duration::from_millis(300)).await;
    if !c.sink.is_open() {
        return Err("the connection was already closed before the disconnect".into());
    }
    let t = Instant::now();
    let how = disconnect_prompt(c).await?;
    let took = t.elapsed();
    let d = format!("{tr:?}: disconnected in {} ({how})", s(took));
    if took.as_secs_f64() <= 3.0 {
        Ok(d)
    } else {
        Err(d)
    }
}

/// Graceful DISCONNECT, wait up to 2 s, then `force_close()`.
async fn disconnect_prompt(c: Conn) -> Result<&'static str, String> {
    c.sink.close();
    let mut end = c.end;
    match timeout(Duration::from_secs(2), &mut end).await {
        Ok(Ok(_)) => Ok("graceful"),
        Ok(Err(e)) => Err(format!("the attempt task failed: {e}")),
        Err(_) => {
            c.sink.force_close();
            timeout(Duration::from_secs(2), end)
                .await
                .map_err(|_| "force_close did not end the connection in 2 s")?
                .map_err(|e| e.to_string())?;
            Ok("graceful wait expired, force_close ended it")
        }
    }
}

/// Disconnect during a stalled send: <= 3 s. One run uses the graceful close (2 s wait, then
/// `force_close()`), one run uses `force_close()` at once.
pub async fn disconnect_stalled_send(ver: Ver) -> Result<String, String> {
    let mut out = Vec::new();
    let mut ok = true;
    for force in [false, true] {
        let port = deaf_fake(ver).await;
        let mut o = Opts::new(ver, Tr::Tcp, "127.0.0.1", port);
        o.keep_alive = 0;
        let c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
        let fl = flood(&c, Duration::from_secs(30));
        sleep(Duration::from_millis(1500)).await;
        let pending_at_start = fl.ended.borrow().is_none();
        let t = Instant::now();
        let how = if force {
            c.sink.force_close();
            timeout(Duration::from_secs(2), c.end)
                .await
                .map_err(|_| "force_close did not end the connection in 2 s")?
                .map_err(|e| e.to_string())?;
            "force_close"
        } else {
            disconnect_prompt(c).await?
        };
        let took = t.elapsed();
        sleep(Duration::from_millis(100)).await;
        let ended = fl.ended.borrow().clone();
        let pend = ended.clone().map_or("pending".into(), |(_, e)| e);
        let ended_after = ended
            .as_ref()
            .map_or("never".into(), |(at, _)| s(at.duration_since(t)));
        // The send must have been pending at the disconnect and must end with `Disconnected`.
        ok &= took.as_secs_f64() <= 3.0
            && pending_at_start
            && ended.is_some_and(|(_, e)| e.contains("Disconnected"));
        out.push(format!(
            "{how}: attempt ended in {}; the pending send ended {pend} {} after the call",
            s(took),
            ended_after
        ));
    }
    let d = out.join("; ");
    if ok { Ok(d) } else { Err(d) }
}

/// A WebSocket fake: handshake, CONNACK in a binary frame, then `script`.
async fn ws_fake<F, Fut>(ver: Ver, script: F) -> u16
where
    F: FnOnce(TcpStream) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let f = Fake::new().await;
    let (l, port) = (f.l, f.port);
    tokio::task::spawn_local(async move {
        let mut st = l.accept().await.expect("accept").0;
        if fake::ws_accept(&mut st).await.is_err() {
            return;
        }
        let Ok((_, _)) = fake::ws_read(&mut st).await else {
            return;
        };
        let _ = st
            .write_all(&fake::ws_frame(true, 2, &fake::connack_ok(ver)))
            .await;
        script(st).await;
    });
    port
}

/// Client frames seen by a WS fake: (payload, time).
type Seen = std::sync::Arc<Mutex<Vec<(Vec<u8>, Instant)>>>;

/// Checks of the stalled-frame case over WS and WSS. Keep alive 2 s: the broker sees exactly one
/// PINGREQ at about 2 s (the PINGRESP is behind the stalled frame), keep alive closes at about
/// 4 s, the WebSocket filter waits for its 1 s shutdown, and the attempt ends in
/// 2 x ka x 1.125 + 2 s.
async fn ws_half_check(label: &str, c: Conn, seen: Seen, t0: Instant) -> Result<String, String> {
    let (end, took) = end_of(c.end, Duration::from_secs(20)).await?;
    let pings: Vec<f64> = seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(p, _)| p == &[0xC0, 0])
        .map(|(_, t)| t.duration_since(t0).as_secs_f64())
        .collect();
    let notice = end.stop_at.map(|t| t.duration_since(t0).as_secs_f64());
    let d = format!(
        "{label}: ended {} (bound 6.5 s), PINGREQ seen at {pings:?} s, Stop notice at {notice:?} s, stop {}",
        s(took),
        end.stop
    );
    let t = took.as_secs_f64();
    let good = pings.len() == 1
        && (1.8..=2.8).contains(&pings[0])
        && (3.9..=6.5).contains(&t)
        && end.stop.contains("filter shutdown timed out");
    if good { Ok(d) } else { Err(d) }
}

/// WS: half a binary frame, keep alive 2 s.
pub async fn ws_half_frame(ver: Ver) -> Result<String, String> {
    let seen: Seen = Default::default();
    let s2 = seen.clone();
    let port = ws_fake(ver, move |mut st| async move {
        let mut h = vec![0x82, 0x7e, 0x01, 0x00];
        h.extend([0u8; 10]);
        let _ = st.write_all(&h).await;
        while let Ok((_, p)) = fake::ws_read(&mut st).await {
            s2.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((p, Instant::now()));
        }
    })
    .await;
    let mut o = Opts::new(ver, Tr::Ws, "127.0.0.1", port);
    o.keep_alive = 2;
    let c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
    let t0 = Instant::now();
    ws_half_check("WS", c, seen, t0).await
}

/// A WSS fake (a thread: TLS, WebSocket upgrade, CONNACK, half a binary frame, then it records
/// the frames of the client). Returns its port and the frames.
fn wss_server(ver: Ver) -> Result<(u16, Seen), String> {
    use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};
    use std::io::{Read, Write};
    let certs =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/broker/certs");
    let mut acc =
        SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).map_err(|e| e.to_string())?;
    acc.set_certificate_chain_file(certs.join("server.crt"))
        .map_err(|e| e.to_string())?;
    acc.set_private_key_file(certs.join("server.key"), SslFiletype::PEM)
        .map_err(|e| e.to_string())?;
    let acc = acc.build();
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = l.local_addr().map_err(|e| e.to_string())?.port();
    let seen: Seen = Default::default();
    let s2 = seen.clone();
    std::thread::spawn(move || {
        let Ok((sock, _)) = l.accept() else { return };
        let Ok(mut st) = acc.accept(sock) else { return };
        let mut req = Vec::new();
        let mut b = [0u8; 1];
        while !req.ends_with(b"\r\n\r\n") {
            if st.read_exact(&mut b).is_err() {
                return;
            }
            req.push(b[0]);
        }
        let Ok(resp) = fake::ws_accept_response(&String::from_utf8_lossy(&req)) else {
            return;
        };
        let _ = st.write_all(resp.as_bytes());
        let read_frame =
            |st: &mut openssl::ssl::SslStream<std::net::TcpStream>| -> Option<Vec<u8>> {
                let mut h = [0u8; 2];
                st.read_exact(&mut h).ok()?;
                let mut len = usize::from(h[1] & 0x7f);
                if len == 126 {
                    let mut x = [0u8; 2];
                    st.read_exact(&mut x).ok()?;
                    len = usize::from(u16::from_be_bytes(x));
                }
                let mut mask = [0u8; 4];
                st.read_exact(&mut mask).ok()?;
                let mut p = vec![0u8; len];
                st.read_exact(&mut p).ok()?;
                for (i, x) in p.iter_mut().enumerate() {
                    *x ^= mask[i % 4];
                }
                Some(p)
            };
        if read_frame(&mut st).is_none() {
            return; // CONNECT
        }
        let _ = st.write_all(&fake::ws_frame(true, 2, &fake::connack_ok(ver)));
        let mut half = vec![0x82, 0x7e, 0x01, 0x00];
        half.extend([0u8; 10]);
        let _ = st.write_all(&half);
        let _ = st.flush();
        while let Some(p) = read_frame(&mut st) {
            s2.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((p, Instant::now()));
        }
    });
    Ok((port, seen))
}

/// WSS: the same stalled frame.
pub async fn wss_half_frame(ver: Ver) -> Result<String, String> {
    let (port, seen) = wss_server(ver)?;
    let (ssl, diag) = tls(true, false, true)?;
    let mut o = Opts::new(ver, Tr::Wss, "localhost", port);
    o.ssl = Some(ssl);
    o.keep_alive = 2;
    let c = connect(&o, Some(&diag))
        .await
        .map_err(|e| format!("{e:?}"))?;
    let t0 = Instant::now();
    ws_half_check("WSS", c, seen, t0).await
}

/// WS: fragmented packet, Ping (answered with a Pong that has the same payload), an unsolicited
/// Pong (ignored), and a Text frame (closes the connection).
pub async fn ws_frames(ver: Ver) -> Result<String, String> {
    let frames = Rc::new(RefCell::new(Vec::<(u8, Vec<u8>)>::new()));
    let f2 = frames.clone();
    let port = ws_fake(ver, move |mut st| async move {
        let pk = fake::publish(ver, "ws/frag", 0, 0, b"fragmented-payload");
        let (a, b, c) = (&pk[..4], &pk[4..9], &pk[9..]);
        for fr in [
            fake::ws_frame(false, 2, a),
            fake::ws_frame(false, 0, b),
            fake::ws_frame(true, 0, c),
            fake::ws_frame(true, 9, b"ping"),
            fake::ws_frame(true, 10, b"unsolicited"),
        ] {
            let _ = st.write_all(&fr).await;
        }
        sleep(Duration::from_millis(500)).await;
        let _ = st.write_all(&fake::ws_frame(true, 1, b"text")).await;
        while let Ok(fr) = fake::ws_read(&mut st).await {
            f2.borrow_mut().push(fr);
        }
    })
    .await;
    let o = Opts::new(ver, Tr::Ws, "127.0.0.1", port);
    let mut c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
    let frag = crate::cases::recv(&mut c, "ws/frag", b"fragmented-payload").await;
    sleep(Duration::from_millis(300)).await;
    let open_before_text = c.sink.is_open();
    let ended = end_of(c.end, Duration::from_secs(3)).await;
    let seen = frames.borrow().clone();
    let pongs: Vec<_> = seen.iter().filter(|(op, _)| *op == 10).collect();
    let d = format!(
        "fragmented packet: {}; Pong frames from the client: {:?}; unsolicited Pong ignored: {}; Text frame: {}",
        frag.as_ref()
            .map_or_else(Clone::clone, |_| "delivered".into()),
        pongs,
        pongs.len() == 1,
        ended.as_ref().map_or_else(Clone::clone, |(e, t)| format!(
            "closed after {} with {}",
            s(*t),
            e.stop
        ))
    );
    let good = frag.is_ok()
        && open_before_text
        && pongs.len() == 1
        && pongs[0].1 == b"ping"
        && ended.is_ok_and(|(e, _)| e.stop.contains("Text frames are not supported"));
    if good { Ok(d) } else { Err(d) }
}

/// First resolved address refuses, the second works.
pub async fn first_address_refuses() -> Result<String, String> {
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        l.local_addr().map_err(|e| e.to_string())?.port()
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let c2 = calls.clone();
    let mut o = Opts::new(Ver::V3, Tr::Tcp, "broker.test", 18830);
    o.resolver = Some(Arc::new(move |_, _| {
        c2.fetch_add(1, Ordering::SeqCst);
        Ok(vec![fake::addr(dead), fake::addr(18830)])
    }));
    let t = Instant::now();
    let c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
    let took = t.elapsed();
    crate::cases::bye(c).await?;
    Ok(format!(
        "refused on :{dead}, connected on :18830 in {}; resolver calls {}",
        s(took),
        calls.load(Ordering::SeqCst)
    ))
}

/// Stalled DNS: 10 cancelled attempts keep one lookup running.
pub async fn stalled_dns() -> Result<String, String> {
    let started = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (st, ac, pk, gt) = (started.clone(), active.clone(), peak.clone(), gate.clone());
    let resolver: crate::dialer::Resolver = Arc::new(move |_, _| {
        st.fetch_add(1, Ordering::SeqCst);
        let now = ac.fetch_add(1, Ordering::SeqCst) + 1;
        pk.fetch_max(now, Ordering::SeqCst);
        let (m, cv) = &*gt;
        let mut open = m.lock().unwrap_or_else(|e| e.into_inner());
        while !*open {
            open = cv.wait(open).unwrap_or_else(|e| e.into_inner());
        }
        ac.fetch_sub(1, Ordering::SeqCst);
        Ok(vec![fake::addr(18830)])
    });
    let mut o = Opts::new(Ver::V3, Tr::Tcp, "stalled.test", 18830);
    o.resolver = Some(resolver);
    o.connect_timeout = Duration::from_millis(300);
    let t = Instant::now();
    for i in 0..10 {
        let t = Instant::now();
        match connect(&o, None).await {
            Err(e) if e.text.contains("Our connect deadline expired") => {
                let took = t.elapsed().as_secs_f64();
                if !(0.25..=0.6).contains(&took) {
                    return Err(format!("attempt {i}: the deadline fired after {took:.2} s"));
                }
            }
            other => {
                return Err(format!(
                    "attempt {i} must end with our connect deadline, got {:?}",
                    other.map(|_| "a connection").map_err(|e| e.text)
                ));
            }
        }
    }
    let during = (
        started.load(Ordering::SeqCst),
        active.load(Ordering::SeqCst),
        peak.load(Ordering::SeqCst),
    );
    let took = t.elapsed();
    {
        let (m, cv) = &*gate;
        *m.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }
    sleep(Duration::from_millis(200)).await;
    let after_active = active.load(Ordering::SeqCst);
    o.connect_timeout = Duration::from_secs(5);
    let next = connect(&o, None)
        .await
        .map(|_| "connected")
        .map_err(|e| format!("{e:?}"));
    let d = format!(
        "10 attempts in {}: lookups started {}, running {}, peak {}; after release running {after_active}; next attempt: {next:?}",
        s(took),
        during.0,
        during.1,
        during.2
    );
    if during == (1, 1, 1) && after_active == 0 && next.is_ok() {
        Ok(d)
    } else {
        Err(d)
    }
}

/// A black-hole dial that is cancelled leaves no extra file descriptor.
pub async fn blackhole_fd() -> Result<String, String> {
    let (_l, _fill, port) = blackhole().await?;
    let (ssl, _) = tls(false, false, false)?;
    let mut out = String::new();
    let mut ok = true;
    for tr in [Tr::Tcp, Tr::Tls, Tr::Ws] {
        let before = fd_count();
        for _ in 0..5 {
            let mut o = Opts::new(Ver::V3, tr, "127.0.0.1", port);
            o.ssl = Some(ssl.clone());
            o.connect_timeout = Duration::from_millis(300);
            let t = Instant::now();
            match connect(&o, None).await {
                Err(e) if e.text.contains("Our connect deadline expired") => {
                    let took = t.elapsed().as_secs_f64();
                    if !(0.25..=0.6).contains(&took) {
                        return Err(format!("{tr:?}: the deadline fired after {took:.2} s"));
                    }
                }
                other => {
                    return Err(format!(
                        "{tr:?}: every attempt must end with our connect deadline, got {:?}",
                        other.map(|_| "a connection").map_err(|e| e.text)
                    ));
                }
            }
        }
        sleep(Duration::from_millis(200)).await;
        let after = fd_count();
        ok &= after == before;
        out += &format!(
            "{tr:?}: 5 attempts ended with our deadline at 0.25 to 0.6 s, fds {before} -> {after}; "
        );
    }
    if ok { Ok(out) } else { Err(out) }
}

/// The first handler call is held pending. The second call (detached) panics after an await.
/// The panic is contained, the first call never completes, and the attempt ends.
pub async fn handler_panic(ver: Ver) -> Result<String, String> {
    let f = Fake::new().await;
    let (l, port) = (f.l, f.port);
    tokio::task::spawn_local(async move {
        let mut st = l.accept().await.expect("accept").0;
        let _ = handshake(&mut st, ver).await;
        let mut w = fake::publish(ver, "pa/1", 1, 1, b"one");
        w.extend(fake::publish(ver, "pa/2", 1, 2, b"two"));
        let _ = st.write_all(&w).await;
        let _ = drain(&mut st, Duration::from_secs(5)).await;
    });
    PROBE.with(|p| {
        p.started.set(0);
        p.completed.set(0);
        p.panicked.set(0);
        p.on.set(true);
    });
    let o = Opts::new(ver, Tr::Tcp, "127.0.0.1", port);
    let r = async {
        let c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
        end_of(c.end, Duration::from_secs(5)).await
    }
    .await;
    let (started, completed, panicked) = PROBE.with(|p| {
        p.on.set(false);
        (p.started.get(), p.completed.get(), p.panicked.get())
    });
    let (end, took) = r?;
    let d = format!(
        "attempt ended {}; handler calls started {started}, completed {completed}, panicked {panicked}; stop {}",
        s(took),
        end.stop
    );
    if started == 2 && completed == 0 && panicked == 1 && end.stop.contains("handler panic") {
        Ok(d)
    } else {
        Err(format!(
            "{d} (want: 2 started, first held, second panicked, none completed)"
        ))
    }
}

/// v5: the broker DISCONNECT reason and reason string are kept before the close completes.
pub async fn broker_disconnect() -> Result<String, String> {
    let f = Fake::new().await;
    let (l, port) = (f.l, f.port);
    tokio::task::spawn_local(async move {
        let mut st = l.accept().await.expect("accept").0;
        let _ = handshake(&mut st, Ver::V5).await;
        let text = b"maintenance";
        let mut props = vec![0x1F];
        props.extend((text.len() as u16).to_be_bytes());
        props.extend(text);
        let mut body = vec![0x8B];
        body.extend(fake::varint(props.len() as u32));
        body.extend(props);
        let _ = st.write_all(&fake::pkt(0xE0, &body)).await;
        let _ = drain(&mut st, Duration::from_secs(2)).await;
    });
    let o = Opts::new(Ver::V5, Tr::Tcp, "127.0.0.1", port);
    let c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
    let (end, took) = end_of(c.end, Duration::from_secs(3)).await?;
    let d = format!(
        "ended {}: broker DISCONNECT {:?}; stop {}",
        s(took),
        end.broker_disconnect,
        end.stop
    );
    let kept = end.broker_disconnect.as_deref().unwrap_or("");
    if kept.contains("ServerShuttingDown") && kept.contains("maintenance") {
        Ok(d)
    } else {
        Err(d)
    }
}

/// v5 Server Keep Alive in CONNACK overrides the profile. Server 5 s with profile 60 s: the first
/// PINGREQ comes at 5 s. Server 0 with profile 2 s: no PINGREQ and the connection stays open
/// beyond the profile ping and response deadlines (2 x 2 x 1.125 + 1 = 5.5 s).
pub async fn server_keep_alive(server_ka: u16) -> Result<String, String> {
    let f = Fake::new().await;
    let (l, port) = (f.l, f.port);
    let first = Rc::new(RefCell::new(None::<Instant>));
    let f2 = first.clone();
    tokio::task::spawn_local(async move {
        let mut st = l.accept().await.expect("accept").0;
        let _ = read_pkt(&mut st).await;
        let mut b = vec![0x00, 0x00, 0x03, 0x13];
        b.extend(server_ka.to_be_bytes());
        let _ = st.write_all(&fake::pkt(0x20, &b)).await;
        while let Ok((h, _)) = read_pkt(&mut st).await {
            if h == 0xC0 && f2.borrow().is_none() {
                *f2.borrow_mut() = Some(Instant::now());
            }
        }
    });
    let mut o = Opts::new(Ver::V5, Tr::Tcp, "127.0.0.1", port);
    o.keep_alive = if server_ka == 0 { 2 } else { 60 };
    let c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
    let t0 = Instant::now();
    sleep(Duration::from_secs(7)).await;
    let seen = first.borrow().map(|t| t.duration_since(t0).as_secs_f64());
    let open = c.sink.is_open();
    let d = format!(
        "Server Keep Alive {server_ka} s, profile {} s: first PINGREQ at {seen:?} s, connection open {open}",
        o.keep_alive
    );
    let good = match (server_ka, seen) {
        (0, None) => open,
        (5, Some(t)) => (5.0..=6.2).contains(&t) && open,
        _ => false,
    };
    c.sink.close();
    if good { Ok(d) } else { Err(d) }
}

/// Keep alive 60 s and 70 s, with and without our publishes, v3 and v5 (70 s is in the 8 s timer
/// step range). The eight runs share the wall clock. Each passes the checks of `ping_unanswered`.
pub async fn long_pings() -> Result<String, String> {
    let all = tokio::join!(
        ping_unanswered(Ver::V3, false, 60),
        ping_unanswered(Ver::V5, false, 60),
        ping_unanswered(Ver::V3, true, 60),
        ping_unanswered(Ver::V5, true, 60),
        ping_unanswered(Ver::V3, false, 70),
        ping_unanswered(Ver::V5, false, 70),
        ping_unanswered(Ver::V3, true, 70),
        ping_unanswered(Ver::V5, true, 70),
    );
    let all = [all.0, all.1, all.2, all.3, all.4, all.5, all.6, all.7];
    let ok = all.iter().all(Result::is_ok);
    let text = all
        .into_iter()
        .map(|r| r.unwrap_or_else(|e| format!("FAIL: {e}")))
        .collect::<Vec<_>>()
        .join(" || ");
    if ok { Ok(text) } else { Err(text) }
}

/// Trickling payload: a valid PUBLISH header declares a payload that never completes. The peer
/// sends more than the rate (256 bytes) in each period, so only the cumulative cap ends it.
/// Source (`ntex-mqtt` `io/mod.rs` `handle_timeout`): each rate check takes one period from the
/// budget and starts a new timer. There are cap / period checks. Each timer fires after at least
/// one and less than one period + 1 s. Add 1 s of I/O shutdown and 1 s of rounding.
pub async fn trickle(ver: Ver) -> Result<String, String> {
    let (rate, cap, period) = if full() {
        ((10u16, 60u16, 256u32), 60.0, 10.0)
    } else {
        ((2, 6, 256), 6.0, 2.0)
    };
    let f = Fake::new().await;
    let (l, port) = (f.l, f.port);
    tokio::task::spawn_local(async move {
        let mut st = l.accept().await.expect("accept").0;
        let _ = handshake(&mut st, ver).await;
        let mut w = fake::publish(ver, "h/t", 0, 0, &vec![5u8; 512 * 1024]);
        w.truncate(30);
        let _ = st.write_all(&w).await;
        let every = Duration::from_secs_f64(period / 2.0);
        loop {
            sleep(every).await;
            if st.write_all(&[5u8; 300]).await.is_err() {
                return;
            }
        }
    });
    let mut o = Opts::new(ver, Tr::Tcp, "127.0.0.1", port);
    o.keep_alive = 0;
    o.frame_rate = rate;
    let c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
    let (end, took) = end_of(c.end, Duration::from_secs(120)).await?;
    let bound = (cap / period) * (period + 1.0) + 2.0;
    let t = took.as_secs_f64();
    let d = format!(
        "ended {} (cap {cap} s, bound {bound} s{}), stop {}",
        s(took),
        if full() {
            ""
        } else {
            ", SCALED rate 2 s / 6 s / 256"
        },
        end.stop
    );
    if end.stop.contains("ReadTimeout") && (cap - 0.2..=bound).contains(&t) {
        Ok(d)
    } else {
        Err(d)
    }
}

/// A fake accepts the TCP connection and never answers the HTTP upgrade. The WebSocket handshake
/// timeout (ntex default, 5 s) must end it before our 10 s deadline, with its own error text.
pub async fn stalled_ws_upgrade(ver: Ver) -> Result<String, String> {
    let f = Fake::new().await;
    let (l, port) = (f.l, f.port);
    tokio::task::spawn_local(async move {
        let mut st = l.accept().await.expect("accept").0;
        let _ = drain(&mut st, Duration::from_secs(30)).await;
    });
    let o = Opts::new(ver, Tr::Ws, "127.0.0.1", port);
    let t = Instant::now();
    let r = connect(&o, None).await;
    let took = t.elapsed().as_secs_f64();
    let text = r.err().map_or("CONNECTED".into(), |e| e.text);
    let d = format!("failed after {took:.2} s: {text}");
    if !text.contains("Our connect deadline expired")
        && text.contains("Timeout")
        && (4.9..=5.6).contains(&took)
    {
        Ok(d)
    } else {
        Err(d)
    }
}
