//! The readable text of a call's arguments while the JSON is still incomplete.
//!
//! A delegate's `prompt` or a shell's `command` is a long string that arrives in
//! pieces. The card shows that string itself (line breaks kept) instead of the
//! JSON around it. The input is the newest part of the raw argument text, so
//! parsing is deliberately loose: it finds the last `"field":"` opening, reads
//! up to the closing quote or the end of what has arrived, and decodes the JSON
//! escapes it recognises. Nothing here redacts; callers redact the result.
