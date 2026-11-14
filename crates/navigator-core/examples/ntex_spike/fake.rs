//! Fake broker helpers: raw tokio TCP on loopback, and a small WebSocket server side.

use std::io;
use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};

use crate::wire::Ver;

pub struct Fake {
    pub l: TcpListener,
    pub port: u16,
}

impl Fake {
    pub async fn new() -> Self {
        Self::with_rcvbuf(None).await
    }

    /// `rcvbuf` makes a small receive window, so a client that sends a lot blocks early.
    pub async fn with_rcvbuf(rcvbuf: Option<u32>) -> Self {
        let sock = TcpSocket::new_v4().expect("socket");
        if let Some(n) = rcvbuf {
            sock.set_recv_buffer_size(n).expect("rcvbuf");
        }
        sock.bind("127.0.0.1:0".parse().expect("addr"))
            .expect("bind");
        let l = sock.listen(16).expect("listen");
        let port = l.local_addr().expect("addr").port();
        Self { l, port }
    }

    pub async fn accept(&self) -> TcpStream {
        self.l.accept().await.expect("accept").0
    }
}

pub fn varint(mut n: u32) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let b = (n % 128) as u8;
        n /= 128;
        if n > 0 {
            out.push(b | 0x80);
        } else {
            out.push(b);
            return out;
        }
    }
}

pub fn pkt(header: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![header];
    v.extend(varint(body.len() as u32));
    v.extend_from_slice(body);
    v
}

pub async fn read_pkt(s: &mut TcpStream) -> io::Result<(u8, Vec<u8>)> {
    let h = s.read_u8().await?;
    let (mut n, mut mult) = (0usize, 1usize);
    loop {
        let b = s.read_u8().await?;
        n += usize::from(b & 0x7f) * mult;
        mult *= 128;
        if b & 0x80 == 0 {
            break;
        }
    }
    let mut body = vec![0u8; n];
    s.read_exact(&mut body).await?;
    Ok((h, body))
}

pub fn connack_ok(ver: Ver) -> Vec<u8> {
    match ver {
        Ver::V3 => vec![0x20, 2, 0, 0],
        Ver::V5 => vec![0x20, 3, 0, 0, 0],
    }
}

/// Reads CONNECT, answers an accepting CONNACK.
pub async fn handshake(s: &mut TcpStream, ver: Ver) -> io::Result<Vec<u8>> {
    let (h, body) = read_pkt(s).await?;
    assert_eq!(h, 0x10, "CONNECT expected");
    s.write_all(&connack_ok(ver)).await?;
    Ok(body)
}

/// PUBLISH packet, minimal encoding.
pub fn publish(ver: Ver, topic: &str, qos: u8, pid: u16, payload: &[u8]) -> Vec<u8> {
    let mut b = (topic.len() as u16).to_be_bytes().to_vec();
    b.extend_from_slice(topic.as_bytes());
    if qos > 0 {
        b.extend_from_slice(&pid.to_be_bytes());
    }
    if ver == Ver::V5 {
        b.push(0);
    }
    b.extend_from_slice(payload);
    pkt(0x30 | (qos << 1), &b)
}

/// Drains a socket until EOF or error. Returns the packets' first bytes and whether EOF came.
pub async fn drain(s: &mut TcpStream, max: std::time::Duration) -> (Vec<u8>, bool) {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    let end = tokio::time::Instant::now() + max;
    loop {
        match tokio::time::timeout_at(end, s.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return (got, true),
            Ok(Ok(n)) => got.extend_from_slice(&buf[..n]),
            Err(_) => return (got, false),
        }
    }
}

// ---- WebSocket server side -------------------------------------------------------------

/// Reads the HTTP upgrade request and answers 101 with subprotocol `mqtt`.
pub async fn ws_accept(s: &mut TcpStream) -> io::Result<String> {
    let mut req = Vec::new();
    let mut b = [0u8; 1];
    while !req.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b).await?;
        req.push(b[0]);
    }
    let text = String::from_utf8_lossy(&req).into_owned();
    let resp = ws_accept_response(&text)?;
    s.write_all(resp.as_bytes()).await?;
    Ok(text)
}

/// The 101 response for an upgrade request (subprotocol `mqtt`).
pub fn ws_accept_response(text: &str) -> io::Result<String> {
    let key = text
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .starts_with("sec-websocket-key:")
                .then(|| l.split_once(':').map(|x| x.1.trim().to_owned()))
                .flatten()
        })
        .ok_or_else(|| io::Error::other("no key"))?;
    let mut hash_in = key.into_bytes();
    hash_in.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let accept = openssl::base64::encode_block(&openssl::sha::sha1(&hash_in));
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: mqtt\r\n\r\n"
    );
    Ok(resp)
}

/// One server (unmasked) frame.
pub fn ws_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![(if fin { 0x80 } else { 0 }) | opcode];
    match payload.len() {
        n if n < 126 => f.push(n as u8),
        n if n < 65536 => {
            f.push(126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            f.push(127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    f.extend_from_slice(payload);
    f
}

/// Reads one client (masked) frame. Returns (opcode, unmasked payload).
pub async fn ws_read(s: &mut TcpStream) -> io::Result<(u8, Vec<u8>)> {
    let b0 = s.read_u8().await?;
    let b1 = s.read_u8().await?;
    let mut len = u64::from(b1 & 0x7f);
    if len == 126 {
        len = u64::from(s.read_u16().await?);
    } else if len == 127 {
        len = s.read_u64().await?;
    }
    let mut mask = [0u8; 4];
    if b1 & 0x80 != 0 {
        s.read_exact(&mut mask).await?;
    }
    let mut p = vec![0u8; len as usize];
    s.read_exact(&mut p).await?;
    if b1 & 0x80 != 0 {
        for (i, x) in p.iter_mut().enumerate() {
            *x ^= mask[i % 4];
        }
    }
    Ok((b0 & 0x0f, p))
}

pub fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}
