//! Standalone composition root and transports (ADR 0045 P7a).
pub mod backend;
pub mod instance;
pub mod lifecycle;
#[cfg(target_os = "linux")]
pub mod planned_bootstrap;
pub mod restart;
#[cfg(all(test, target_os = "linux"))]
#[path = "../shared-test-support/signal_entry.rs"]
pub(crate) mod signal_test_entry;
// Only the core bootstrap installs graph recovery; embedded hosts remain legacy.
pub mod recovery_service;
pub mod recovery_startup;
pub mod unix;
