//! Standalone composition root and transports (ADR 0045 P7a).
pub mod backend;
pub mod instance;
pub mod lifecycle;
pub mod restart;
// Only the core bootstrap installs graph recovery; embedded hosts remain legacy.
pub mod recovery_service;
pub mod recovery_startup;
pub mod unix;
