use workbench_client::restart::confirmation::*;
#[test]
fn confirmation_matrix() {
    for terminal in [false, true] {
        for answer in [false, true] {
            assert_eq!(
                require_confirmation(terminal, true, answer),
                Ok(Confirmation::ExplicitFlag)
            );
        }
    }
    assert!(require_confirmation(false, false, true).is_err());
    assert!(require_confirmation(false, false, false).is_err());
    assert!(require_confirmation(true, false, false).is_err());
    assert_eq!(
        require_confirmation(true, false, true),
        Ok(Confirmation::InteractiveApproval)
    );
}
