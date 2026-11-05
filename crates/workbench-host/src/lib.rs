//! Core-side handling of the evorch workbench protocol (ADR 0045).
//!
//! - [`commands`]: the command services that act on the runtime, shared by every
//!   ingress.
//! - [`ingress`]: [`CommandHost::handle_command`], the protocol entry point.
//! - [`receipts`]: command receipts and submission deduplication.
//! - [`projects`]: project id to project root resolution.
//! - [`provenance`]: the provenance the in-process paths attach.
//!
//! This crate depends on the protocol and on core crates only. The boundary
//! check (`scripts/check-workbench-boundaries.sh`) keeps GUI and client crates
//! out of its dependency graph.

pub mod commands;
pub mod ingress;
pub mod lane;
pub mod projects;
pub mod provenance;
pub mod receipts;

pub use ingress::CommandHost;
