//! Web adapter of the evorch core: HTTP + Server-Sent Events (ADR 0045 §4, §5).
//!
//! The browser UI is a client of the same protocol as the desktop GUI. This
//! crate carries `workbench-protocol` values over HTTP and decides nothing
//! about their meaning:
//!
//! | Endpoint | Purpose |
//! |---|---|
//! | `POST /api/v1/request` | a `RequestEnvelope` (command or query) in, a `ReplyEnvelope` out |
//! | `GET /api/v1/events?scope=..&after=..` | SSE: `subscription_started`, then `ui_event` messages |
//! | `GET /api/v1/health` | core status |
//! | `GET /` and assets | embedded front end ([`StaticAsset`]) |
//!
//! # Boundaries
//!
//! - **Backend.** The core binary implements [`WorkbenchBackend`] and passes
//!   it to [`WebServer::start`]. This crate depends on no core crate.
//! - **Provenance.** The ingress attaches `Human` × `Web`; a request body that
//!   names a provenance is refused (the protocol rejects unknown fields).
//! - **Secret masking.** Replies and events are relayed unchanged. Masking
//!   with `secret-guard` is done by the backend before it returns a value;
//!   this crate does not depend on `secret-guard` and never inspects text.
//! - **Network.** Only loopback addresses can be bound. Remote access is
//!   Tailscale Serve in front of the loopback port, whose name must be added
//!   to [`WebServerConfig::allowed_hosts`]. There is no authentication of
//!   users; the browser-facing checks are listed in the `security` module.
//! - **Errors.** [`http_error`] documents the status mapping. Internal error
//!   text never reaches the browser.

mod api;
pub mod assets;
pub mod backend;
pub mod config;
pub mod http_error;
mod security;
mod server;
pub mod shutdown;
pub mod sse;
mod state;

pub use assets::{PLACEHOLDER_ASSETS, StaticAsset};
pub use backend::{BackendSubscription, WebSession, WorkbenchBackend};
pub use config::{ConfigError, DEFAULT_PORT, LOOPBACK_HOSTS, WebServerConfig};
pub use server::{WebApp, WebServer, WebServerError};
pub use shutdown::ShutdownSignal;
