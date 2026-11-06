use crate::assets::StaticAsset;
use crate::security::Shared;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use workbench_protocol::handshake::WireLimits;

/// Handler state: the backend plus the settings the handlers read.
pub(crate) struct AppState<B> {
    pub(crate) backend: Arc<B>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) wire_limits: WireLimits,
    pub(crate) sse_keepalive: Duration,
    pub(crate) max_request_bytes: usize,
    pub(crate) assets: &'static [StaticAsset],
    client_counter: AtomicU64,
}

impl<B> AppState<B> {
    pub(crate) fn new(
        backend: Arc<B>,
        shared: Arc<Shared>,
        config: &crate::config::WebServerConfig,
        assets: &'static [StaticAsset],
    ) -> Self {
        Self {
            backend,
            shared,
            wire_limits: config.wire_limits,
            sse_keepalive: config.sse_keepalive,
            max_request_bytes: config.max_request_bytes,
            assets,
            client_counter: AtomicU64::new(1),
        }
    }

    pub(crate) fn next_client_number(&self) -> u64 {
        self.client_counter.fetch_add(1, Ordering::Relaxed)
    }
}
