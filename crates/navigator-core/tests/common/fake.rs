//! A fake broker for the tests that need no real broker: TCP, TLS, WS and WSS on the loopback.
//! The handler gets a [`Conn`] and does what the test needs (stop reading, send half a packet,
//! never answer). The server works with blocking `std` sockets on its own threads.
#![allow(dead_code)] // Each test crate uses a part of it.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use navigator_core::net::{Protocol, Transport};
use openssl::asn1::Asn1Time;
use openssl::bn::BigNum;
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::ssl::{SslAcceptor, SslMethod, SslStream, SslVerifyMode};
use openssl::x509::extension::{BasicConstraints, SubjectAlternativeName};
use openssl::x509::{X509, X509NameBuilder};

/// Certificates for `127.0.0.1` (IP entry), written to a temp folder.
pub struct Certs {
    _dir: tempfile::TempDir,
    pub ca_file: PathBuf,
    pub client_cert: PathBuf,
    pub client_key: PathBuf,
    ca: X509,
    server: X509,
    server_key: PKey<Private>,
}

fn key() -> PKey<Private> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).expect("group");
    PKey::from_ec_key(EcKey::generate(&group).expect("ec key")).expect("pkey")
}

fn cert(cn: &str, key: &PKey<Private>, ca: Option<(&X509, &PKey<Private>)>) -> X509 {
    let mut name = X509NameBuilder::new().expect("name");
    name.append_entry_by_nid(Nid::COMMONNAME, cn).expect("cn");
    let name = name.build();
    let mut b = X509::builder().expect("builder");
    b.set_version(2).expect("version");
    let serial = BigNum::from_u32(1)
        .and_then(|n| n.to_asn1_integer())
        .expect("serial");
    b.set_serial_number(&serial).expect("serial");
    b.set_subject_name(&name).expect("subject");
    b.set_issuer_name(ca.map_or(&*name, |(c, _)| c.subject_name()))
        .expect("issuer");
    b.set_pubkey(key).expect("pubkey");
    b.set_not_before(&Asn1Time::days_from_now(0).expect("time"))
        .expect("nb");
    b.set_not_after(&Asn1Time::days_from_now(30).expect("time"))
        .expect("na");
    if ca.is_none() {
        let bc = BasicConstraints::new().critical().ca().build().expect("bc");
        b.append_extension(bc).expect("ext");
    }
    let mut san = SubjectAlternativeName::new();
    san.ip("127.0.0.1");
    let san = san
        .build(&b.x509v3_context(ca.map(|(c, _)| c.as_ref()), None))
        .expect("san");
    b.append_extension(san).expect("ext");
    b.sign(ca.map_or(key, |(_, k)| k), MessageDigest::sha256())
        .expect("sign");
    b.build()
}

impl Certs {
    pub fn new() -> Self {
        let (ca_key, server_key, client_key) = (key(), key(), key());
        let ca = cert("Test CA", &ca_key, None);
        let server = cert("127.0.0.1", &server_key, Some((&ca, &ca_key)));
        let client = cert("client", &client_key, Some((&ca, &ca_key)));
        let dir = tempfile::tempdir().expect("temp dir");
        let write = |name: &str, bytes: Vec<u8>| {
            let p = dir.path().join(name);
            std::fs::write(&p, bytes).expect("write");
            p
        };
        Self {
            ca_file: write("ca.pem", ca.to_pem().expect("pem")),
            client_cert: write("client.pem", client.to_pem().expect("pem")),
            client_key: write(
                "client.key",
                client_key.private_key_to_pem_pkcs8().expect("pem"),
            ),
            _dir: dir,
            ca,
            server,
            server_key,
        }
    }

    fn acceptor(&self, require_client_cert: bool) -> SslAcceptor {
        let mut a =
            SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).expect("acceptor");
        a.set_certificate(&self.server).expect("cert");
        a.set_private_key(&self.server_key).expect("key");
        if require_client_cert {
            a.cert_store_mut().add_cert(self.ca.clone()).expect("ca");
            a.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);
        }
        a.build()
    }
}

/// The socket of a TLS server. With `lossy` it drops every write after the first read that follows
/// a write. That is the alert after a failed handshake: the client never sees it, as after a
/// reset on Windows (Linux still hands over the data that came before a reset). Use `lossy` only
/// with clients that fail the handshake.
struct Gate {
    tcp: TcpStream,
    lossy: bool,
    wrote: bool,
    swallow: bool,
}

impl Read for Gate {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.swallow |= self.lossy && self.wrote;
        self.tcp.read(buf)
    }
}

impl Write for Gate {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.swallow {
            return Ok(buf.len());
        }
        self.wrote = true;
        self.tcp.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}

enum Stream {
    Tcp(TcpStream),
    Tls(Box<SslStream<Gate>>),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Self::Tcp(s) => s,
            Self::Tls(s) => &s.get_ref().tcp,
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.read(buf),
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.write(buf),
            Self::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}

/// One accepted connection. For WS and WSS the handler calls [`Conn::upgrade`] (it may also
/// never call it, to stall the upgrade).
pub struct Conn {
    stream: Stream,
    ws: bool,
    /// Payload of WebSocket frames that the MQTT reader has not used yet.
    pending: Vec<u8>,
    pub version: Protocol,
}

pub fn varint(mut n: usize) -> Vec<u8> {
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

/// A packet with the fixed header `header` and the body `body`.
pub fn pkt(header: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![header];
    v.extend(varint(body.len()));
    v.extend_from_slice(body);
    v
}

/// A server (unmasked) WebSocket frame.
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

impl Conn {
    /// Reads the HTTP upgrade request and answers 101 with the subprotocol `mqtt`.
    pub fn upgrade(&mut self) -> io::Result<()> {
        let mut req = Vec::new();
        let mut b = [0u8; 1];
        while !req.ends_with(b"\r\n\r\n") {
            self.stream.read_exact(&mut b)?;
            req.push(b[0]);
        }
        let text = String::from_utf8_lossy(&req).into_owned();
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
        self.stream.write_all(
            format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: mqtt\r\n\r\n"
            )
            .as_bytes(),
        )
    }

    /// Reads one client (masked) frame: (opcode, unmasked payload).
    pub fn ws_read(&mut self) -> io::Result<(u8, Vec<u8>)> {
        let mut h = [0u8; 2];
        self.stream.read_exact(&mut h)?;
        let mut len = u64::from(h[1] & 0x7f);
        if len == 126 {
            let mut b = [0u8; 2];
            self.stream.read_exact(&mut b)?;
            len = u64::from(u16::from_be_bytes(b));
        } else if len == 127 {
            let mut b = [0u8; 8];
            self.stream.read_exact(&mut b)?;
            len = u64::from_be_bytes(b);
        }
        let mut mask = [0u8; 4];
        if h[1] & 0x80 != 0 {
            self.stream.read_exact(&mut mask)?;
        }
        let mut p = vec![0u8; len as usize];
        self.stream.read_exact(&mut p)?;
        if h[1] & 0x80 != 0 {
            for (i, x) in p.iter_mut().enumerate() {
                *x ^= mask[i % 4];
            }
        }
        Ok((h[0] & 0x0f, p))
    }

    /// Reads `n` bytes of the MQTT stream (from WebSocket frames when needed).
    fn read_exact(&mut self, n: usize) -> io::Result<Vec<u8>> {
        if !self.ws {
            let mut v = vec![0u8; n];
            self.stream.read_exact(&mut v)?;
            return Ok(v);
        }
        while self.pending.len() < n {
            match self.ws_read()? {
                (0 | 2, p) => self.pending.extend(p),
                (8, _) => return Err(io::ErrorKind::UnexpectedEof.into()),
                _ => {}
            }
        }
        Ok(self.pending.drain(..n).collect())
    }

    /// Reads one MQTT packet: (first byte, body).
    pub fn read_packet(&mut self) -> io::Result<(u8, Vec<u8>)> {
        let head = self.read_exact(1)?[0];
        let (mut n, mut mult) = (0usize, 1usize);
        loop {
            let b = self.read_exact(1)?[0];
            n += usize::from(b & 0x7f) * mult;
            mult *= 128;
            if b & 0x80 == 0 {
                break;
            }
        }
        Ok((head, self.read_exact(n)?))
    }

    /// Like `read_packet`, but `None` when no packet starts within `idle`. Plain TCP and TLS only.
    pub fn read_packet_idle(&mut self, idle: Duration) -> io::Result<Option<(u8, Vec<u8>)>> {
        assert!(!self.ws, "read_packet_idle is for TCP and TLS");
        self.stream.tcp().set_read_timeout(Some(idle))?;
        let mut first = [0u8; 1];
        match self.stream.read(&mut first) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
        self.stream.tcp().set_read_timeout(None)?;
        let (mut n, mut mult) = (0usize, 1usize);
        loop {
            let b = self.read_exact(1)?[0];
            n += usize::from(b & 0x7f) * mult;
            mult *= 128;
            if b & 0x80 == 0 {
                break;
            }
        }
        Ok(Some((first[0], self.read_exact(n)?)))
    }

    /// Sends MQTT bytes. Over WS and WSS they go in one binary frame.
    pub fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.ws {
            self.stream.write_all(&ws_frame(true, 2, bytes))
        } else {
            self.stream.write_all(bytes)
        }
    }

    /// Sends bytes as they are (a part of a frame, for example).
    pub fn send_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stream.write_all(bytes)?;
        self.stream.flush()
    }

    /// WS and WSS: the upgrade. Then reads CONNECT and answers with an accepting CONNACK. Returns
    /// the CONNECT body.
    pub fn accept_connect(&mut self) -> io::Result<Vec<u8>> {
        let connack: &[u8] = match self.version {
            Protocol::V311 => &[0x20, 2, 0, 0],
            Protocol::V5 => &[0x20, 3, 0, 0, 0],
        };
        self.accept_connect_with(connack)
    }

    /// Like `accept_connect` with the CONNACK `connack`.
    pub fn accept_connect_with(&mut self, connack: &[u8]) -> io::Result<Vec<u8>> {
        if self.ws {
            self.upgrade()?;
        }
        let (head, body) = self.read_packet()?;
        assert_eq!(head, 0x10, "CONNECT expected");
        self.send(connack)?;
        Ok(body)
    }

    /// Reads and drops what the client sends until it closes or `limit` has passed.
    pub fn hold(&mut self, limit: Duration) {
        self.stream
            .tcp()
            .set_read_timeout(Some(limit))
            .expect("timeout");
        let mut buf = [0u8; 4096];
        while self.stream.read(&mut buf).is_ok_and(|n| n > 0) {}
    }

    /// Does not read for `limit` (the receive buffers fill up), then returns.
    pub fn stop_reading(&self, limit: Duration) {
        std::thread::sleep(limit);
    }

    /// Reads packets until the client closes; calls `f` for each. A read ends after `limit`.
    pub fn read_until_close(&mut self, limit: Duration, mut f: impl FnMut(u8, Vec<u8>)) {
        self.stream
            .tcp()
            .set_read_timeout(Some(limit))
            .expect("timeout");
        while let Ok((h, b)) = self.read_packet() {
            f(h, b);
        }
    }
}

#[derive(Default)]
pub struct Options {
    pub require_client_cert: bool,
    /// After a failed TLS handshake, lose the alert and close with the data of the client still
    /// unread (a reset). Windows loses the alert in this case; see `Gate`. The default models a
    /// broker that sends the alert and closes cleanly (it drains first).
    pub reset_on_handshake_failure: bool,
}

/// A handshake failed and the alert is sent. Close the socket in one of two ways (see `Options`).
fn end_failed_handshake(sock: &TcpStream, reset: bool) {
    let mut buf = [0u8; 1024];
    if reset {
        // Wait until the client has sent its next data (the WS upgrade or CONNECT). Then close
        // with that data unread.
        sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let _ = sock.peek(&mut buf);
        return;
    }
    let _ = sock.shutdown(std::net::Shutdown::Write);
    sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
    let mut s = sock;
    while matches!(s.read(&mut buf), Ok(n) if n > 0) {}
}

pub struct Fake {
    pub port: u16,
    pub certs: Arc<Certs>,
    stop: Arc<AtomicBool>,
}

impl Fake {
    /// Starts a server. `handler(n, conn)` runs on its own thread for the n-th connection (from 0).
    /// A failed TLS handshake ends the connection without a call.
    pub fn start(
        transport: Transport,
        version: Protocol,
        options: Options,
        handler: impl Fn(usize, Conn) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let certs = Arc::new(Certs::new());
        let stop = Arc::new(AtomicBool::new(false));
        let tls = matches!(transport, Transport::Tls | Transport::Wss);
        let ws = matches!(transport, Transport::Ws | Transport::Wss);
        let acceptor = tls.then(|| Arc::new(certs.acceptor(options.require_client_cert)));
        let handler = Arc::new(handler);
        let stop2 = stop.clone();
        let reset = options.reset_on_handshake_failure;
        std::thread::spawn(move || {
            for (n, sock) in listener.incoming().flatten().enumerate() {
                if stop2.load(Ordering::SeqCst) {
                    return;
                }
                let (acceptor, handler) = (acceptor.clone(), handler.clone());
                std::thread::spawn(move || {
                    let stream = match acceptor {
                        Some(a) => match a.accept(Gate {
                            tcp: sock,
                            lossy: reset,
                            wrote: false,
                            swallow: false,
                        }) {
                            Ok(s) => Stream::Tls(Box::new(s)),
                            Err(openssl::ssl::HandshakeError::Failure(mid)) => {
                                end_failed_handshake(&mid.get_ref().tcp, reset);
                                return;
                            }
                            Err(_) => return,
                        },
                        None => Stream::Tcp(sock),
                    };
                    handler(
                        n,
                        Conn {
                            stream,
                            ws,
                            pending: Vec::new(),
                            version,
                        },
                    );
                });
            }
        });
        Self { port, certs, stop }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}
