//! Restart client seams: confirmation, intent binding, reconnect rules, the
//! epoch resume plan and the one-shot command. Nothing here waits on time: the
//! reconnect machine takes the instants it is given.

mod support;

use workbench_client::restart::confirmation::*;

#[path = "restart_admin/intent.rs"]
mod intent;
#[path = "restart_admin/reconnect.rs"]
mod reconnect;
#[path = "restart_admin/resume.rs"]
mod resume;
#[path = "restart_admin/session.rs"]
mod session;
#[path = "restart_admin/view.rs"]
mod view;

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

#[test]
fn the_proof_exists_exactly_when_the_confirmation_does() {
    for terminal in [false, true] {
        for flag in [false, true] {
            for answer in [false, true] {
                assert_eq!(
                    confirm(terminal, flag, answer).map(Confirmed::method),
                    require_confirmation(terminal, flag, answer)
                );
            }
        }
    }
}
