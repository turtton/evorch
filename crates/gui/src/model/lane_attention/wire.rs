//! The boundary adapter between the lane ledger's domain values (`event_bus::lane`) and the
//! wire read model (`workbench_protocol::lane_attention`). Every field is mapped by name; a
//! value the wire cannot hold is an error, never silently dropped.
//!
//! The adapter sits in the client crate for now: core crates must not depend on the wire
//! crate, and the core-side host crate that will own it is not in `main` yet. It is a pure
//! function so it can move there unchanged.
