//! The memory feature drawn headless: a list is asked as an operation and asked
//! again when it settles, an answer for another project is dropped, and the screen
//! only ever sends queries.
use super::*;
use crate::core_client::features::{
    FeatureAction, FeatureError, FeatureRequest,
    execution::tests::{answer, asked, command_of, query_of, screen},
};
use egui_kittest::kittest::{NodeT, Queryable};
use workbench_protocol::{
    errors::ErrorCode,
    ids::OperationId,
    management::MemoryState,
    namespaces::diagnostics::ReadOutcome,
    namespaces::memory::MemoryScope,
    operation::{OperationState, OperationView},
    views::Page,
    wire::Revision,
};

fn view(id: &OperationId, state: OperationState) -> OperationView {
    OperationView {
        operation_id: id.clone(),
        state,
        revision: Revision(1),
        result: None,
    }
}

fn summary(id: &str, favorite: bool) -> MemorySummary {
    MemorySummary {
        state: MemoryState {
            id: MemoryId::new(id).unwrap(),
            status: MemoryStatus::Candidate,
            favorite,
            revision: Revision(1),
            actions: vec![],
        },
        scope: MemoryScope::Project,
        preview: format!("preview of {id}"),
    }
}

fn list_reply(id: &OperationId, items: Vec<MemorySummary>) -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::Memory(MemoryReply::List(Box::new(
        ReadOutcome {
            operation: view(id, OperationState::Ready),
            data: Some(Page {
                items,
                next_cursor: None,
                snapshot_revision: Revision(1),
            }),
        },
    )))))
}

fn pending_reply(id: &OperationId) -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::Memory(MemoryReply::List(Box::new(
        ReadOutcome {
            operation: view(id, OperationState::Pending),
            data: None,
        },
    )))))
}

fn list_of(action: &crate::core_client::features::FeatureAction) -> ListMemory {
    match query_of(action) {
        Query::Memory(MemoryQuery::ListMemory(list)) => list.clone(),
        other => panic!("a list was expected, got {other:?}"),
    }
}

fn settled(feature: &mut dyn Feature, id: &OperationId) {
    feature.event(&UiEvent::Memory(MemoryEvent::MemoryOperationChanged {
        view: view(id, OperationState::Ready),
    }));
}

#[test]
fn a_list_is_asked_again_when_it_settles_and_a_row_opens_its_detail() {
    let mut screen = screen(Box::<MemoryFeature>::default(), None);
    let first = asked(&mut screen);
    assert_eq!(first.len(), 1);
    let list = list_of(&first[0]);
    assert_eq!(list.project.as_str(), "p");

    answer(&mut screen, first[0].tag, pending_reply(&list.operation_id));
    screen.get_by_label("Loading…");

    settled(screen.state_mut().feature.as_mut(), &list.operation_id);
    let again = asked(&mut screen);
    assert_eq!(again.len(), 1);
    assert_eq!(
        list_of(&again[0]).operation_id,
        list.operation_id,
        "the same operation is asked for again"
    );
    answer(
        &mut screen,
        again[0].tag,
        list_reply(
            &list.operation_id,
            vec![summary("lesson-a", true), summary("lesson-b", false)],
        ),
    );
    screen.get_by_label("lesson-a");
    screen.get_by_label("lesson-b");
    screen.get_by_label("preview of lesson-a");
    assert_eq!(screen.query_all_by_label("candidate").count(), 2);
    assert_eq!(screen.query_all_by_label("Approved").count(), 1);
    assert_eq!(screen.query_all_by_label("Unconfirmed").count(), 1);
    screen.get_by_label("2 lessons");

    screen.get_by_label("lesson-b").click();
    let detail = asked(&mut screen);
    assert_eq!(detail.len(), 1);
    let Query::Memory(MemoryQuery::GetMemory(get)) = query_of(&detail[0]) else {
        panic!("a detail was expected");
    };
    assert_eq!(get.id.as_str(), "lesson-b");
    assert_ne!(get.operation_id, list.operation_id);
    let body = |text: &str, total: u64| BoundedText {
        text: text.into(),
        total_bytes: total,
        truncated: total > text.len() as u64,
    };
    answer(
        &mut screen,
        detail[0].tag,
        FeatureReply::Query(Ok(QueryReply::Memory(MemoryReply::Detail(Box::new(
            ReadOutcome {
                operation: view(&get.operation_id, OperationState::Ready),
                data: Some(MemoryDetail {
                    state: summary("lesson-b", false).state,
                    scope: MemoryScope::User,
                    task_id: "task-9".into(),
                    content: body("always format", 13),
                    evidence: body("seen twice", 4000),
                }),
            },
        ))))),
    );
    screen.get_by_label("always format");
    screen.get_by_label("user");
    // The evidence is folded, as in the legacy pane, until it is asked for.
    assert!(screen.query_by_label("seen twice").is_none());
    screen.get_by_label("Evidence").click();
    screen.run();
    screen.get_by_label("seen twice");
    screen.get_by_label("Showing the first 10 of 4000 bytes.");
    screen.get_by_label("task-9");
}

#[test]
fn an_answer_for_another_project_or_an_older_ask_is_dropped() {
    let mut screen = screen(Box::<MemoryFeature>::default(), None);
    let first = asked(&mut screen);
    let old = list_of(&first[0]);

    screen.state_mut().selection.project = Some(ProjectId::new("q").unwrap());
    let second = asked(&mut screen);
    let new = list_of(&second[0]);
    assert_eq!(new.project.as_str(), "q");
    assert_ne!(new.operation_id, old.operation_id);

    answer(
        &mut screen,
        first[0].tag,
        list_reply(&old.operation_id, vec![summary("old-one", false)]),
    );
    assert!(screen.query_by_label_contains("old-one").is_none());
    answer(
        &mut screen,
        second[0].tag,
        list_reply(&new.operation_id, vec![summary("new-one", false)]),
    );
    screen.get_by_label("new-one");
    screen.get_by_label("preview of new-one");
}

#[test]
fn a_failed_read_is_shown_and_a_new_search_is_a_new_operation() {
    let mut screen = screen(Box::<MemoryFeature>::default(), None);
    let first = asked(&mut screen);
    let list = list_of(&first[0]);
    answer(
        &mut screen,
        first[0].tag,
        FeatureReply::Query(Ok(QueryReply::Memory(MemoryReply::List(Box::new(
            ReadOutcome {
                operation: view(
                    &list.operation_id,
                    OperationState::Failed {
                        code: ErrorCode::StorageUnavailable,
                    },
                ),
                data: None,
            },
        ))))),
    );
    screen.get_by_label("The storage could not be read.");

    // The search input is shut until it is asked for, and draws no text input before.
    assert!(
        screen
            .query_by_role(egui::accesskit::Role::TextInput)
            .is_none()
    );
    screen.get_by_label("Filter by words").click();
    screen.run();
    screen.get_by_label("Search").click();
    let next = asked(&mut screen);
    assert_eq!(next.len(), 1);
    assert_ne!(list_of(&next[0]).operation_id, list.operation_id);
}

#[test]
fn without_a_project_nothing_is_asked() {
    let mut screen = screen(Box::<MemoryFeature>::default(), None);
    // The first frame, drawn while the project was still selected, asked once.
    screen.state_mut().frame.actions.clear();
    screen.state_mut().selection.project = None;
    assert!(asked(&mut screen).is_empty());
    screen.get_by_label("Select a project to browse memory.");
}

/// A screen with `lesson-b` open: validated is offered, rejecting is refused
/// because the lesson is a favorite.
fn screen_with_detail()
-> egui_kittest::Harness<'static, crate::core_client::features::execution::tests::Screen> {
    screen_with_lesson(
        true,
        vec![
            ActionAvailability {
                action: MemoryAction::Validate,
                state: ActionState::Allowed,
            },
            ActionAvailability {
                action: MemoryAction::Reject,
                state: ActionState::Denied {
                    reason: DenialReason::FavoriteMustBeCleared,
                },
            },
        ],
    )
}

/// A screen with `lesson-b` (revision 7) open, offering `actions`.
fn screen_with_lesson(
    favorite: bool,
    actions: Vec<ActionAvailability<MemoryAction>>,
) -> egui_kittest::Harness<'static, crate::core_client::features::execution::tests::Screen> {
    let mut screen = screen(Box::<MemoryFeature>::default(), None);
    let first = asked(&mut screen);
    let list = list_of(&first[0]);
    answer(
        &mut screen,
        first[0].tag,
        list_reply(&list.operation_id, vec![summary("lesson-b", favorite)]),
    );
    screen.get_by_label("lesson-b").click();
    let detail = asked(&mut screen);
    let Query::Memory(MemoryQuery::GetMemory(get)) = query_of(&detail[0]) else {
        panic!("a detail was expected");
    };
    let mut state = summary("lesson-b", favorite).state;
    state.revision = Revision(7);
    state.actions = actions;
    let text = |text: &str| BoundedText {
        text: text.into(),
        total_bytes: text.len() as u64,
        truncated: false,
    };
    answer(
        &mut screen,
        detail[0].tag,
        FeatureReply::Query(Ok(QueryReply::Memory(MemoryReply::Detail(Box::new(
            ReadOutcome {
                operation: view(&get.operation_id, OperationState::Ready),
                data: Some(MemoryDetail {
                    state,
                    scope: MemoryScope::Project,
                    task_id: "task-1".into(),
                    content: text("content"),
                    evidence: text("evidence"),
                }),
            },
        ))))),
    );
    screen
}

fn update_view(id: &OperationId, state: OperationState, changed: Option<bool>) -> MemoryUpdateView {
    MemoryUpdateView {
        operation: view(id, state),
        project: ProjectId::new("p").unwrap(),
        id: MemoryId::new("lesson-b").unwrap(),
        action: MemoryAction::Validate,
        revision: None,
        changed,
        denial: None,
        message: None,
    }
}

fn sent_update(action: &FeatureAction) -> &UpdateMemory {
    match command_of(action) {
        Command::Memory(MemoryCommand::ValidateMemory(update)) => update,
        other => panic!("a validate was expected, got {other:?}"),
    }
}

#[test]
fn an_update_carries_the_revision_that_was_read_and_is_sent_once() {
    let mut screen = screen_with_detail();
    // What the core refuses is shown as a disabled button, never sent.
    assert!(screen.get_by_label("Reject").accesskit_node().is_disabled());

    screen.get_by_label("Validate").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let update = sent_update(&sent[0]);
    assert_eq!(update.project.as_str(), "p");
    assert_eq!(update.target.id.as_str(), "lesson-b");
    assert_eq!(update.target.expected_revision, Revision(7));
    screen.get_by_label("Updating…");

    // While it runs, a second click sends nothing.
    screen.get_by_label("Validate").click();
    assert!(asked(&mut screen).is_empty());

    // Its end arrives as an event and the lesson is read again, not guessed.
    let id = update.operation_id.clone();
    screen
        .state_mut()
        .feature
        .event(&UiEvent::Memory(MemoryEvent::MemoryUpdateCompleted {
            update: update_view(&id, OperationState::Ready, Some(true)),
        }));
    let again = asked(&mut screen);
    screen.get_by_label("Updated.");
    assert!(
        again
            .iter()
            .all(|action| matches!(action.request, FeatureRequest::Query(_)))
    );
    assert!(
        again
            .iter()
            .any(|action| matches!(query_of(action), Query::Memory(MemoryQuery::GetMemory(_))))
    );
}

#[test]
fn a_stale_revision_asks_to_read_again_and_does_not_resend() {
    let mut screen = screen_with_detail();
    screen.get_by_label("Validate").click();
    let sent = asked(&mut screen);
    let id = sent_update(&sent[0]).operation_id.clone();
    let failed = update_view(
        &id,
        OperationState::Failed {
            code: ErrorCode::StaleRevision,
        },
        None,
    );
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Ok({
            let mut reply = workbench_protocol::commands::CommandReply::accepted();
            reply.result = Some(Box::new(CommandResult::Memory(MemoryResult::Operation(
                failed,
            ))));
            reply
        })),
    );
    screen.get_by_label_contains("changed elsewhere");
    let next = asked(&mut screen);
    assert!(
        next.iter()
            .all(|action| matches!(action.request, FeatureRequest::Query(_))),
        "the stale update is read about, not retried"
    );
    assert!(!next.is_empty());
}

#[test]
fn an_update_of_unknown_outcome_is_asked_for_and_never_counted_as_done() {
    let mut screen = screen_with_detail();
    screen.get_by_label("Validate").click();
    let sent = asked(&mut screen);
    let id = sent_update(&sent[0]).operation_id.clone();
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
    );
    let check = asked(&mut screen);
    assert_eq!(check.len(), 1, "only a question about the operation");
    let Query::Memory(MemoryQuery::GetMemoryUpdate(target)) = query_of(&check[0]) else {
        panic!("the update was to be asked for");
    };
    assert_eq!(target.operation_id, id);

    // The core does not know it (it restarted): not done, read the state.
    answer(
        &mut screen,
        check[0].tag,
        FeatureReply::Query(Err(
            FeatureError::new("no such operation").with_code(ErrorCode::NotFound)
        )),
    );
    screen.get_by_label_contains("not counted as done");
    assert!(screen.query_by_label("Updated.").is_none());
    let reread = asked(&mut screen);
    assert!(
        reread
            .iter()
            .all(|action| matches!(action.request, FeatureRequest::Query(_)))
    );
}

#[test]
fn the_approval_filter_and_refresh_each_begin_a_new_list() {
    let mut screen = screen(Box::<MemoryFeature>::default(), None);
    let first = list_of(&asked(&mut screen)[0]);
    assert_eq!(first.favorite, None);

    screen.get_by_label("Approval").click();
    screen.run();
    screen.get_by_label("Unconfirmed").click();
    let filtered = asked(&mut screen);
    assert_eq!(filtered.len(), 1);
    let filtered = list_of(&filtered[0]);
    assert_eq!(filtered.favorite, Some(false), "unconfirmed lessons only");
    assert_ne!(filtered.operation_id, first.operation_id);

    screen.get_by_label("Refresh").click();
    let refreshed = asked(&mut screen);
    assert_eq!(refreshed.len(), 1);
    let refreshed = list_of(&refreshed[0]);
    assert_eq!(refreshed.favorite, Some(false), "the filter is kept");
    assert_ne!(refreshed.operation_id, filtered.operation_id);
}

#[test]
fn a_reject_is_sent_only_once_confirmed_with_the_revision_that_was_read() {
    let mut screen = screen_with_lesson(
        false,
        vec![
            ActionAvailability {
                action: MemoryAction::Reject,
                state: ActionState::Allowed,
            },
            ActionAvailability {
                action: MemoryAction::SetFavorite,
                state: ActionState::Allowed,
            },
        ],
    );
    // The approval comes first, as in the legacy pane.
    let approve = screen.get_by_label("Approve").rect().min.x;
    assert!(approve < screen.get_by_label("Reject").rect().min.x);

    screen.get_by_label("Reject").click();
    assert!(asked(&mut screen).is_empty(), "one click sends nothing");
    screen.get_by_label("Reject permanently?");
    screen.get_by_label("Cancel").click();
    assert!(asked(&mut screen).is_empty());
    assert!(screen.query_by_label("Confirm reject").is_none());

    screen.get_by_label("Reject").click();
    asked(&mut screen);
    screen.get_by_label("Confirm reject").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let Command::Memory(MemoryCommand::RejectMemory(update)) = command_of(&sent[0]) else {
        panic!("a reject was expected");
    };
    assert_eq!(update.target.id.as_str(), "lesson-b");
    assert_eq!(update.target.expected_revision, Revision(7));
    assert!(screen.query_by_label("Confirm reject").is_none());
}
