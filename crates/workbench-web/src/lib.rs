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
//! | `GET /api/v1/health` | adapter version and the core's `ServerHello` (build, protocol version, capabilities, epoch, status) |
//! | `GET /` and assets | embedded front end: the web workbench ([`UI_ASSETS`]) by default |
//!
//! # Boundaries
//!
//! - **Backend.** The core binary implements [`WorkbenchBackend`] and passes
//!   it to [`WebServer::start`]. This crate depends on no core crate.
//! - **Provenance.** The ingress attaches `Human` × `Web`; a request body that
//!   names a provenance is refused (the protocol rejects unknown fields).
//! - **Secret masking.** The backend owns the masking policy. Every JSON
//!   response and SSE data item, including adapter-generated output, passes
//!   through [`WorkbenchBackend::protect_output`] at final serialization.
//!   This crate does not depend on `secret-guard` or define masking rules.
//! - **Network.** Only loopback addresses can be bound. Remote access is
//!   Tailscale Serve in front of the loopback port, whose name must be added
//!   to [`WebServerConfig::allowed_hosts`]. There is no authentication of
//!   users; the browser-facing checks are listed in the `security` module.
//! - **Slow clients.** A request head, a request body and an idle keep-alive
//!   connection each have a deadline ([`WebServerConfig::header_read_timeout`],
//!   [`WebServerConfig::body_read_timeout`]); shutdown stops waiting for open
//!   connections after [`WebServerConfig::shutdown_grace`] without interrupting
//!   a command the backend already accepted.
//! - **Errors.** [`http_error`] documents the status mapping. Internal error
//!   text never reaches the browser.

mod api;
pub mod assets;
pub mod backend;
pub mod config;
pub mod http_error;
mod output;
mod security;
mod server;
pub mod shutdown;
pub mod sse;
mod state;

pub use assets::{StaticAsset, UI_ASSETS};
pub use backend::{BackendSubscription, WebSession, WorkbenchBackend};
pub use config::{ConfigError, DEFAULT_PORT, LOOPBACK_HOSTS, WebServerConfig};
pub use server::{WebApp, WebServer, WebServerError};
pub use shutdown::ShutdownSignal;
