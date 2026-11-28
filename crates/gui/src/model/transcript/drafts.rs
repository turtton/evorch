//! Tool calls the model is still composing.
//!
//! `ToolCallDelta` fragments are display-only: a draft is a `Tool` entry in
//! `ToolStatus::Drafting` that `ToolStarted` converts in place, so the card keeps
//! its position. Anything the accepted response never turned into a call is
//! discarded rather than left as a card that can never finish.
//!
//! Drafts are tracked by (run, call index) with a stable entry ID, so they are
//! found, confirmed and discarded however far the transcript has since grown.

/// Raw argument text kept per draft so a credential split across fragments is
/// still recognised. This is the only unredacted copy and never leaves the
/// model; it is strictly bounded, so a draft holds at most this much raw text.
pub(super) const RAW_WINDOW_BYTES: usize = 64 * 1024;
/// Redacted tail stored in the entry and handed to the renderer, once for the
/// argument JSON and once for the decoded text.
pub(super) const PREVIEW_BYTES: usize = 16 * 1024;
/// A window whose start was cut shows nothing within the longest credential
/// length of that start (see `shown_tail`). That distance, at least the
/// designed `MAX_CREDENTIAL_BYTES`, must leave room for a whole preview.
const _: () = assert!(RAW_WINDOW_BYTES - PREVIEW_BYTES >= secret_guard::MAX_CREDENTIAL_BYTES);
