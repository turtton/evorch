//! Bodies that stay in the store until something shows them (ADR: lazy bodies).
//!
//! A stored event may name its big text by a token instead of holding it. The
//! GUI keeps such text as a reference ([`BodyText`]), draws a placeholder in
//! its place, and reads it on a worker ([`BodyResolver`]). A reference is never
//! joined into a string and never shown.

pub mod backend;
pub mod clock;
pub mod meter;
pub mod parts;
pub mod resolver;
pub mod rows;
pub mod source;
pub mod split;
pub mod storage_backend;
#[doc(hidden)]
pub mod testing;
pub mod text;
pub mod types;
pub mod ui;

pub use backend::{
    BodyBackend, BodyResult, DeferredEvent, EventFailure, FailureKind, HistoryRow, NoBodies,
};
pub use clock::{Clock, ManualClock, SystemClock};
pub use meter::{ClassStats, ImmediateClass, ImmediateMeter, ImmediateStats};
pub use parts::{Accepted, DeferredPartAssembler};
pub use resolver::{
    BodyFailure, BodyPin, BodyResolver, BodyView, EventPhase, FailureReport, PinState,
    ResolverConfig, ResolverStats,
};
pub use rows::{RowsOutcome, RowsPurpose, RowsRequest};
pub use source::{HistorySource, StorageHistory};
pub use storage_backend::StorageBodies;
pub use text::{BodyText, Segment};
pub use types::{BodyError, BodyErrorKind, BodyOrigin, BodyRef, DeferredBody, TOKEN_ESCAPE};

#[cfg(test)]
mod resolver_tests;
