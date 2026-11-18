//! Seam for the dedicated renderer frame connection of the local UDS.
//!
//! `unix::connection` asks `claims` about the first frame of a connection before
//! it treats the frame as a `ClientHello`. The integration PR leaves the answer
//! fixed at `false`, so every connection keeps the control-plane handling it had;
//! the frame-delivery PR replaces this leaf with the real frame connection and
//! changes no other file for the hand-off.

/// Whether the first frame opens a renderer frame connection instead of a
/// control connection.
pub(crate) fn claims(_first_frame: &[u8]) -> bool {
    false
}
