//! "Our dialer": DNS in our own blocking task (one lookup at a time), one TCP dial for each
//! resolved address in order, then `from_tcp_stream`, then TLS with ntex-tls when needed.
//! It is the connector for MQTT (TCP, TLS) and for `WsClient` (WS, WSS).

use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use ntex_error::Error;
use ntex_io::{Io, Sealed};
use ntex_net::connect::{Address, Connect, ConnectError, Connector};
use ntex_service::{Ctx, Service, cfg::SharedCfg};
use ntex_tls::openssl::SslConnector as NtexSsl;
use tokio::sync::Semaphore;

/// Host and port to dial. The host stays the profile host (SNI, verification, WS authority).
#[derive(Clone, Debug)]
pub struct Target {
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

/// Test hook for DNS. The default is `getaddrinfo`.
pub type Resolver = Arc<dyn Fn(&str, u16) -> io::Result<Vec<SocketAddr>> + Send + Sync>;

static LOOKUP_SLOT: std::sync::LazyLock<Arc<Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(Semaphore::new(1)));

#[derive(Clone)]
pub struct Dialer {
    resolver: Resolver,
    /// One permit for the whole process: at most one OS lookup runs. The blocking task owns it
    /// until the lookup returns.
    slot: Arc<Semaphore>,
    tls: Option<NtexSsl<Connector<Target>>>,
}

impl Dialer {
    pub fn new(tls: Option<openssl::ssl::SslConnector>) -> Self {
        Self {
            resolver: Arc::new(|host, port| Ok((host, port).to_socket_addrs()?.collect())),
            slot: LOOKUP_SLOT.clone(),
            tls: tls.map(NtexSsl::new),
        }
    }

    pub fn with_resolver(mut self, r: Resolver) -> Self {
        self.resolver = r;
        self
    }

    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(ip, port)]);
        }
        // Cancel safe: a dropped wait gives the permit back at once. A started lookup keeps it.
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

    /// Each address in order. The caller wraps this in the one connect deadline.
    async fn dial(&self, host: &str, port: u16) -> io::Result<std::net::TcpStream> {
        let mut last = None;
        for addr in self.resolve(host, port).await? {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(s) => return s.into_std(),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no addresses")))
    }
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
        match &self.tls {
            None => Ok(io.seal()),
            Some(tls) => Ok(tls.connect(io, &host, ctx.st()).await?.seal()),
        }
    }
}
