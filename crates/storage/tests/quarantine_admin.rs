//! Q1 storage contracts: inspection, revision CAS, idempotent discard and durability.
#[path = "quarantine_admin/discard.rs"]
mod discard;
#[path = "quarantine_admin/fixture.rs"]
mod fixture;
#[path = "quarantine_admin/inspection.rs"]
mod inspection;

#[test]
fn denial_and_operation_budgets_are_separate() {
    use storage::admin_audit::*;
    assert_eq!(DENIAL_MAX_ROWS, 1000);
    assert_eq!(AUDIT_MAX_ACCEPTED_ROWS, 10000);
    assert_eq!(AUDIT_BATCH_ROWS, 64);
}
