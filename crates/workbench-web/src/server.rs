//! Router construction, the listener and the start/stop API.

use crate::api;
use crate::assets::{PLACEHOLDER_ASSETS, StaticAsset};
use crate::backend::WorkbenchBackend;
use crate::config::{ConfigError, WebServerConfig};
use crate::security::{Shared, guard};
use crate::shutdown::ShutdownSignal;
use crate::sse;
use crate::state::AppState;
use axum::Router;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use axum::serve::Listener;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

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
    /// Build the application with the placeholder front end.
    pub fn new<B: WorkbenchBackend>(
        config: &WebServerConfig,
        backend: Arc<B>,
    ) -> Result<Self, ConfigError> {
        Self::with_assets(config, backend, PLACEHOLDER_ASSETS)
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
            sse_slots: Arc::new(Semaphore::new(config.max_sse_streams)),
            shutdown: shutdown.clone(),
        });
        let state = Arc::new(AppState::new(backend, Arc::clone(&shared), config, assets));
        let router = Router::new()
            .route("/api/v1/request", post(api::request::<B>))
            .route("/api/v1/events", get(sse::events::<B>))
            .route("/api/v1/health", get(api::health::<B>))
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
    /// Start serving the placeholder front end.
    ///
    /// Returns `Ok(None)` without binding anything when `config.enabled` is
    /// false. An enabled configuration is validated first; a bind address that
    /// is not loopback is an error, never replaced by another address.
    pub async fn start<B: WorkbenchBackend>(
        config: WebServerConfig,
        backend: Arc<B>,
    ) -> Result<Option<Self>, WebServerError> {
        Self::start_with_assets(config, backend, PLACEHOLDER_ASSETS).await
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
        let stop = shutdown.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app.router())
                .with_graceful_shutdown(async move { stop.triggered().await })
                .await
        });
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

/// A TCP listener that closes connections beyond `max_connections`.
struct LimitedListener {
    inner: TcpListener,
    slots: Arc<Semaphore>,
}

impl Listener for LimitedListener {
    type Io = LimitedStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
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

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
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
