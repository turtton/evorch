//! Quarantine and owned-graphs leaves. Each is registered by its own module.
use super::Inputs;

mod quarantine;

pub(super) fn install(inputs: &Inputs<'_>) {
    quarantine::install(inputs);
}
