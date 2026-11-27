//! Standalone composition root and transports (ADR 0045 P7a).
pub mod admin_composition;
pub mod allocator;
pub mod backend;
pub(crate) mod connection_log;
#[cfg(target_os = "linux")]
pub mod handoff_description;
pub mod instance;
pub mod lifecycle;
pub mod logging;
#[cfg(target_os = "linux")]
pub mod planned_bootstrap;
pub mod restart;
#[cfg(all(test, target_os = "linux"))]
#[path = "../shared-test-support/signal_entry.rs"]
pub(crate) mod signal_test_entry;
// Only the core bootstrap installs graph recovery; embedded hosts remain legacy.
pub mod recovery_admin;
pub mod recovery_service;
pub mod recovery_startup;
pub(crate) mod renderer;
pub mod unix;
