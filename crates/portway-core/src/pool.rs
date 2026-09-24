//! One HTTP/1.1 connection pool per upstream, dialed and timed by hand.
//!
//! Owning the dial is what makes the phase log honest: DNS, TCP and TLS are
//! measured where they happen, and the socket's fd is captured before the
//! stream disappears into hyper, so the ACK watcher always has a handle.
//!
//! HTTP/1.1 on purpose. hyper's h1 client has no h2 path at all and the TLS
//! config pins ALPN to `http/1.1`, because an agent abort must *close the
//! connection* — that disconnect is what stops the engine generating. On a
//! pooled h2 connection an early close left the engine running to max_tokens
//! HTTP/1.1 makes cancellation close the request connection.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::Uri;
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::body::{ClockSlot, TimedBody, TimedIo};
use crate::clock::PhaseClock;

/// 300s keeps the pooled upstream connection through any realistic pause
/// between agent requests. httpx's 5s default expired it and re-paid
/// dns+tcp+tls (~70ms) on nearly every request that followed a few idle
/// seconds — the bug this constant exists to prevent.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_IDLE: usize = 8;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum UpstreamError {
    Connect(io::Error),
    Body(String),
    Resolve(String),
    Protocol(hyper::Error),
    Timeout(&'static str),
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::Body(e) => write!(f, "BodyError({e})"),
            UpstreamError::Connect(e) => write!(f, "ConnectError({e})"),
            UpstreamError::Resolve(host) => write!(f, "ResolveError({host})"),
            UpstreamError::Protocol(e) => write!(f, "ProtocolError({e})"),
            UpstreamError::Timeout(phase) => write!(f, "Timeout({phase})"),
        }
    }
}

/// A live connection plus the two handles the phase log needs from it.
struct PooledConn {
    send: SendRequest<TimedBody>,
    slot: Arc<ClockSlot>,
    fd: RawFd,
    idle_since: Instant,
}

/// Plain or TLS, resolved once at dial time so the hot path stays monomorphic.
enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

macro_rules! delegate {
    ($self:ident, $method:ident $(, $arg:expr)*) => {
        match &mut *$self {
            Stream::Plain(s) => Pin::new(s).$method($($arg),*),
            Stream::Tls(s) => Pin::new(s.as_mut()).$method($($arg),*),
        }
    };
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        delegate!(self, poll_read, cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        delegate!(self, poll_write, cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        delegate!(self, poll_write_vectored, cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Stream::Plain(s) => s.is_write_vectored(),
            Stream::Tls(s) => s.is_write_vectored(),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, poll_flush, cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, poll_shutdown, cx)
    }
}

pub struct Upstream {
    pub telemetry: Arc<crate::telemetry::Telemetry>,
    pub base_path: String,
    pub base: String,
    pub authority: String,
    host: String,
    port: u16,
    tls: Option<Arc<rustls::ClientConfig>>,
    idle: Mutex<VecDeque<PooledConn>>,
}

impl Upstream {
    pub fn new(base: &str, tls: Option<Arc<rustls::ClientConfig>>) -> Result<Self, String> {
        Self::with_telemetry(base, tls, Arc::default())
    }
    pub fn with_telemetry(
        base: &str,
        tls: Option<Arc<rustls::ClientConfig>>,
        telemetry: Arc<crate::telemetry::Telemetry>,
    ) -> Result<Self, String> {
        let uri: Uri = base
            .parse()
            .map_err(|e| format!("invalid upstream URL: {e}"))?;
        let host = uri
            .host()
            .ok_or_else(|| "upstream URL has no host".to_string())?
            .to_string();
        let secure = match uri.scheme_str() {
            Some("https") => true,
            None => return Err("upstream URL requires http:// or https://".into()),
            Some("http") => false,
            Some(other) => return Err(format!("unsupported upstream scheme {other}")),
        };
        if uri.query().is_some()
            || base.contains('#')
            || uri.authority().is_some_and(|a| a.as_str().contains('@'))
        {
            return Err("upstream URL must not contain credentials, query or fragment".into());
        }
        let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
        // The Host header keeps the port only when it is not the default, the
        // same shape httpx puts on the wire.
        let authority = match (secure, port) {
            (true, 443) | (false, 80) => host.clone(),
            _ => format!("{host}:{port}"),
        };
        Ok(Upstream {
            telemetry,
            base_path: uri.path().trim_end_matches('/').to_owned(),
            base: base.trim_end_matches('/').to_string(),
            authority,
            host,
            port,
            tls: if secure {
                Some(tls.unwrap_or_else(crate::server::tls_config))
            } else {
                None
            },
            idle: Mutex::new(VecDeque::new()),
        })
    }

    fn checkout(&self) -> Option<PooledConn> {
        let mut idle = self.idle.lock().unwrap();
        while let Some(conn) = idle.pop_front() {
            if conn.idle_since.elapsed() < IDLE_TIMEOUT && !conn.send.is_closed() {
                return Some(conn);
            }
        }
        None
    }

    fn put(&self, mut conn: PooledConn) {
        if conn.send.is_closed() {
            return;
        }
        conn.slot.clear();
        conn.idle_since = Instant::now();
        let mut idle = self.idle.lock().unwrap();
        if idle.len() < MAX_IDLE {
            idle.push_back(conn);
        }
    }

    pub fn idle_count(&self) -> usize {
        self.idle.lock().unwrap().len()
    }

    async fn dial(&self, clock: &PhaseClock) -> Result<PooledConn, UpstreamError> {
        let started = Instant::now();
        let addrs: Vec<_> = tokio::net::lookup_host((self.host.as_str(), self.port))
            .await
            .map_err(|_| UpstreamError::Resolve(self.host.clone()))?
            .collect();
        clock.set_dns(started.elapsed());
        if addrs.is_empty() {
            return Err(UpstreamError::Resolve(self.host.clone()));
        }

        let started = Instant::now();
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, connect_any(&addrs))
            .await
            .map_err(|_| UpstreamError::Timeout("connect"))?
            .map_err(UpstreamError::Connect)?;
        // Nagle would hold a small request back waiting for more; every
        // millisecond here rides on a 233ms round trip.
        let _ = tcp.set_nodelay(true);
        clock.set_tcp(started.elapsed());

        // Capture the fd before the stream moves into TLS and then into hyper.
        let fd = tcp.as_raw_fd();
        let stream = match &self.tls {
            None => Stream::Plain(tcp),
            Some(config) => {
                let started = Instant::now();
                let name = rustls::pki_types::ServerName::try_from(self.host.clone())
                    .map_err(|_| UpstreamError::Resolve(self.host.clone()))?;
                let tls = TlsConnector::from(Arc::clone(config))
                    .connect(name, tcp)
                    .await
                    .map_err(UpstreamError::Connect)?;
                clock.set_tls(started.elapsed());
                Stream::Tls(Box::new(tls))
            }
        };

        let slot: Arc<ClockSlot> = Arc::default();
        let io = TokioIo::new(TimedIo::new(
            stream,
            Arc::clone(&slot),
            Arc::clone(&self.telemetry),
        ));
        let (send, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(UpstreamError::Protocol)?;
        // The driver must run for the connection's whole life. Dropping the
        // response body later makes hyper close the socket from in here, which
        // is exactly how an agent abort reaches the engine.
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(PooledConn {
            send,
            slot,
            fd,
            idle_since: Instant::now(),
        })
    }

    /// Send one request, reusing a pooled connection when one is warm.
    ///
    /// A pooled connection can be closed by the far end between the check and
    /// the send, so a reused connection gets exactly one retry on a fresh dial
    /// — `try_send_request` hands the request back untouched for it.
    pub async fn send(
        self: &Arc<Self>,
        request: http::Request<TimedBody>,
        clock: Arc<PhaseClock>,
    ) -> Result<(http::Response<Incoming>, Lease), UpstreamError> {
        let mut request = request;
        if let Some(conn) = self.checkout() {
            match self.dispatch(conn, request, Arc::clone(&clock)).await {
                Ok(done) => return Ok(done),
                Err(Retry::Fatal(err)) => return Err(err),
                Err(Retry::Again(returned)) => request = *returned,
            }
        }
        let conn = self.dial(&clock).await?;
        match self.dispatch(conn, request, clock).await {
            Ok(done) => Ok(done),
            Err(Retry::Fatal(err)) => Err(err),
            Err(Retry::Again(_)) => Err(UpstreamError::Connect(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "upstream closed a freshly dialed connection",
            ))),
        }
    }

    async fn dispatch(
        self: &Arc<Self>,
        mut conn: PooledConn,
        request: http::Request<TimedBody>,
        clock: Arc<PhaseClock>,
    ) -> Result<(http::Response<Incoming>, Lease), Retry> {
        clock.set_fd(conn.fd);
        conn.slot.set(clock);
        if conn.send.ready().await.is_err() {
            return Err(Retry::Again(Box::new(request)));
        }
        match conn.send.try_send_request(request).await {
            Ok(response) => Ok((
                response,
                Lease {
                    conn: Some(conn),
                    upstream: Arc::clone(self),
                },
            )),
            Err(mut err) => match err.take_message() {
                Some(returned) => Err(Retry::Again(Box::new(returned))),
                None => Err(Retry::Fatal(UpstreamError::Protocol(err.into_error()))),
            },
        }
    }
}

/// Boxed because a retry is the cold path: the request only comes back when a
/// pooled connection turned out to be dead.
enum Retry {
    Again(Box<http::Request<TimedBody>>),
    Fatal(UpstreamError),
}

async fn connect_any(addrs: &[std::net::SocketAddr]) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::AddrNotAvailable, "no address");
    for addr in addrs {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(err) => last = err,
        }
    }
    Err(last)
}

/// Holds the connection for as long as its response body is being read.
///
/// Returned to the pool only by `release`, which the relay calls after reading
/// the body to completion. Every other end — an agent abort, an error, a
/// half-read stream — drops the lease, and a connection that still owes bytes
/// must not be reused.
pub struct Lease {
    conn: Option<PooledConn>,
    upstream: Arc<Upstream>,
}

impl Lease {
    pub fn release(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.upstream.put(conn);
        }
    }
}
