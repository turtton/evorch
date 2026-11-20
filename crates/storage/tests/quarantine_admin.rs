//! Q1 registration-only seam, not a quarantine mutation implementation.
#[test]
fn denial_and_operation_budgets_are_separate() {
    use storage::admin_audit::*;
    assert_eq!(DENIAL_MAX_ROWS, 1000);
    assert_eq!(AUDIT_MAX_ACCEPTED_ROWS, 10000);
    assert_eq!(AUDIT_BATCH_ROWS, 64);
}
