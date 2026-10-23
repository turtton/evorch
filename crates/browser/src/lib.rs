//! The agent browser (ADR 0034): Chromium driven over CDP for the agent
//! browser tools, kept free of GUI types so the GUI, runtime and tools can
//! share it.

pub mod agent;
mod jpeg;
