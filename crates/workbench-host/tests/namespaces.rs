//! Host adapters per namespace: a query is answered in the namespace it named,
//! by the adapter that owns it, and what only the host knows is not mistaken
//! for something the projection lacks.

mod common;

use workbench_host::CommandHost;
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::ids::*;
use workbench_protocol::namespaces::{
    ApprovalsQuery, ApprovalsReply, ConversationQuery, ConversationReply, ExecutionQuery,
    ExecutionReply, LanesQuery, LifecycleQuery, LifecycleReply, ProjectionQuery, ProjectionReply,
    QuestionsQuery, QuestionsReply, WorkspaceQuery, WorkspaceReply,
};
use workbench_protocol::queries::*;
use workbench_protocol::wire::{ByteOffset, InstanceEpoch, Revision};

struct Fixture {
    _rt: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
    _store: storage::Storage,
    host: CommandHost,
}

impl Fixture {
    fn new() -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (runtime, store) = common::runtime_with_store(dir.path());
        let host = common::host_with_project(&rt, runtime, dir.path());
        Self {
            _rt: rt,
            _dir: dir,
            _store: store,
            host,
        }
    }

    fn query(&self, query: Query) -> Result<QueryReply, workbench_protocol::WorkbenchError> {
        self.host
            .handle_query(&SessionId::new("session").unwrap(), &query)
    }
}

fn thread() -> ThreadId {
    ThreadId::new("thread-1").unwrap()
}

#[test]
fn a_query_is_answered_in_the_namespace_it_named() {
    let f = Fixture::new();
    let page = PageRequest::default();
    let pending = PendingQuery {
        thread: None,
        pending_only: false,
        page: page.clone(),
    };
    let thread_query = ThreadQuery {
        thread: thread(),
        page: page.clone(),
    };

    assert!(matches!(
        f.query(Query::Workspace(WorkspaceQuery::ListProjects(page.clone()))),
        Ok(QueryReply::Workspace(WorkspaceReply::Projects(_)))
    ));
    assert!(matches!(
        f.query(Query::Workspace(WorkspaceQuery::ListThreads(ListThreads {
            project: common::project(),
            page: page.clone(),
        }))),
        Ok(QueryReply::Workspace(WorkspaceReply::Threads(_)))
    ));
    assert!(matches!(
        f.query(Query::Execution(ExecutionQuery::ListRuns(
            thread_query.clone()
        ))),
        Ok(QueryReply::Execution(ExecutionReply::Runs(_)))
    ));
    assert!(matches!(
        f.query(Query::Questions(QuestionsQuery::ListQuestions(
            pending.clone()
        ))),
        Ok(QueryReply::Questions(QuestionsReply::Questions(_)))
    ));
    assert!(matches!(
        f.query(Query::Approvals(ApprovalsQuery::ListApprovals(pending))),
        Ok(QueryReply::Approvals(ApprovalsReply::Approvals(_)))
    ));
    assert!(matches!(
        f.query(Query::Conversation(ConversationQuery::GetInputReceipts(
            thread_query
        ))),
        Ok(QueryReply::Conversation(ConversationReply::InputReceipts(
            _
        )))
    ));
    assert!(matches!(
        f.query(Query::Lifecycle(LifecycleQuery::GetCoreStatus)),
        Ok(QueryReply::Lifecycle(LifecycleReply::CoreStatus(_)))
    ));
}

#[test]
fn what_only_the_host_knows_is_answered_by_the_host_adapters() {
    let f = Fixture::new();
    // The receipt belongs to the ingress session, not to the projection.
    let reply = f
        .query(Query::Projection(ProjectionQuery::GetCommandReceipt {
            request_id: RequestId::new("req-never-sent").unwrap(),
        }))
        .unwrap();
    let QueryReply::Projection(ProjectionReply::CommandReceipt(receipt)) = reply else {
        panic!("expected a command receipt, got {reply:?}");
    };
    assert_eq!(receipt.request_id.as_str(), "req-never-sent");
    assert_eq!(receipt.state, CommandReceiptState::Unknown);

    // An unknown lane and an unknown approval are not found, which is the
    // adapters' answer; an unsupported error would mean nobody owned them.
    let lane = f
        .query(Query::Lanes(LanesQuery::GetLaneManagement {
            lane_id: workbench_protocol::lane_attention::LaneId::new("lane-x").unwrap(),
        }))
        .unwrap_err();
    assert_ne!(lane.code, ErrorCode::UnsupportedCapability);
    let input = f
        .query(Query::Approvals(ApprovalsQuery::GetApprovalInput(
            GetApprovalInput {
                approval_id: ApprovalId::new("approval-x").unwrap(),
                core_epoch: InstanceEpoch(1),
                approval_revision: Revision(1),
                offset: ByteOffset(0),
                max_bytes: 65_536,
            },
        )))
        .unwrap_err();
    assert_ne!(input.code, ErrorCode::UnsupportedCapability);
}

#[test]
fn a_query_that_fails_validation_is_refused_before_any_adapter_runs() {
    let f = Fixture::new();
    let error = f
        .query(Query::Projection(ProjectionQuery::ReadBody(ReadBody {
            body_id: BodyId::new("body-1").unwrap(),
            offset: ByteOffset(0),
            max_bytes: 1,
        })))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
}
