//! Bodies that stay in the store until something shows them (ADR: lazy bodies).
//!
//! A stored event may name its big text by a token instead of holding it. The
//! GUI keeps such text as a reference ([`BodyText`]), draws a placeholder in
//! its place, and reads it on a worker ([`BodyResolver`]). A reference is never
//! joined into a string and never shown.

pub mod backend;
pub mod clock;
pub mod parts;
pub mod resolver;
pub mod source;
pub mod split;
pub mod text;
pub mod types;
pub mod ui;

pub use backend::{
    BodyBackend, BodyResult, DeferredEvent, EventFailure, EventReread, FailureKind, HistoryRow,
    NoBodies,
};
pub use clock::{Clock, ManualClock, SystemClock};
pub use parts::{Accepted, DeferredPartAssembler};
pub use resolver::{
    BodyFailure, BodyPin, BodyResolver, BodyView, EventPhase, FailureReport, PinState,
    ResolverConfig, ResolverStats,
};
pub use source::{HistorySource, StorageHistory};
pub use text::{BodyText, Segment};
pub use types::{BodyError, BodyErrorKind, BodyOrigin, BodyRef, DeferredBody, TOKEN_ESCAPE};
