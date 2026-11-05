//! Static bounds every peer enforces regardless of the negotiated
//! [`WireLimits`](crate::handshake::WireLimits).
//!
//! The text limits are chosen so the largest valid command still fits the
//! default frame once JSON escaping (at most 2x, because control characters
//! other than `\t`, `\n` and `\r` are rejected) is applied. A test pins that.

/// Longest identifier, in bytes.
pub const MAX_ID_BYTES: usize = 128;
/// Longest short name (profile, model, skill, role label, tool name), in bytes.
pub const MAX_NAME_BYTES: usize = 256;
/// Longest single-line title or summary, in bytes.
pub const MAX_TITLE_BYTES: usize = 1024;
/// Longest chat message or answer, in bytes.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// Longest goal objective, in bytes.
pub const MAX_OBJECTIVE_BYTES: usize = 128 * 1024;
/// Longest inline message in a committed-history page; longer ones use a `BodyRef`.
pub const MAX_INLINE_MESSAGE_BYTES: usize = 16 * 1024;
/// Most skill names on one chat submission.
pub const MAX_SKILLS: usize = 32;
/// Most image references on one chat submission.
pub const MAX_IMAGES: usize = 16;
/// Most packet/issue references on one goal.
pub const MAX_REFERENCES: usize = 32;
/// Longest packet/issue reference value, in bytes.
pub const MAX_REFERENCE_BYTES: usize = 512;
/// Most constraints on one goal.
pub const MAX_CONSTRAINTS: usize = 32;
/// Longest single constraint, in bytes.
pub const MAX_CONSTRAINT_BYTES: usize = 2048;
/// Most options on one question.
pub const MAX_OPTIONS: usize = 32;
/// Longest single question option, in bytes.
pub const MAX_OPTION_BYTES: usize = 1024;
/// Longest media type, in bytes.
pub const MAX_MIME_BYTES: usize = 127;
/// Most items a client may request, and a server may return, in one page.
pub const MAX_PAGE_SIZE: u32 = 200;
/// Page size used when a client has no preference.
pub const DEFAULT_PAGE_SIZE: u32 = 50;
/// Smallest body chunk a client may request; large enough for any UTF-8 scalar
/// so a reader always makes progress.
pub const MIN_BODY_CHUNK_BYTES: u32 = 4;
/// Largest frame any peer accepts, whatever the other side advertises.
pub const HARD_MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;
/// Default and largest advertised body chunk.
pub const DEFAULT_BODY_CHUNK_BYTES: u32 = 64 * 1024;
/// Default maximum frame.
pub const DEFAULT_FRAME_BYTES: u32 = 1024 * 1024;
