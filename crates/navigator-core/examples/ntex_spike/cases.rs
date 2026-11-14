//! Cases a, b, c: the Docker Mosquitto matrix, TLS failure texts and packet sizes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use navigator_core::tls::{TlsDiag, TlsMaterial};
use openssl::ssl::SslConnector;
use tokio::time::timeout;

use crate::fake::{self, Fake};
use crate::wire::{Conn, Opts, Tr, Ver, connect, v3_max_remaining};

pub const CLIENT_CERT_TEXT: &str =
    "The broker needs a client certificate. Add your certificate and key.";

fn certs() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/broker/certs")
}

/// Our own `TlsMaterial` set-up, as the app uses it.
pub fn tls(ca: bool, client: bool, verify: bool) -> Result<(SslConnector, TlsDiag), String> {
    let d = certs();
    let (ca_p, cert, key) = (d.join("ca.crt"), d.join("client.crt"), d.join("client.key"));
    TlsMaterial::load(
        ca.then_some(ca_p.as_path()),
        client.then_some((cert.as_path(), key.as_path())),
        None,
        verify,
    )?
    .connector()
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

pub async fn recv(c: &mut Conn, topic: &str, payload: &[u8]) -> Result<u8, String> {
    match timeout(Duration::from_secs(5), c.rx.recv()).await {
        Ok(Some(m)) if m.topic == topic && m.payload == payload => Ok(m.qos),
        Ok(Some(m)) => Err(format!(
            "wrong message: topic {} len {} (want {} len {})",
            m.topic,
            m.payload.len(),
            topic,
            payload.len()
        )),
        Ok(None) => Err("incoming channel closed".into()),
        Err(_) => Err("no message in 5 s".into()),
    }
}

pub async fn bye(c: Conn) -> Result<String, String> {
    c.sink.close();
    match timeout(Duration::from_secs(3), c.end).await {
        Ok(Ok(e)) => Ok(e.result),
        Ok(Err(e)) => Err(format!("end task failed: {e}")),
        Err(_) => Err("end did not finish in 3 s".into()),
    }
}

/// Case a: connect, subscribe, publish QoS 0 and 1, receive both, disconnect.
pub async fn matrix_one(
    ver: Ver,
    tr: Tr,
    port: u16,
    host: &str,
    ca: bool,
    client: bool,
    user: Option<(&str, &str)>,
) -> Result<String, String> {
    let mut o = Opts::new(ver, tr, host, port);
    let mut diag = None;
    if matches!(tr, Tr::Tls | Tr::Wss) {
        let (ssl, d) = tls(ca, client, true)?;
        o.ssl = Some(ssl);
        diag = Some(d);
    }
    o.username = user.map(|(u, p)| (u.to_owned(), p.to_owned()));
    let t = Instant::now();
    let mut c = connect(&o, diag.as_ref())
        .await
        .map_err(|e| format!("connect: {e:?}"))?;
    let t_conn = t.elapsed();
    let c_ack: String = c.ack.chars().take(60).collect();
    let topic = format!("spike/{}/{ver:?}/{tr:?}/{port}", std::process::id());
    let sub = c.sink.subscribe(&format!("{topic}/#"), 1).await?;
    if !(sub.contains("Success") || sub.contains("Granted")) {
        return Err(format!("subscribe refused: {sub}"));
    }
    let t0 = Instant::now();
    let a0 = c
        .sink
        .publish(&format!("{topic}/0"), b"qos0".to_vec(), 0)
        .await?;
    recv(&mut c, &format!("{topic}/0"), b"qos0").await?;
    let t_q0 = t0.elapsed();
    let t1 = Instant::now();
    let a1 = c
        .sink
        .publish(&format!("{topic}/1"), b"qos1".to_vec(), 1)
        .await?;
    let q = recv(&mut c, &format!("{topic}/1"), b"qos1").await?;
    let t_q1 = t1.elapsed();
    bye(c).await?;
    Ok(format!(
        "connect {} (CONNACK {}), qos0 {a0} rtt {}, qos1 {a1} rtt {} (rx qos {q})",
        ms(t_conn),
        c_ack,
        ms(t_q0),
        ms(t_q1)
    ))
}

// ---- b: TLS failures ----------------------------------------------------------------

/// The text the app shows today for an I/O error without a recorded certificate failure. This
/// mirrors `io_outcome` in `net.rs` for the text branches that matter here.
pub fn today_text(raw: &str, diag: Option<String>, has_cert: bool) -> String {
    if let Some(t) = diag {
        return t;
    }
    let lower = raw.to_lowercase();
    if lower.contains("unexpected eof")
        || lower.contains("connection reset")
        || lower.contains("broken pipe")
    {
        "The broker closed the connection.".into()
    } else if lower.contains("certificate required")
        || (lower.contains("alert") && lower.contains("bad certificate"))
        || (lower.contains("handshake failure") && !has_cert)
    {
        CLIENT_CERT_TEXT.into()
    } else if lower.contains("certificate") {
        format!(
            "TLS: the server certificate is not valid ({raw}). Add its CA file, or turn off verification."
        )
    } else {
        format!("Network error: {raw}")
    }
}

pub async fn tls_fail(ver: Ver, port: u16, ca: bool, expect: &str) -> Result<String, String> {
    let (ssl, diag) = tls(ca, false, true)?;
    let mut o = Opts::new(ver, Tr::Tls, "localhost", port);
    o.ssl = Some(ssl);
    let t = Instant::now();
    match connect(&o, Some(&diag)).await {
        Ok(_) => Err("connect worked, it must fail".into()),
        Err(e) => {
            let text = today_text(
                e.io_text.as_deref().unwrap_or(&e.text),
                e.diag.clone(),
                false,
            );
            let took = ms(t.elapsed());
            if text == expect {
                Ok(format!("{took}: \"{text}\""))
            } else {
                Err(format!(
                    "{took}: got \"{text}\" (raw {}), want \"{expect}\"",
                    e.text
                ))
            }
        }
    }
}

/// mTLS without a client certificate. TLS 1.3 reports the refusal after the handshake, so the
/// error can come from CONNACK wait. Records the raw error and our mapped text.
pub async fn mtls_refused(ver: Ver, tr: Tr, port: u16) -> Result<String, String> {
    let (ssl, diag) = tls(true, false, true)?;
    let mut o = Opts::new(ver, tr, "localhost", port);
    o.ssl = Some(ssl);
    let t = Instant::now();
    match connect(&o, Some(&diag)).await {
        Ok(_) => Err("connect worked, it must fail".into()),
        Err(e) => {
            let text = today_text(
                e.io_text.as_deref().unwrap_or(&e.text),
                e.diag.clone(),
                false,
            );
            let took = ms(t.elapsed());
            if text == CLIENT_CERT_TEXT {
                Ok(format!(
                    "{took}: io error text \"{}\"",
                    e.io_text.as_deref().unwrap_or(&e.text)
                ))
            } else {
                Err(format!(
                    "{took}: error \"{}\" maps to \"{text}\"",
                    e.io_text.as_deref().unwrap_or(&e.text)
                ))
            }
        }
    }
}

/// Local material errors keep their texts. No network.
pub fn local_key_texts() -> Result<String, String> {
    let d = certs();
    let (cert, key) = (d.join("client.crt"), d.join("client.key"));
    let tmp = std::env::temp_dir().join(format!("spike-enc-{}.key", std::process::id()));
    let ok = std::process::Command::new("openssl")
        .args([
            "pkcs8",
            "-topk8",
            "-v2",
            "aes256",
            "-passout",
            "pass:secret",
            "-in",
        ])
        .arg(&key)
        .arg("-out")
        .arg(&tmp)
        .status()
        .map_err(|e| e.to_string())?
        .success();
    if !ok {
        return Err("openssl command failed".into());
    }
    let enc = TlsMaterial::load(None, Some((&cert, &tmp)), None, true)
        .err()
        .ok_or("encrypted key loaded without passphrase")?;
    let wrong = TlsMaterial::load(None, Some((&cert, &tmp)), Some("nope"), true)
        .err()
        .ok_or("wrong passphrase loaded")?;
    let right = TlsMaterial::load(None, Some((&cert, &tmp)), Some("secret"), true).is_ok();
    let _ = std::fs::remove_file(&tmp);
    let mismatch = TlsMaterial::load(
        None,
        Some((&cert, &d.join("client-mismatch.key"))),
        None,
        true,
    )
    .err()
    .ok_or("mismatch loaded")?;
    let want_enc =
        "The client key is encrypted. Enter the key passphrase, or use a key without a passphrase.";
    let want_wrong = "The client key passphrase is wrong, or the key is not valid.";
    let want_mm = "The client certificate and key do not match.";
    if enc == want_enc && wrong == want_wrong && right && mismatch == want_mm {
        Ok("encrypted, wrong passphrase, right passphrase, mismatch: texts unchanged".into())
    } else {
        Err(format!("{enc:?} / {wrong:?} / {right} / {mismatch:?}"))
    }
}

// ---- c: sizes ------------------------------------------------------------------------

/// Payload length that makes the whole PUBLISH packet exactly `total` bytes.
fn payload_for(ver: Ver, topic: &str, qos: u8, total: usize) -> usize {
    let r_fixed = 2 + topic.len() + if qos > 0 { 2 } else { 0 } + usize::from(ver == Ver::V5);
    // 1 byte header + 3 byte varint for 1 MiB sized packets.
    total - 1 - 3 - r_fixed
}

/// Exactly `max_packet` passes both ways (a broker round trip), QoS 0 and 1.
pub async fn size_exact(ver: Ver, tr: Tr, port: u16) -> Result<String, String> {
    let max = 1usize << 20;
    let mut o = Opts::new(ver, tr, "localhost", port);
    let mut diag = None;
    if tr == Tr::Wss {
        let (ssl, d) = tls(true, false, true)?;
        o.ssl = Some(ssl);
        diag = Some(d);
    }
    let mut c = connect(&o, diag.as_ref())
        .await
        .map_err(|e| format!("{e:?}"))?;
    let topic = format!("sz/{}/{ver:?}", std::process::id());
    c.sink.subscribe(&topic, 1).await?;
    let mut out = String::new();
    for qos in [0u8, 1] {
        let body = vec![b'x'; payload_for(ver, &topic, qos, max)];
        let t = Instant::now();
        c.sink.publish(&topic, body.clone(), qos).await?;
        recv(&mut c, &topic, &body).await?;
        out += &format!("exact 1 MiB QoS{qos} out+in {}; ", ms(t.elapsed()));
    }
    bye(c).await?;
    Ok(out)
}

/// One byte more than `max_packet`, outgoing, with a broker that announces no limit.
/// v3 refuses it in the library with `OverMaxPacketSize` (v3 applies `max_size` to outgoing
/// packets). v5 limits outgoing only by the broker's announced Maximum Packet Size, and
/// Mosquitto announces none: the library must send the packet (today's rule).
pub async fn size_plus_one_out(ver: Ver) -> Result<String, String> {
    let max = 1usize << 20;
    let o = Opts::new(ver, Tr::Tcp, "localhost", 18830);
    let c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
    let topic = format!("sz1/{}/{ver:?}", std::process::id());
    c.sink.subscribe(&topic, 1).await?;
    let body = vec![b'x'; payload_for(ver, &topic, 0, max) + 1];
    let r = c.sink.publish(&topic, body, 0).await;
    match (ver, r) {
        (Ver::V3, Err(e)) if e.contains("OverMaxPacketSize") => {
            bye(c).await?;
            Ok(format!("refused before the send: {e}"))
        }
        (Ver::V3, other) => Err(format!(
            "v3 must fail with OverMaxPacketSize, got {other:?}"
        )),
        (Ver::V5, Ok(a)) => {
            // The send returned Ok ("{a}"), which proves only that the packet was encoded and
            // written. The broker's echo exceeds our incoming limit, so the task must end
            // cleanly with `MaxSizeExceeded`.
            let end = timeout(Duration::from_secs(3), c.end)
                .await
                .map_err(|_| "the connection stayed open after the +1 byte publish".to_string())?
                .map_err(|e| format!("end task failed: {e}"))?;
            let d = format!(
                "v5 publish returned {a:?} (no announced limit); the connection ended cleanly with stop {}",
                end.stop
            );
            if end.result == "Ok(())" && end.stop.contains("MaxSizeExceeded") {
                Ok(d)
            } else {
                Err(d)
            }
        }
        (Ver::V5, Err(e)) => Err(format!(
            "v5 must send with no announced limit, got error {e}"
        )),
    }
}

/// v5, fake broker whose CONNACK announces Maximum Packet Size = 1 MiB (property 0x27): exactly
/// 1 MiB is sent, one byte more is refused before the send with `OverMaxPacketSize`, and the
/// broker sees no second packet.
pub async fn size_announced_limit() -> Result<String, String> {
    let max = 1usize << 20;
    let f = Fake::new().await;
    let (l, port) = (f.l, f.port);
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::<usize>::new()));
    let s2 = seen.clone();
    tokio::task::spawn_local(async move {
        use tokio::io::AsyncWriteExt;
        let mut st = l.accept().await.expect("accept").0;
        let _ = fake::read_pkt(&mut st).await;
        let mut body = vec![0x00, 0x00, 0x05, 0x27];
        body.extend((max as u32).to_be_bytes());
        let _ = st.write_all(&fake::pkt(0x20, &body)).await;
        while let Ok((h, b)) = fake::read_pkt(&mut st).await {
            if h & 0xF0 == 0x30 {
                s2.borrow_mut()
                    .push(1 + fake::varint(b.len() as u32).len() + b.len());
            }
        }
    });
    let o = Opts::new(Ver::V5, Tr::Tcp, "127.0.0.1", port);
    let c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
    let topic = "sz/ann";
    let exact = vec![b'x'; payload_for(Ver::V5, topic, 0, max)];
    c.sink.publish(topic, exact, 0).await?;
    let over = vec![b'x'; payload_for(Ver::V5, topic, 0, max) + 1];
    let r = c.sink.publish(topic, over, 0).await;
    sleep_ms(300).await;
    let sizes = seen.borrow().clone();
    let d = format!("exact send ok; +1 byte: {r:?}; the broker saw publish sizes {sizes:?}");
    match r {
        Err(e) if e.contains("OverMaxPacketSize") && sizes == [max] => Ok(d),
        _ => Err(d),
    }
}

async fn sleep_ms(n: u64) {
    tokio::time::sleep(Duration::from_millis(n)).await;
}

/// A fake broker pushes a whole packet of exactly `max_packet` (`plus` 0) or one byte more.
pub async fn size_incoming(ver: Ver, plus: usize) -> Result<String, String> {
    let max = 1usize << 20;
    let f = Fake::new().await;
    let port = f.port;
    let topic = "t/sz";
    let body = vec![b'y'; payload_for(ver, topic, 0, max) + plus];
    let wire = fake::publish(ver, topic, 0, 0, &body);
    let total = wire.len();
    tokio::task::spawn_local(async move {
        use tokio::io::AsyncWriteExt;
        let mut s = f.accept().await;
        let _ = fake::handshake(&mut s, ver).await;
        let _ = s.write_all(&wire).await;
        let _ = fake::drain(&mut s, Duration::from_secs(5)).await;
    });
    let o = Opts::new(ver, Tr::Tcp, "127.0.0.1", port);
    let mut c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
    let t = Instant::now();
    let got = recv(&mut c, topic, &body).await;
    match (plus, got) {
        (0, Ok(_)) => Ok(format!(
            "whole packet of {total} bytes delivered in {}",
            ms(t.elapsed())
        )),
        (0, Err(e)) => Err(format!("{total} bytes: {e}")),
        (_, Ok(_)) => Err(format!("{total} bytes (max+{plus}) was delivered")),
        (_, Err(e)) => {
            let end = timeout(Duration::from_secs(3), c.end).await;
            let end = end
                .map_err(|_| "connection still open after 3 s")?
                .map_err(|e| e.to_string())?;
            let d = format!(
                "{total} bytes (max+{plus}) refused after {}: {e}; stop {}",
                ms(t.elapsed()),
                end.stop
            );
            if e == "incoming channel closed" && end.stop.contains("MaxSizeExceeded") {
                Ok(d)
            } else {
                Err(d)
            }
        }
    }
}

/// v3 only: the codec checks the Remaining Length, not the length of its encoding. A 4-byte
/// encoding of `r` passes (the packet is 1 byte over `max_packet`), and `r + 1` fails.
pub async fn size_nonminimal(plus: u32) -> Result<String, String> {
    let r = v3_max_remaining(1 << 20) + plus;
    let topic = "t/nm";
    let mut body = (topic.len() as u16).to_be_bytes().to_vec();
    body.extend_from_slice(topic.as_bytes());
    let payload = vec![b'z'; r as usize - body.len()];
    body.extend_from_slice(&payload);
    // 4-byte non-minimal varint of r.
    let mut wire = vec![
        0x30,
        (r & 0x7f) as u8 | 0x80,
        ((r >> 7) & 0x7f) as u8 | 0x80,
    ];
    wire.push(((r >> 14) & 0x7f) as u8 | 0x80);
    wire.push((r >> 21) as u8);
    wire.extend_from_slice(&body);
    let total = wire.len();
    let f = Fake::new().await;
    let port = f.port;
    tokio::task::spawn_local(async move {
        use tokio::io::AsyncWriteExt;
        let mut s = f.accept().await;
        let _ = fake::handshake(&mut s, Ver::V3).await;
        let _ = s.write_all(&wire).await;
        let _ = fake::drain(&mut s, Duration::from_secs(5)).await;
    });
    let o = Opts::new(Ver::V3, Tr::Tcp, "127.0.0.1", port);
    let mut c = connect(&o, None).await.map_err(|e| format!("{e:?}"))?;
    let got = recv(&mut c, topic, &payload).await;
    match (plus, got) {
        (0, Ok(_)) => Ok(format!(
            "r = {r}, non-minimal length, packet of {total} bytes delivered"
        )),
        (0, Err(e)) => Err(format!("r = {r}: {e}")),
        (_, Ok(_)) => Err(format!("r + {plus} was delivered")),
        (_, Err(e)) => {
            let end = timeout(Duration::from_secs(3), c.end).await;
            let end = end
                .map_err(|_| format!("open after 3 s without a delivery ({e})"))?
                .map_err(|e| e.to_string())?;
            let d = format!(
                "r + {plus} = {r}, packet of {total} bytes refused: {e}; stop {}",
                end.stop
            );
            if e == "incoming channel closed" && end.stop.contains("MaxSizeExceeded") {
                Ok(d)
            } else {
                Err(d)
            }
        }
    }
}

/// Unit check of the v3 Remaining Length helper at the varint boundaries.
pub fn varint_boundaries() -> Result<String, String> {
    let len = |r: u32| fake::varint(r).len() as u32;
    let mut lines = String::new();
    for max in [
        128u32,
        129,
        130,
        16_385,
        16_386,
        2_097_153,
        2_097_154,
        1 << 20,
    ] {
        let r = v3_max_remaining(max);
        let fits = 1 + len(r) + r <= max;
        let next_fails = 1 + len(r + 1) + r + 1 > max;
        if !(fits && next_fails) {
            return Err(format!(
                "max {max}: r {r} fits {fits} next_fails {next_fails}"
            ));
        }
        lines += &format!("{max}->{r} ");
    }
    // Both sides of each length transition (1 to 2, 2 to 3 and 3 to 4 bytes): (limit, r).
    for (max, want) in [
        (129u32, 127u32),
        (130, 127),
        (131, 128),
        (16_386, 16_383),
        (16_387, 16_383),
        (16_388, 16_384),
        (2_097_155, 2_097_151),
        (2_097_156, 2_097_151),
        (2_097_157, 2_097_152),
    ] {
        let got = v3_max_remaining(max);
        if got != want {
            return Err(format!("limit {max}: r {got}, want {want}"));
        }
        lines += &format!("{max}->{got} ");
    }
    if v3_max_remaining(1 << 20) != 1_048_572 {
        return Err(format!("1 MiB gives {}", v3_max_remaining(1 << 20)));
    }
    Ok(format!("max_packet->r: {lines}(1 MiB -> 1 048 572)"))
}
