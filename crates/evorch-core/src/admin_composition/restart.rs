//! Restart coordinator leaf. Registered by its own change.
use super::Inputs;
pub mod recipes;
pub mod targets;

pub(super) fn install(_inputs: &Inputs<'_>) {}
