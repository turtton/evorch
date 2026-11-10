//! Wire protocol between the evorch core and its clients (ADR 0045).
//!
//! The desktop GUI, the web adapter and the CLI all speak this protocol, so it
//! is deliberately small and isolated:
//!
//! - No workspace dependencies and no runtime or storage types. Anything that
//!   crosses the wire is a type defined here.
//! - UTF-8 JSON. Enums are `{"type": .., "payload": ..}` in snake_case.
//!   `u64` values (revisions, sequences, offsets, epochs) are decimal strings
//!   because JavaScript cannot hold them exactly.
//! - Types model what may be said, not what a core implements. A capability
//!   listed in [`handshake::Capability`] is only offered when the core's
//!   [`handshake::ServerHello`] says so.
//!
//! Frame layout is in [`framing`], the connection handshake in [`handshake`],
//! request and reply envelopes in [`envelope`].

pub mod commands;
pub mod conversation;
pub mod envelope;
pub mod errors;
pub mod events;
pub mod framing;
pub mod handshake;
pub mod ids;
pub mod lane_attention;
pub mod lane_management;
pub mod limits;
pub mod queries;
pub mod validation;
pub mod views;
pub mod wire;

pub use errors::{ErrorCode, RetryAdvice, WorkbenchError};
pub use handshake::{Capability, ProtocolVersion};
