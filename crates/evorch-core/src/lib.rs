//! Standalone composition root and transports (ADR 0045 P7a).
pub mod backend;
pub mod instance;
pub mod lifecycle;
// Explicit staged APIs only: production main does not run graph recovery yet.
pub mod recovery_startup;
pub mod unix;
