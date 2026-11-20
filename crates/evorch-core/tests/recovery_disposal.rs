//! Closed public reasons do not leak implementation/storage diagnostics.
#[test]
fn every_startup_blocked_reason_maps_to_the_closed_public_vocabulary() {
    use evorch_core::{recovery_admin::map_startup_blocked, recovery_startup::StartupBlocked as S};
    use workbench_host::bootstrap::core_startup::RecoveryAdminError;
    use workbench_protocol::namespaces::lifecycle::owned_graphs::{
        GraphAuthorityProblem as P, GraphProblemDetail as D,
    };
    for (input, problem, detail) in [
        (S::OwnerBlocked, P::OwnerBlocked, D::OwnerAbsent),
        (
            S::CurrentBindingRequired,
            P::CurrentBindingRequired,
            D::CurrentBindingRequired,
        ),
        (
            S::CurrentSettingsRequired,
            P::CurrentSettingsRequired,
            D::CurrentSettingsRequired,
        ),
        (
            S::CliContinuityRequired,
            P::CliContinuityRequired,
            D::CliContinuityRequired,
        ),
        (
            S::TeamReconciliationRequired,
            P::TeamReconciliationRequired,
            D::TeamReconciliationRequired,
        ),
        (
            S::StorageUnavailable,
            P::StorageUnavailable,
            D::StorageUnavailable,
        ),
        (
            S::CatalogUnavailable,
            P::CurrentSettingsRequired,
            D::CatalogUnavailable,
        ),
        (
            S::AlreadyActivated,
            P::CurrentBindingRequired,
            D::AlreadyActivated,
        ),
        (S::Released, P::CurrentBindingRequired, D::Released),
        (
            S::NeedsAttention,
            P::CurrentBindingRequired,
            D::NeedsAttention,
        ),
    ] {
        assert_eq!(
            map_startup_blocked(input),
            RecoveryAdminError::Blocked { problem, detail }
        );
    }
}
