//! Desktop side of the public restart: what the screen shows and what a click
//! may send. The model is plain data (no egui, no connection), so a test drives
//! it by feeding replies and failures.
//!
//! * A restart or an interrupt is only ever sent from a pending confirmation
//!   the person accepted, and the confirmation is dropped when the target it
//!   names changed, so an old dialog cannot run against a new target.
//! * A command is sent once. After a lost reply or a restart, the panel asks
//!   for the operation by its id ([`panel::RestartPanel::reconcile_queries`]);
//!   it never produces a command for that.
//! * The reconnect banner carries the stage and the deadline; the deadlines
//!   themselves are the pure machine's in `workbench_client::restart`.
//!
//! `features/restart.rs` wires this model to the shared connection and the window.
//! The connection actor (`connection.rs`) owns the socket: after the core's own
//! evidence of a restart it waits for the successor connect-only and reads the
//! new session inside the 30 second window; it never starts a core meanwhile.
pub mod panel;
pub mod text;

#[cfg(test)]
mod tests;
