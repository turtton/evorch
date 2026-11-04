//! Client for the evorch workbench protocol (ADR 0045).
//!
//! [`WorkbenchClient`] speaks [`workbench_protocol`] over any [`Transport`]:
//! a Unix domain socket in production ([`UnixTransport`]) and an in-memory
//! pipe with a scriptable fake core in tests ([`fake`]).
//!
//! Guarantees:
//!
//! - The first frame is the handshake; nothing else is sent before the core accepts.
//! - One task writes, one task reads. Replies are matched to requests by id, in
//!   any order. A reply with no matching request closes the connection.
//! - Subscription events are multiplexed by subscription id, deduplicated by
//!   cursor and dropped at or before the snapshot boundary. A subscriber that
//!   falls behind gets `ResyncRequired` and is stopped.
//! - Dropping a request future does not cancel the request. A command whose
//!   reply is lost to a disconnect reports [`TransportError::OutcomeUnknown`]
//!   and is never resent automatically.
//!
//! Starting or restarting the core is out of scope here: this layer only
//! reports the connection state ([`ConnectionState`]).

mod client;
mod error;
pub mod fake;
mod subscription;
mod transport;

pub use client::{ClientConfig, ConnectionState, WorkbenchClient};
pub use error::{ClientError, TransportError};
pub use subscription::{Subscription, SubscriptionEnd};
pub use transport::{FrameSink, FrameSource, Transport, UnixSink, UnixSource, UnixTransport};
