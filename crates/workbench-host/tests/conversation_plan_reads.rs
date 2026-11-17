//! Read-only workbench plan contract over the real durable projection.
mod common;
use event_bus::{ConversationPlanEvent, EventKind, PlanTaskStatus};
use storage::conversation_plan::{PlanOperation, PlanUpdate, TaskRef};
use workbench_host::areas::execution::ExecutionArea;
use workbench_protocol::{
    commands::ThreadTarget,
    errors::ErrorCode,
    ids::ThreadId,
    namespaces::execution::plan::*,
    namespaces::{ExecutionEvent, ExecutionQuery, ExecutionReply},
    queries::{Query, QueryReply},
    wire::Revision,
};
struct Fixture {
    _dir: tempfile::TempDir,
    db: storage::Storage,
    area: ExecutionArea,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("plan.db"),
            ..Default::default()
        };
        let db = storage::Storage::open(config.clone()).unwrap();
        let runtime = common::runtime()
            .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
        let area = ExecutionArea::with_runtime(runtime.clone());
        Self {
            _dir: dir,
            db,
            area,
        }
    }
    fn query(
        &self,
        q: ExecutionQuery,
    ) -> Result<ExecutionReply, workbench_protocol::WorkbenchError> {
        let QueryReply::Execution(reply) = self.area.query(&Query::Execution(q.clone()), &q)?
        else {
            panic!("wrong namespace")
        };
        reply.validate(&Default::default()).unwrap();
        let json = serde_json::to_string(&reply).unwrap();
        let back: ExecutionReply = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reply);
        Ok(reply)
    }
    fn update(&self, rev: u64, ops: Vec<PlanOperation>) -> storage::conversation_plan::PlanApplied {
        self.db
            .handle()
            .update_conversation_plan(PlanUpdate {
                thread_id: "thread".into(),
                expected_revision: rev,
                source_run_id: Some("root".into()),
                operations: ops,
            })
            .unwrap()
    }
}
fn target() -> ThreadTarget {
    ThreadTarget {
        thread: ThreadId::new("thread").unwrap(),
    }
}
fn add(name: &str) -> PlanOperation {
    PlanOperation::Add {
        client_ref: name.into(),
        parent: None,
        position: None,
        title: name.into(),
        description: Some("detail".into()),
        weight: None,
        status: Some(PlanTaskStatus::InProgress),
    }
}
fn tree(cursor: Option<String>) -> ExecutionQuery {
    ExecutionQuery::ReadConversationPlanTree(PlanTreeQuery {
        thread: target().thread,
        cursor,
        limit: Some(1),
    })
}
fn summary(f: &Fixture) -> PlanSummary {
    let ExecutionReply::ConversationPlan(p) = f
        .query(ExecutionQuery::GetConversationPlan(target()))
        .unwrap()
    else {
        panic!()
    };
    *p
}
#[test]
fn reads_page_at_one_revision_and_reject_stale_or_foreign_cursors() {
    let f = Fixture::new();
    assert_eq!(summary(&f).revision, Revision(0));
    assert_eq!(summary(&f).plan_id, None);
    let first = f.update(0, vec![add("a"), add("b")]);
    let ExecutionReply::ConversationPlanTree(page) = f.query(tree(None)).unwrap() else {
        panic!()
    };
    assert_eq!(page.revision, Revision(1));
    assert_eq!(page.nodes.len(), 1);
    let cursor = page.next_cursor.unwrap();
    let ExecutionReply::ConversationPlanTree(next) = f.query(tree(Some(cursor.clone()))).unwrap()
    else {
        panic!()
    };
    assert_eq!(next.revision, Revision(1));
    assert_ne!(page.nodes[0].task_id, next.nodes[0].task_id);
    assert!(next.next_cursor.is_none());
    let task = first.created[0].1.clone();
    let ExecutionReply::ConversationPlanTask(detail) = f
        .query(ExecutionQuery::GetConversationPlanTask(PlanTaskQuery {
            thread: target().thread,
            task_id: task.clone(),
        }))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(detail.node.description, "detail");
    assert_eq!(detail.revision, Revision(1));
    let foreign = ExecutionQuery::GetConversationPlanTask(PlanTaskQuery {
        thread: ThreadId::new("other").unwrap(),
        task_id: task.clone(),
    });
    assert_eq!(f.query(foreign).unwrap_err().code, ErrorCode::NotFound);
    f.update(
        1,
        vec![PlanOperation::Update {
            task: TaskRef::Id(task),
            title: Some("new".into()),
            description: None,
            result: Some("result".into()),
            blocker: None,
            weight: None,
            status: None,
        }],
    );
    assert_eq!(
        f.query(tree(Some(cursor))).unwrap_err().code,
        ErrorCode::Conflict
    );
    assert_eq!(
        f.query(tree(Some("bad".into()))).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(summary(&f).revision, Revision(2));
    let mut bad = PlanTreeQuery {
        thread: target().thread,
        cursor: None,
        limit: Some(101),
    };
    assert_eq!(
        f.query(ExecutionQuery::ReadConversationPlanTree(bad.clone()))
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    bad.limit = Some(0);
    assert!(
        f.query(ExecutionQuery::ReadConversationPlanTree(bad))
            .is_err()
    );
}
#[test]
fn history_pages_and_re_reads_recover_from_missing_duplicate_reversed_hints() {
    let f = Fixture::new();
    let one = f.update(0, vec![add("a")]);
    let two = f.update(1, vec![add("b")]);
    let query = |after| {
        ExecutionQuery::ReadConversationPlanHistory(PlanHistoryQuery {
            thread: target().thread,
            after_revision: after,
            limit: Some(1),
        })
    };
    let ExecutionReply::ConversationPlanHistory(page) = f.query(query(None)).unwrap() else {
        panic!()
    };
    assert_eq!(page.entries[0].revision, Revision(1));
    assert_eq!(page.next_after_revision, Some(Revision(1)));
    let ExecutionReply::ConversationPlanHistory(last) =
        f.query(query(page.next_after_revision)).unwrap()
    else {
        panic!()
    };
    assert_eq!(last.entries[0].revision, Revision(2));
    assert!(last.next_after_revision.is_none());
    assert!(last.entries[0].change.is_some());
    assert!(last.entries[0].event_id > page.entries[0].event_id);
    // No hint was delivered for revision 2. A fresh query already sees it.
    assert_eq!(summary(&f).revision, Revision(2));
    let mut observed = summary(&f);
    for applied in [&two, &one, &one] {
        let EventKind::ConversationPlan(ConversationPlanEvent::Changed {
            thread_id,
            revision,
            ..
        }) = &applied.event.as_ref().unwrap().event.kind
        else {
            panic!()
        };
        let hint = ExecutionEvent::ConversationPlanChanged {
            thread: ThreadId::new(thread_id).unwrap(),
            revision: Revision(*revision),
        };
        hint.validate().unwrap();
        let encoded = serde_json::to_value(&hint).unwrap();
        assert!(encoded.to_string().len() < 256);
        assert!(encoded["payload"].get("nodes").is_none());
        observed = summary(&f);
        assert_eq!(observed.revision, Revision(2));
    }
    assert_eq!(observed.progress.node_count, 2);
    let restart = common::runtime().with_run_store(
        runtime::RunStore::open(
            &storage::StorageConfig {
                db_path: f._dir.path().join("plan.db"),
                ..Default::default()
            },
            f.db.handle(),
        )
        .unwrap(),
    );
    let area = ExecutionArea::with_runtime(restart);
    let q = ExecutionQuery::GetConversationPlan(target());
    assert!(area.query(&Query::Execution(q.clone()), &q).is_ok());
}
