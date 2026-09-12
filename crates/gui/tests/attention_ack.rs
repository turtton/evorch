use gui::app::WorkbenchState;
use workspace_ui::ThreadRunPhase;

type Workbench = WorkbenchState<runtime::AgentRuntime>;

#[test]
fn unread_waiting_marks_attention_until_acknowledged() {
    // Given: a waiting surface whose current revision was displayed.
    let mut state = Workbench::new_attention_ack();
    state.observe(ThreadRunPhase::Waiting).unwrap();
    let revision = state.revision();
    assert!(state.is_unread());
    // When: that revision is acknowledged while focused.
    assert!(state.acknowledge_surface(Some(&revision), Some(true)));
    // Then: waiting remains waiting, but no longer needs emphasis.
    assert!(!state.is_unread());
    assert_eq!(state.phase(), ThreadRunPhase::Waiting);
}

#[test]
fn ack_requires_displayed_focused_and_revision_match() {
    for (displayed, focused) in [(false, Some(true)), (true, Some(false)), (true, None)] {
        // Given: waiting with one acknowledgement prerequisite absent.
        let mut state = Workbench::new_attention_ack();
        state.observe(ThreadRunPhase::Waiting).unwrap();
        let revision = state.revision();
        // When: an incomplete display acknowledgement arrives.
        let accepted = state.acknowledge_surface(displayed.then_some(&revision), focused);
        // Then: it cannot clear unread attention.
        assert!(!accepted);
        assert!(state.is_unread());
    }
}

#[test]
fn stale_revision_ack_is_ignored() {
    // Given: a rendered waiting revision superseded by completion.
    let mut state = Workbench::new_attention_ack();
    state.observe(ThreadRunPhase::Waiting).unwrap();
    let stale = state.revision();
    state.observe(ThreadRunPhase::Done).unwrap();
    // When: the old rendered revision is acknowledged.
    assert!(!state.acknowledge_surface(Some(&stale), Some(true)));
    // Then: completion remains unread.
    assert!(state.is_unread());
    assert_eq!(state.state_change_seq(), 2);
    assert_eq!(state.acknowledged_seq(), 0);
}

#[test]
fn repeated_phase_preserves_acknowledgement() {
    // Given: acknowledged waiting.
    let mut state = Workbench::new_attention_ack();
    state.observe(ThreadRunPhase::Waiting).unwrap();
    state.acknowledge_surface(Some(&state.revision()), Some(true));
    // When: polling reports the same phase again.
    state.observe(ThreadRunPhase::Waiting).unwrap();
    // Then: no artificial state change or unread marker is generated.
    assert_eq!(state.state_change_seq(), 1);
    assert_eq!(state.acknowledged_seq(), 1);
    assert!(!state.is_unread());
}

#[test]
fn subsequent_completion_becomes_unread() {
    // Given: waiting has already been read.
    let mut state = Workbench::new_attention_ack();
    state.observe(ThreadRunPhase::Waiting).unwrap();
    state.acknowledge_surface(Some(&state.revision()), Some(true));
    // When: the run completes.
    state.observe(ThreadRunPhase::Done).unwrap();
    // Then: a new unread revision is exposed.
    assert!(state.is_unread());
    assert_eq!(state.state_change_seq(), 2);
    assert_eq!(state.acknowledged_seq(), 1);
}

#[test]
fn revision_from_another_surface_or_previous_boot_is_ignored() {
    // Given: two independently created surfaces with identical sequence numbers.
    let mut previous = Workbench::new_attention_ack();
    previous.observe(ThreadRunPhase::Waiting).unwrap();
    let mut current = Workbench::new_attention_ack();
    current.observe(ThreadRunPhase::Waiting).unwrap();
    // When: a revision from another pane/thread or a prior model lifetime arrives.
    assert!(!current.acknowledge_surface(Some(&previous.revision()), Some(true)));
    // Then: equal sequence numbers cannot acknowledge a different surface.
    assert!(current.is_unread());
}

#[test]
fn inactive_phases_do_not_request_unread_emphasis() {
    for phase in [ThreadRunPhase::Pending, ThreadRunPhase::Running] {
        // Given: a new surface.
        let mut state = Workbench::new_attention_ack();
        // When: an inactive or running phase is observed.
        state.observe(phase).unwrap();
        // Then: unread emphasis is reserved for actionable/completed states.
        assert!(!state.is_unread());
    }
}
