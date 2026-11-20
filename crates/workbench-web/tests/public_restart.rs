//! CW: the web adapter's management contract over HTTP and SSE.
//!
//! `contracts` (real HTTP, no authentication) is the production evidence: all
//! nine management operations and every management SSE path are refused.
//! `management` and `stream` use a scripted facade and a contract-only fake
//! principal for the positive path; they prove the adapter and browser
//! contract, never that the web has working authentication.
// `contracts` (owned by the integration PR) loads `common` itself.
#[allow(clippy::duplicate_mod)]
mod common;
#[path = "public_restart_contracts.rs"]
mod contracts;
#[path = "public_restart/management.rs"]
mod management;
#[path = "public_restart/stream.rs"]
mod stream;
#[path = "public_restart/support.rs"]
mod support;
