//! Read-only thread tools (ADR 0041): `thread_search`, `thread_grep` and `thread_read`.
//!
//! Every target passes [`visibility::resolve`] before any of its records are read.
//! Conversation text is normalized and redacted before it is matched or returned,
//! so a pattern can never probe a secret.

pub(super) mod normalize;
pub(super) mod visibility;
