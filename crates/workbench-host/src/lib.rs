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

// The renderer helper's IPC is a dependency of the host from PR 16a-2's
// integration PR on, so the later PRs (the renderer service, the artifact
// adapters) need no manifest or lockfile change. Nothing uses it yet.
use renderer_ipc as _;

pub mod admin;
pub mod areas;
pub mod attachments;
pub mod bootstrap;
pub mod commands;
pub mod connections;
pub mod ingress;
pub mod invocations;
pub mod lane;
pub mod lane_delivery;
pub mod lane_management;
pub mod projection;
pub mod projects;
pub mod provenance;
pub mod queries;
pub mod receipts;
pub mod scratch_archive;
pub mod subscriptions;
pub mod thread_retention;

pub use ingress::CommandHost;
/// The Q1 leaf, built and registered by the composition root.
pub use ingress::namespaces::lifecycle::quarantine::{QuarantineFacade, StorageQuarantineStore};
