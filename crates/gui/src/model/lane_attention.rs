//! What the GUI keeps of the lane ledger's attention list (ADR 0044 Phase 2).
//!
//! The list is derived by storage from the lane ledger; this board is only the last answer,
//! held per lead in the wire form. It makes no decision: it has no timer, never marks an item
//! handled, and an item leaves it only when a later read no longer reports it. Notifications
//! are a separate model, so clearing or acknowledging them changes nothing here.
//!
//! Reads can finish out of order, and other windows write the same ledger. Every read carries
//! the number it was started with, and an answer that is not newer than the one already held is
//! refused, so an old projection never replaces a newer one. An action is only run for an item
//! the latest answer still holds, unchanged.

pub mod wire;
