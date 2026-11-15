//! The `memory` reads (PR 13) over a real database: what a project's lessons
//! show, what stays out, and that asking never changes anything.
#[path = "memory_reads/support.rs"]
mod support;

use std::sync::Arc;

use storage::memory::{Lesson, LessonScope};
use support::{World, operation, project, ready, world};
use workbench_host::areas::{Areas, diagnostics::read_ops::SlugOf, memory::MemoryArea};
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::ids::MemoryId;
use workbench_protocol::management::{ActionState, DenialReason, MemoryAction, MemoryStatus};
use workbench_protocol::namespaces::diagnostics::{OperationTarget, ReadOutcome};
use workbench_protocol::namespaces::memory::{GetMemory, ListMemory, MemoryQuery, MemoryReply};
use workbench_protocol::namespaces::memory::{MemoryDetail, MemorySummary};
use workbench_protocol::queries::{PageRequest, Query, QueryReply};
use workbench_protocol::views::Page;

const SLUG: &str = "acme/widgets";

fn memory_world() -> World {
    world(|parts| Areas {
        memory: MemoryArea::from_parts(
            parts.tasks,
            parts.projection.source("memory").unwrap(),
            parts.database,
            Arc::new(|_: &std::path::Path| SLUG.to_owned()) as SlugOf,
        ),
        ..Default::default()
    })
}

fn lesson(id: &str, project: &str, content: &str) -> Lesson {
    Lesson {
        id: id.into(),
        project: project.into(),
        task_id: "task-1".into(),
        content: content.into(),
        evidence: format!("evidence for {id}"),
        scope: LessonScope::Project,
    }
}

fn list_request(id: &str) -> ListMemory {
    ListMemory {
        operation_id: operation(id),
        project: project(),
        status: None,
        favorite: None,
        text: None,
        page: PageRequest::default(),
    }
}

fn list(world: &World, request: ListMemory) -> ReadOutcome<Page<MemorySummary>> {
    match world
        .query(Query::Memory(MemoryQuery::ListMemory(request)))
        .unwrap()
    {
        QueryReply::Memory(MemoryReply::List(outcome)) => *outcome,
        other => panic!("{other:?}"),
    }
}

fn detail(world: &World, id: &str, lesson: &str) -> Result<ReadOutcome<MemoryDetail>, ErrorCode> {
    match world.query(Query::Memory(MemoryQuery::GetMemory(GetMemory {
        operation_id: operation(id),
        project: project(),
        id: MemoryId::new(lesson).unwrap(),
    }))) {
        Ok(QueryReply::Memory(MemoryReply::Detail(outcome))) => Ok(*outcome),
        Ok(other) => panic!("{other:?}"),
        Err(error) => Err(error.code),
    }
}

fn seed(world: &World) {
    let writer = world.store.handle();
    writer
        .append_lesson(&lesson("lesson-a", SLUG, "Always run the formatter first"))
        .unwrap();
    writer
        .append_lesson(&lesson("lesson-b", SLUG, "Prefer small commits"))
        .unwrap();
    writer
        .append_lesson(&lesson("lesson-c", SLUG, "Quote \"words\" in searches"))
        .unwrap();
    writer
        .append_lesson(&lesson("lesson-other", "someone/else", "Another project"))
        .unwrap();
}

#[test]
fn a_list_is_an_operation_and_holds_only_this_projects_lessons() {
    let w = memory_world();
    seed(&w);
    let first = list(&w, list_request("list-1"));
    assert!(
        first.data.is_none(),
        "the answer to the first ask never waits for the read"
    );
    w.settle();
    let settled = list(&w, list_request("list-1"));
    assert!(ready(&settled.operation.state), "{:?}", settled.operation);
    let page = settled.data.unwrap();
    let ids: Vec<_> = page.items.iter().map(|m| m.state.id.as_str()).collect();
    assert_eq!(ids.len(), 3, "{ids:?}");
    assert!(!ids.contains(&"lesson-other"));
    for item in &page.items {
        assert_eq!(item.state.status, MemoryStatus::Candidate);
        assert!(!item.preview.is_empty());
    }
}

#[test]
fn filters_and_search_words_narrow_the_list_and_never_break_the_query() {
    let w = memory_world();
    seed(&w);
    w.store
        .handle()
        .set_lesson_favorite("lesson-b", true)
        .unwrap();

    let favorite = w.read(|| {
        Ok::<_, workbench_protocol::WorkbenchError>(list(
            &w,
            ListMemory {
                favorite: Some(true),
                ..list_request("list-favorite")
            },
        ))
    });
    let items = favorite.data.unwrap().items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].state.id.as_str(), "lesson-b");

    // A word with a quote, and one that is a full-text operator, are literals.
    for (n, text) in ["\"words\"", "formatter OR", "NEAR(", "*"]
        .into_iter()
        .enumerate()
    {
        let found = w.read(|| {
            Ok::<_, workbench_protocol::WorkbenchError>(list(
                &w,
                ListMemory {
                    text: Some(text.into()),
                    ..list_request(&format!("search-{n}"))
                },
            ))
        });
        assert!(
            ready(&found.operation.state),
            "{text}: {:?}",
            found.operation
        );
    }
    let formatter = w.read(|| {
        Ok::<_, workbench_protocol::WorkbenchError>(list(
            &w,
            ListMemory {
                text: Some("formatter".into()),
                ..list_request("search-formatter")
            },
        ))
    });
    let items = formatter.data.unwrap().items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].state.id.as_str(), "lesson-a");
}

#[test]
fn pages_of_one_operation_do_not_repeat_or_skip() {
    let w = memory_world();
    seed(&w);
    let page = |cursor| ListMemory {
        page: PageRequest { cursor, limit: 1 },
        ..list_request("paged")
    };
    let first = w.read(|| Ok::<_, workbench_protocol::WorkbenchError>(list(&w, page(None))));
    let mut seen = vec![];
    let mut current = first.data.unwrap();
    loop {
        assert_eq!(current.items.len(), 1);
        seen.push(current.items[0].state.id.as_str().to_owned());
        let Some(cursor) = current.next_cursor.clone() else {
            break;
        };
        current = list(&w, page(Some(cursor))).data.unwrap();
    }
    seen.sort();
    assert_eq!(seen, ["lesson-a", "lesson-b", "lesson-c"]);
}

#[test]
fn an_operation_id_names_one_request() {
    let w = memory_world();
    seed(&w);
    w.read(|| Ok::<_, workbench_protocol::WorkbenchError>(list(&w, list_request("same-id"))));
    let other = w.query(Query::Memory(MemoryQuery::ListMemory(ListMemory {
        favorite: Some(true),
        ..list_request("same-id")
    })));
    assert_eq!(other.unwrap_err().code, ErrorCode::Conflict);

    let unknown = w.query(Query::Memory(MemoryQuery::GetMemoryOperation(
        OperationTarget {
            operation_id: operation("never-asked"),
        },
    )));
    assert_eq!(unknown.unwrap_err().code, ErrorCode::NotFound);
    let known = w
        .query(Query::Memory(MemoryQuery::GetMemoryOperation(
            OperationTarget {
                operation_id: operation("same-id"),
            },
        )))
        .unwrap();
    assert!(matches!(
        known,
        QueryReply::Memory(MemoryReply::Operation(view)) if ready(&view.state)
    ));
}

#[test]
fn a_detail_is_bounded_and_a_foreign_lesson_is_not_found() {
    let w = memory_world();
    let long = "x".repeat(100 * 1024);
    w.store
        .handle()
        .append_lesson(&lesson(
            "lesson-secret",
            SLUG,
            &format!("never paste {long} into chat"),
        ))
        .unwrap();
    w.store
        .handle()
        .append_lesson(&lesson("lesson-foreign", "someone/else", "not yours"))
        .unwrap();

    let found = w.read(|| detail(&w, "detail-secret", "lesson-secret").map_err(error));
    let data = found.data.unwrap();
    assert!(data.content.truncated);
    assert!(data.content.total_bytes > data.content.text.len() as u64);
    assert!(data.content.text.starts_with("never paste"));

    let foreign = w.read(|| detail(&w, "detail-foreign", "lesson-foreign").map_err(error));
    assert!(foreign.data.is_none());
    assert!(matches!(
        foreign.operation.state,
        workbench_protocol::operation::OperationState::Failed {
            code: ErrorCode::NotFound
        }
    ));
}

fn error(code: ErrorCode) -> workbench_protocol::WorkbenchError {
    workbench_protocol::WorkbenchError::new(
        code,
        "refused",
        workbench_protocol::errors::RetryAdvice::Never,
    )
}

#[test]
fn the_revision_grows_with_every_change_and_the_actions_follow_the_state() {
    let w = memory_world();
    w.store
        .handle()
        .append_lesson(&lesson("lesson-r", SLUG, "revisions"))
        .unwrap();
    let before = w
        .read(|| detail(&w, "rev-1", "lesson-r").map_err(error))
        .data
        .unwrap()
        .state;
    let allowed = |state: &workbench_protocol::management::MemoryState, action| {
        state
            .actions
            .iter()
            .find(|a| a.action == action)
            .map(|a| a.state)
            .unwrap()
    };
    assert_eq!(allowed(&before, MemoryAction::Reject), ActionState::Allowed);

    w.store
        .handle()
        .set_lesson_favorite("lesson-r", true)
        .unwrap();
    let after = w
        .read(|| detail(&w, "rev-2", "lesson-r").map_err(error))
        .data
        .unwrap()
        .state;
    assert!(after.favorite);
    assert_ne!(before.revision, after.revision);
    assert_eq!(
        allowed(&after, MemoryAction::Reject),
        ActionState::Denied {
            reason: DenialReason::FavoriteMustBeCleared
        }
    );

    // The earlier operation still holds what it read: its snapshot is not rewritten.
    let kept = detail(&w, "rev-1", "lesson-r").unwrap().data.unwrap().state;
    assert_eq!(kept.revision, before.revision);
}

#[test]
fn asking_writes_nothing() {
    let w = memory_world();
    seed(&w);
    let count = |w: &World| {
        let db = storage::Database::open_read_only(&w.dir.path().join("host.db")).unwrap();
        ["lesson-a", "lesson-b", "lesson-c"]
            .into_iter()
            .map(|id| db.memory_history(id).unwrap().len() + db.favorite_history(id).unwrap().len())
            .sum::<usize>()
    };
    let before = count(&w);
    w.read(|| Ok::<_, workbench_protocol::WorkbenchError>(list(&w, list_request("no-write"))));
    detail(&w, "no-write-detail", "lesson-a").unwrap();
    w.settle();
    assert_eq!(count(&w), before);
}

#[test]
fn without_the_area_wired_a_read_is_unsupported() {
    let w = world(|_| Areas::default());
    let refused = w.query(Query::Memory(MemoryQuery::ListMemory(list_request("x"))));
    assert_eq!(refused.unwrap_err().code, ErrorCode::UnsupportedCapability);
}
