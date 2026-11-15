//! The memory feature drawn headless: a list is asked as an operation and asked
//! again when it settles, an answer for another project is dropped, and the screen
//! only ever sends queries.
use super::*;
use crate::core_client::features::execution::tests::{answer, asked, query_of, screen};
use egui_kittest::kittest::Queryable;
use workbench_protocol::{
    errors::ErrorCode,
    ids::OperationId,
    management::MemoryState,
    namespaces::diagnostics::ReadOutcome,
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
    screen.get_by_label("読み込み中…");

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
    screen.get_by_label("★ lesson-a · 候補 · preview of lesson-a");
    screen.get_by_label("lesson-b · 候補 · preview of lesson-b");

    screen
        .get_by_label("lesson-b · 候補 · preview of lesson-b")
        .click();
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
    screen.get_by_label("seen twice");
    screen.get_by_label("(先頭 10 バイトのみ表示 / 全体 4000 バイト)");
    screen.get_by_label("タスク: task-9");
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
    screen.get_by_label("new-one · 候補 · preview of new-one");
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
    screen.get_by_label("保存先を読み取れませんでした。");

    // The search input is shut until it is asked for, and draws no text input before.
    assert!(
        screen
            .query_by_role(egui::accesskit::Role::TextInput)
            .is_none()
    );
    screen.get_by_label("検索語で絞り込む").click();
    screen.run();
    screen.get_by_label("検索").click();
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
    screen.get_by_label("プロジェクトを選択すると教訓を表示します。");
}
