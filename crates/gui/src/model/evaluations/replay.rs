//! Reading back the conversation of a finished evaluation trial.
//!
//! A trial keeps its own database (`evorch.db`). It is opened read-only, never
//! created, migrated or written, and its events are projected into a
//! [`TranscriptRegistry`] that belongs to this replay alone: nothing here
//! touches the Workbench's transcripts, runs, phases or usage. A database the
//! current schema cannot open read-only (an older one) is reported as such and
//! the trial's `conversation.json` is offered instead.
