//! The single place that publishes management.
//!
//! `MANAGEMENT_PUBLISHED` is the only product switch: there is no environment
//! variable or configuration key. Tests that assemble an in-process host use the
//! test-only `publish_for_test` of `AdminService` instead.
use crate::backend::HostBackend;
use workbench_host::admin::{AdminService, InstalledFacades};

/// Flipped to `true` only by the change that publishes management.
const MANAGEMENT_PUBLISHED: bool = false;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publication {
    /// The switch is off: management stays refused and unadvertised.
    Off,
    /// The switch is on but a leaf is missing or the platform cannot run the
    /// effects: still refused and unadvertised.
    Blocked(InstalledFacades),
    Published,
}

pub(super) fn publish(admin: &AdminService, backend: &HostBackend) -> Publication {
    publish_with(
        MANAGEMENT_PUBLISHED,
        cfg!(target_os = "linux"),
        admin,
        backend,
    )
}

fn publish_with(
    published: bool,
    linux: bool,
    admin: &AdminService,
    backend: &HostBackend,
) -> Publication {
    if !published {
        return Publication::Off;
    }
    let installed = admin.installed_facades();
    if !linux || !installed.all() {
        return Publication::Blocked(installed);
    }
    admin.enable_management();
    backend.advertise_management();
    Publication::Published
}

#[cfg(test)]
mod tests;
