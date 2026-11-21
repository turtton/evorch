//! Quarantine and owned-graphs leaves. Each is registered by its own module.
use super::Inputs;

mod owned_graphs;
mod quarantine;
#[cfg(test)]
pub(super) mod support;

pub(super) fn install(inputs: &Inputs<'_>) {
    quarantine::install(inputs);
    owned_graphs::install(inputs);
}
