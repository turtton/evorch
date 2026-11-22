//! Shared restart client seams; no implicit replay or resume.
//!
//! Everything here is surface independent (CLI and desktop use it) and holds
//! no clock, no task and no connection of its own:
//!
//! * [`confirmation`] – the proof every irreversible command needs.
//! * [`intent`] – turns what the person saw (targets, status, blockers) into a
//!   command bound to exactly that, and refuses a binding that is already stale.
//! * [`recovery`] – Q1/Q2 discard, release and resume, bound to what was shown.
//! * [`reconnect`] – a pure state machine: connection loss is a *planned*
//!   restart only on evidence from the core, never from EOF alone.
//! * [`resume`] – after a restart the old cursor is discarded, a snapshot is
//!   read, and only then is a new subscription followed.
//! * [`session`] – typed wire calls. A command is sent once and never again.
//! * [`view`] – the wording of results, conflicts, audit trouble and the
//!   reconnect state with its deadline.
pub mod confirmation;
pub mod intent;
pub mod reconnect;
pub mod recovery;
pub mod resume;
pub mod session;
pub mod view;
