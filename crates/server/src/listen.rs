//! Serving HTTP on a listener, within the limits of the `http` config: a timeout for reading
//! request headers, closing connections that go idle or stall, a cap on open connections, and
//! on shutdown, a deadline for the requests in flight.

use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto,
    service::TowerToHyperService,
};
use rustls::ServerConfig;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
};

/// The limits of a listener, from the `http` config.
#[derive(Debug, Clone)]
pub struct Limits {
    header_read_timeout: Duration,
    idle_timeout: Option<Duration>,
    max_connections: u32,
    shutdown_timeout: Duration,
}

impl Limits {
    pub fn new(http: &hylla_config::Http) -> Self {
        let idle = http.idle_timeout.duration();
        Self {
            header_read_timeout: http.header_read_timeout.duration(),
            idle_timeout: (!idle.is_zero()).then_some(idle),
            max_connections: http.max_connections.max(1),
            shutdown_timeout: http.shutdown_timeout.duration(),
        }
    }
}

/// Tells listeners to stop accepting connections and finish the requests in flight.
#[derive(Clone)]
pub struct Shutdown(watch::Receiver<bool>);

impl Shutdown {
    /// Starts once `stopping` is set.
    pub fn new(stopping: &watch::Sender<bool>) -> Self {
        Self(stopping.subscribe())
    }

    /// Completes once shutdown started, or its sender is gone.
    async fn started(&mut self) {
        let _ = self.0.wait_for(|stopping| *stopping).await;
    }
}

// r[impl server.limits]
// r[impl server.shutdown]
/// Serves `router` on `listener`, over TLS with `tls`, until `shutdown` starts, then waits for
/// the requests in flight, for the shutdown timeout at most, before closing the connections.
/// `what` names the listener in logs, such as `cluster`.
pub async fn serve(
    listener: TcpListener,
    router: axum::Router,
    tls: Option<Arc<ServerConfig>>,
    limits: Limits,
    mut shutdown: Shutdown,
    what: &'static str,
) {
    let limits = Arc::new(limits);
    let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);
    let connections = Arc::new(Semaphore::new(limits.max_connections as usize));
    // Closes the connections still open when the shutdown timeout passes.
    let (close, closing) = watch::channel(false);
    loop {
        // Beyond the limit, connections wait in the listen backlog.
        let permit = tokio::select! {
            permit = connections.clone().acquire_owned() => permit.expect("never closed"),
            () = shutdown.started() => break,
        };
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    // Such as running out of file descriptors: wait for some to free up.
                    tracing::warn!("accepting a {what} connection failed: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            () = shutdown.started() => break,
        };
        let connection = Connection {
            router: router.clone(),
            limits: limits.clone(),
            shutdown: shutdown.clone(),
            closing: Shutdown(closing.clone()),
            what,
        };
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = stream.set_nodelay(true);
            match acceptor {
                Some(acceptor) => connection.serve_tls(stream, peer, acceptor).await,
                None => connection.serve(stream, peer).await,
            }
        });
    }
    drop(listener);

    let open = || limits.max_connections as usize - connections.available_permits();
    if open() > 0 {
        tracing::info!(
            connections = open(),
            "{what}: waiting for the requests in flight"
        );
    }
    let drained = connections.acquire_many(limits.max_connections);
    if tokio::time::timeout(limits.shutdown_timeout, drained)
        .await
        .is_err()
    {
        tracing::warn!(
            connections = open(),
            "{what}: closing the connections still open after the shutdown timeout"
        );
        let _ = close.send(true);
    }
}

/// What a connection is served with.
struct Connection {
    router: axum::Router,
    limits: Arc<Limits>,
    shutdown: Shutdown,
    closing: Shutdown,
    what: &'static str,
}

impl Connection {
    async fn serve_tls(
        self,
        stream: TcpStream,
        peer: std::net::SocketAddr,
        acceptor: tokio_rustls::TlsAcceptor,
    ) {
        let what = self.what;
        let handshake =
            tokio::time::timeout(self.limits.header_read_timeout, acceptor.accept(stream));
        match handshake.await {
            Ok(Ok(stream)) => self.serve(stream, peer).await,
            Ok(Err(error)) => tracing::warn!(%peer, "refused a {what} connection: {error}"),
            Err(_) => {
                tracing::warn!(%peer, "refused a {what} connection: the handshake timed out")
            }
        }
    }

    async fn serve<I>(mut self, io: I, peer: std::net::SocketAddr)
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let what = self.what;
        let activity = Arc::new(Activity::new());
        let io = Watched {
            inner: io,
            activity: activity.clone(),
        };
        let mut builder = auto::Builder::new(TokioExecutor::new());
        builder
            .http1()
            .timer(TokioTimer::new())
            .header_read_timeout(self.limits.header_read_timeout);
        builder.http2().timer(TokioTimer::new());
        let connection =
            builder.serve_connection(TokioIo::new(io), TowerToHyperService::new(self.router));
        tokio::pin!(connection);
        let mut draining = false;
        loop {
            tokio::select! {
                served = connection.as_mut() => {
                    if let Err(error) = served {
                        tracing::debug!(%peer, "{what} connection failed: {error}");
                    }
                    return;
                }
                () = self.shutdown.started(), if !draining => {
                    // Finishes the request in flight, if any, and closes the connection.
                    connection.as_mut().graceful_shutdown();
                    draining = true;
                }
                () = self.closing.started() => return,
                () = activity.idle_for(self.limits.idle_timeout) => {
                    tracing::debug!(%peer, "closing an idle {what} connection");
                    return;
                }
            }
        }
    }
}

/// When a connection last sent or received anything.
struct Activity {
    started: Instant,
    /// Milliseconds since `started`.
    last: AtomicU64,
}

impl Activity {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last: AtomicU64::new(0),
        }
    }

    fn touch(&self) {
        let now = self.started.elapsed().as_millis() as u64;
        self.last.store(now, Ordering::Relaxed);
    }

    /// Completes once nothing was sent or received for `timeout`, never without one.
    async fn idle_for(&self, timeout: Option<Duration>) {
        let Some(timeout) = timeout else {
            return std::future::pending().await;
        };
        loop {
            let last = self.started + Duration::from_millis(self.last.load(Ordering::Relaxed));
            let deadline = last + timeout;
            if Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep_until(deadline.into()).await;
        }
    }
}

/// IO that records its activity.
struct Watched<I> {
    inner: I,
    activity: Arc<Activity>,
}

impl<I: AsyncRead + Unpin> AsyncRead for Watched<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.activity.touch();
        }
        polled
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for Watched<I> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write(cx, buf);
        if matches!(polled, Poll::Ready(Ok(written)) if written > 0) {
            self.activity.touch();
        }
        polled
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        if matches!(polled, Poll::Ready(Ok(written)) if written > 0) {
            self.activity.touch();
        }
        polled
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
