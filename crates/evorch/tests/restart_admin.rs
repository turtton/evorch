//! The management CLI against a core over a real Unix socket: a lost ACK, a
//! restart to a new epoch, old cursors, no core running, Ctrl-C, confirmations
//! and conflicts. The tests wait on what the core saw or the CLI printed,
//! never on time; the 120 second window runs on the paused clock.
#![cfg(target_os = "linux")]

#[path = "restart_admin/fake_core.rs"]
mod fake_core;
#[path = "restart_admin/flows.rs"]
mod flows;
#[path = "restart_admin/harness.rs"]
mod harness;
#[path = "restart_admin/recovery_flows.rs"]
mod recovery_flows;
