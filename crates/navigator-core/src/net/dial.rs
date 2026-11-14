//! Our dialer: DNS in our own blocking task (one lookup at a time), one TCP dial for each
//! resolved address in order, then `from_tcp_stream`, then TLS with `ntex-tls` when needed.
//! It is the connector for MQTT over TCP and TLS, and for `WsClient` (WS and WSS).
//!
//! The caller wraps every use in one deadline (`CONNECT_TIMEOUT`). A cancel drops the future:
//! the TCP dial and the wait for DNS end at once. The OS lookup itself cannot stop, so it keeps
//! the only permit of a one-slot semaphore until it returns.

use std::cell::Cell;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::rc::Rc;
use std::sync::{Arc, LazyLock};

use ntex_error::Error;
use ntex_io::{Io, Sealed};
use ntex_net::connect::{Address, Connect, ConnectError};
use ntex_service::cfg::SharedCfg;
use ntex_service::{Ctx, Service};
use ntex_tls::openssl::SslConnector as NtexSsl;
use tokio::sync::Semaphore;

/// Host and port to dial. The host stays the profile host (SNI, verification, WS authority).
#[derive(Clone, Debug)]
pub(super) struct Target {
    pub host: String,
    pub port: u16,
}

impl Address for Target {
    fn host(&self) -> &str {
        &self.host
    }
    fn port(&self) -> Option<u16> {
        Some(self.port)
    }
}

/// The DNS lookup. The default is `getaddrinfo`. Tests give their own.
pub(super) type Resolver = Arc<dyn Fn(&str, u16) -> io::Result<Vec<SocketAddr>> + Send + Sync>;

/// One permit for the whole process: at most one OS lookup runs at a time.
static LOOKUP_SLOT: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));

#[derive(Clone)]
pub(super) struct Dialer {
    resolver: Resolver,
    slot: Arc<Semaphore>,
    tls: Option<NtexSsl<ntex_net::connect::Connector<Target>>>,
    /// Set when the transport is complete (TCP, and TLS if used). After that, a missing CONNACK
    /// is a broker problem, not a network problem.
    transport_up: Rc<Cell<bool>>,
}

impl Dialer {
    pub(super) fn new(
        tls: Option<openssl::ssl::SslConnector>,
        transport_up: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            resolver: Arc::new(|host, port| Ok((host, port).to_socket_addrs()?.collect())),
            slot: LOOKUP_SLOT.clone(),
            tls: tls.map(NtexSsl::new),
            transport_up,
        }
    }

    /// A dialer with its own lookup and its own permit (tests).
    #[cfg(test)]
    pub(super) fn with_lookup(mut self, resolver: Resolver, slot: Arc<Semaphore>) -> Self {
        self.resolver = resolver;
        self.slot = slot;
        self
    }

    pub(super) fn mark_up(&self) {
        self.transport_up.set(true);
    }

    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(ip, port)]);
        }
        // Cancel safe: a dropped wait gives nothing away. A started lookup keeps the permit.
        let permit = self
            .slot
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| io::Error::other("resolver closed"))?;
        let (resolver, h) = (self.resolver.clone(), host.to_owned());
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            resolver(&h, port)
        })
        .await
        .map_err(|e| io::Error::other(format!("lookup task failed: {e}")))?
    }

    /// Each address in order. The first that connects wins. The error is the last one.
    pub(super) async fn dial(&self, host: &str, port: u16) -> io::Result<std::net::TcpStream> {
        let mut last = None;
        for addr in self.resolve(host, port).await? {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(s) => {
                    // Small packets (PUBACK, PINGREQ) must not wait for more data.
                    let _ = s.set_nodelay(true);
                    return s.into_std();
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "failed to lookup address information: no addresses",
            )
        }))
    }
}

/// The host without the brackets of an IPv6 address.
fn tls_name(host: &str) -> &str {
    host.trim_start_matches('[').trim_end_matches(']')
}

impl<A: Address> Service<SharedCfg, Connect<A>> for Dialer {
    type Res = Io<Sealed>;
    type Error = Error<ConnectError>;

    async fn call(
        &self,
        req: Connect<A>,
        ctx: Ctx<'_, Self, SharedCfg>,
    ) -> Result<Io<Sealed>, Self::Error> {
        let host = req.host().to_owned();
        let tcp = self
            .dial(&host, req.port())
            .await
            .map_err(|e| Error::from(ConnectError::Io(e)))?;
        let io = ntex_net::from_tcp_stream(tcp, ctx.st().clone())
            .map_err(|e| Error::from(ConnectError::Io(e)))?;
        let io = match &self.tls {
            None => io.seal(),
            // The name for SNI and verification has no brackets. The WebSocket URL has them
            // around an IPv6 address, and OpenSSL would take "[::1]" for a host name.
            Some(tls) => tls.connect(io, tls_name(&host), ctx.st()).await?.seal(),
        };
        self.mark_up();
        Ok(io)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    fn dialer(resolver: Resolver) -> Dialer {
        Dialer::new(None, Rc::default()).with_lookup(resolver, Arc::new(Semaphore::new(1)))
    }

    /// The first address refuses, the second works: the dial returns the second connection. When
    /// every address refuses, the error is the last one.
    #[tokio::test(flavor = "current_thread")]
    async fn the_next_address_is_tried_when_the_first_refuses() {
        let open = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let open_addr = open.local_addr().expect("addr");
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr"); // the listener is dropped: refused
        let d = dialer(Arc::new(move |_, _| Ok(vec![closed, open_addr])));
        let stream = d.dial("broker.test", 1).await.expect("second address");
        assert_eq!(stream.peer_addr().expect("peer"), open_addr);

        let d = dialer(Arc::new(move |_, _| Ok(vec![closed, closed])));
        let err = d.dial("broker.test", 1).await.expect_err("all refuse");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    /// A lookup that never returns, and 10 attempts that give up: only one OS lookup runs. The
    /// next attempt can start a lookup after the first one returned.
    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_attempts_keep_one_lookup() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let wait = Mutex::new(wait);
        let counted = calls.clone();
        let d = dialer(Arc::new(move |_, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            // Blocks until the sender is dropped.
            let _ = wait.lock().expect("lock").recv();
            Ok(Vec::new())
        }));
        for _ in 0..10 {
            let attempt =
                tokio::time::timeout(Duration::from_millis(50), d.dial("stalled.test", 1));
            assert!(attempt.await.is_err(), "the lookup is stalled");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one lookup, not 10");
        drop(release);
        let err = tokio::time::timeout(Duration::from_secs(2), d.dial("stalled.test", 1))
            .await
            .expect("the permit is free after the lookup returned")
            .expect_err("the lookup found no address");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
