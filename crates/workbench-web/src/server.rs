//! Router construction, the listener and the start/stop API.

use crate::api;
use crate::assets::{StaticAsset, UI_ASSETS};
use crate::backend::WorkbenchBackend;
use crate::config::{ConfigError, WebServerConfig};
use crate::security::{Shared, guard};
use crate::shutdown::ShutdownSignal;
use crate::sse;
use crate::state::AppState;
use axum::Router;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinHandle, JoinSet};

/// Why the server could not start or stopped abnormally.
#[derive(Debug, thiserror::Error)]
pub enum WebServerError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("cannot bind the web listener: {0}")]
    Bind(#[source] io::Error),
    #[error("the web listener bound to a non-loopback address {0}")]
    NotLoopback(SocketAddr),
    #[error("the web server failed: {0}")]
    Serve(#[source] io::Error),
    #[error("the web server task panicked")]
    Panicked,
}

/// The HTTP application without a socket, for embedding and tests.
pub struct WebApp {
    router: Router,
    shutdown: ShutdownSignal,
}

impl WebApp {
    /// Build the application with the web workbench ([`UI_ASSETS`]).
    pub fn new<B: WorkbenchBackend>(
        config: &WebServerConfig,
        backend: Arc<B>,
    ) -> Result<Self, ConfigError> {
        Self::with_assets(config, backend, UI_ASSETS)
    }

    pub fn with_assets<B: WorkbenchBackend>(
        config: &WebServerConfig,
        backend: Arc<B>,
        assets: &'static [StaticAsset],
    ) -> Result<Self, ConfigError> {
        let policy = config.policy()?;
        let shutdown = ShutdownSignal::new();
        let shared = Arc::new(Shared {
            policy,
            in_flight: Arc::new(Semaphore::new(config.max_in_flight_requests)),
            backend_slots: Arc::new(Semaphore::new(config.max_in_flight_requests)),
            sse_slots: Arc::new(Semaphore::new(config.max_sse_streams)),
            shutdown: shutdown.clone(),
        });
        let state = Arc::new(AppState::new(backend, Arc::clone(&shared), config, assets));
        let router = Router::new()
            .route(
                "/api/v1/request",
                post(api::request::<B>).fallback(api::method_not_allowed("POST")),
            )
            .route(
                "/api/v1/events",
                get(sse::events::<B>).fallback(api::method_not_allowed("GET")),
            )
            .route(
                "/api/v1/health",
                get(api::health::<B>).fallback(api::method_not_allowed("GET")),
            )
            .fallback(api::fallback::<B>)
            .with_state(state)
            .layer(from_fn_with_state(shared, guard));
        Ok(Self { router, shutdown })
    }

    /// The router, ready to be served or driven with `tower::ServiceExt`.
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    pub fn shutdown_signal(&self) -> ShutdownSignal {
        self.shutdown.clone()
    }
}

/// A running web server.
///
/// Dropping the handle starts the same graceful shutdown as
/// [`WebServer::shutdown`] but does not wait for it.
pub struct WebServer {
    local_addr: SocketAddr,
    shutdown: ShutdownSignal,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl WebServer {
    /// Start serving the web workbench ([`UI_ASSETS`]).
    ///
    /// Returns `Ok(None)` without binding anything when `config.enabled` is
    /// false. An enabled configuration is validated first; a bind address that
    /// is not loopback is an error, never replaced by another address.
    pub async fn start<B: WorkbenchBackend>(
        config: WebServerConfig,
        backend: Arc<B>,
    ) -> Result<Option<Self>, WebServerError> {
        Self::start_with_assets(config, backend, UI_ASSETS).await
    }

    pub async fn start_with_assets<B: WorkbenchBackend>(
        config: WebServerConfig,
        backend: Arc<B>,
        assets: &'static [StaticAsset],
    ) -> Result<Option<Self>, WebServerError> {
        if !config.enabled {
            return Ok(None);
        }
        let app = WebApp::with_assets(&config, backend, assets)?;
        let listener = TcpListener::bind(config.bind)
            .await
            .map_err(WebServerError::Bind)?;
        let local_addr = listener.local_addr().map_err(WebServerError::Bind)?;
        if !local_addr.ip().is_loopback() {
            return Err(WebServerError::NotLoopback(local_addr));
        }
        let listener = LimitedListener {
            inner: listener,
            slots: Arc::new(Semaphore::new(config.max_connections)),
        };
        let shutdown = app.shutdown_signal();
        let task = tokio::spawn(serve(
            listener,
            app.router(),
            ServeSettings {
                header_read_timeout: config.header_read_timeout,
                shutdown_grace: config.shutdown_grace,
            },
            shutdown.clone(),
        ));
        Ok(Some(Self {
            local_addr,
            shutdown,
            task: Some(task),
        }))
    }

    /// The address actually bound (useful with port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stop accepting connections, end every SSE stream and wait until the
    /// server has finished its in-flight requests.
    pub async fn shutdown(mut self) -> Result<(), WebServerError> {
        self.shutdown.trigger();
        match self.task.take() {
            Some(task) => match task.await {
                Ok(result) => result.map_err(WebServerError::Serve),
                Err(_) => Err(WebServerError::Panicked),
            },
            None => Ok(()),
        }
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        self.shutdown.trigger();
    }
}

struct ServeSettings {
    header_read_timeout: Duration,
    shutdown_grace: Duration,
}

/// Accept connections until shutdown, then drain them.
///
/// Draining waits `shutdown_grace` for open connections to finish, then drops
/// the rest. Dropping a connection cancels its HTTP handler only: a command the
/// backend has accepted runs in its own task and is not interrupted.
async fn serve(
    mut listener: LimitedListener,
    router: Router,
    settings: ServeSettings,
    shutdown: ShutdownSignal,
) -> io::Result<()> {
    let service = TowerToHyperService::new(router);
    let mut connections: JoinSet<()> = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            () = shutdown.triggered() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            (stream, _peer) = listener.accept() => {
                connections.spawn(serve_connection(
                    stream,
                    service.clone(),
                    settings.header_read_timeout,
                    shutdown.clone(),
                ));
            }
        }
    }
    // Stop accepting before draining so no new connection can slip in.
    drop(listener);
    let drained = tokio::time::timeout(settings.shutdown_grace, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    Ok(())
}

async fn serve_connection(
    stream: LimitedStream,
    service: TowerToHyperService<Router>,
    header_read_timeout: Duration,
    shutdown: ShutdownSignal,
) {
    let connection = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout)
        .keep_alive(true)
        .serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);
    tokio::select! {
        _ = connection.as_mut() => return,
        () = shutdown.triggered() => connection.as_mut().graceful_shutdown(),
    }
    let _ = connection.await;
}

/// A TCP listener that closes connections beyond `max_connections`.
struct LimitedListener {
    inner: TcpListener,
    slots: Arc<Semaphore>,
}

impl LimitedListener {
    async fn accept(&mut self) -> (LimitedStream, SocketAddr) {
        loop {
            let (stream, addr) = match self.inner.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    // Resource exhaustion (EMFILE) would otherwise spin.
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
                    ) {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    continue;
                }
            };
            match Arc::clone(&self.slots).try_acquire_owned() {
                Ok(permit) => {
                    let _ = stream.set_nodelay(true);
                    return (
                        LimitedStream {
                            stream,
                            _permit: permit,
                        },
                        addr,
                    );
                }
                Err(_) => drop(stream),
            }
        }
    }
}

/// A connection that holds its slot until it is dropped.
struct LimitedStream {
    stream: TcpStream,
    _permit: OwnedSemaphorePermit,
}

impl AsyncRead for LimitedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for LimitedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
