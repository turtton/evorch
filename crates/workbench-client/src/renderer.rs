//! Renderer data plane client (ADR 0045 stage 16a-2, F3).
//!
//! The control connection ([`crate::WorkbenchClient`]) carries only references
//! and receipts. The bytes of a renderer frame, a Capture PNG or an image
//! artifact travel on a dedicated connection that this module opens, binds to
//! the control connection and reads with bounded buffers. The wiring lands
//! first so that the leaf implementation does not touch `lib.rs`.
