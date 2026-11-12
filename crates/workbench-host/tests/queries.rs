mod common;

use event_bus::{Event, MessageEvent, UserQuestion};
use runtime::AgentRuntime;
use sandbox::approval::{ApprovalRegistry, ApprovalRequest, ApprovalState};
use std::time::Duration;
use workbench_host::CommandHost;
use workbench_protocol::namespaces::{
    ApprovalsCommand, ApprovalsEvent, ApprovalsQuery, ApprovalsReply, ConversationEvent,
    ConversationQuery, ConversationReply, ExecutionQuery, ExecutionReply, LifecycleQuery,
    LifecycleReply, ProjectionEvent, ProjectionQuery, ProjectionReply, QuestionsCommand,
    QuestionsEvent, QuestionsQuery, QuestionsReply, WorkspaceQuery, WorkspaceReply,
};
use workbench_protocol::{
    commands::*, errors::ErrorCode, events::*, ids::*, queries::*, views::*, wire::*,
};

struct Fixture {
    rt: tokio::runtime::Runtime,
    dir: tempfile::TempDir,
    store: storage::Storage,
    runtime: AgentRuntime,
    host: CommandHost,
}
impl Fixture {
    fn new() -> Self {
        Self::with_runtime(tokio::runtime::Runtime::new().unwrap())
    }
    fn with_runtime(rt: tokio::runtime::Runtime) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (runtime, store) = common::runtime_with_store(dir.path());
        let host = common::host_with_project(&rt, runtime.clone(), dir.path());
        let f = Self {
            rt,
            dir,
            store,
            runtime,
            host,
        };
        f.thread("thread-1", "run-1", "First");
        f
    }
    fn thread(&self, thread: &str, run: &str, title: &str) {
        self.store
            .handle()
            .upsert_threads_host(&[storage::threads::ThreadUpsert {
                thread_id: thread.into(),
                project_id: Some(common::project().to_string()),
                project_root: Some(self.dir.path().to_string_lossy().into_owned()),
                title: Some(title.into()),
                root_run_id: Some(run.into()),
                ..Default::default()
            }])
            .unwrap();
        self.store
            .handle()
            .upsert_run_context(&storage::RunContextRecord {
                run_id: run.into(),
                role: "worker".into(),
                name: format!("chat:Worker:{thread}"),
                parent_run_id: None,
                config_json: "{}".into(),
                messages_json: "[]".into(),
                checkpoints_json: "[]".into(),
                terminal_phase: "done".into(),
                restorable: false,
                updated_at_ns: 1,
                host_inputs_json: "[]".into(),
            })
            .unwrap();
    }
    fn query(&self, query: Query) -> QueryReply {
        self.host
            .handle_query(&SessionId::new("session").unwrap(), &query)
            .unwrap()
    }
    fn subscribe(&self) -> workbench_host::subscriptions::Subscription {
        self.host
            .projection()
            .subscribe_with_snapshot(&Subscribe {
                scope: SubscriptionScope::All,
                after: None,
            })
            .unwrap()
    }
    fn tombstone(&self) {
        let db = rusqlite::Connection::open(self.dir.path().join("host.db")).unwrap();
        db.execute("INSERT INTO thread_tombstones(thread_id,family_key,deleted_at_ns) VALUES ('thread-1','family',1)", []).unwrap();
        db.execute("INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES ('run-1','thread-1','family')", []).unwrap();
    }
}
fn pending() -> PendingQuery {
    PendingQuery {
        thread: None,
        pending_only: true,
        page: PageRequest::default(),
    }
}
fn history(page: PageRequest) -> Query {
    Query::Conversation(ConversationQuery::ReadCommittedHistory(
        ReadCommittedHistory {
            direction: Default::default(),
            thread: ThreadId::new("thread-1").unwrap(),
            run: None,
            page,
        },
    ))
}

#[test]
fn queries_page_by_snapshot_and_refuse_reused_or_stale_cursors() {
    let f = Fixture::new();
    for i in 2..=5 {
        f.thread(&format!("thread-{i}"), &format!("run-{i}"), "next");
    }
    let first = ListThreads {
        project: common::project(),
        page: PageRequest {
            cursor: None,
            limit: 2,
        },
    };
    let QueryReply::Workspace(WorkspaceReply::Threads(a)) =
        f.query(Query::Workspace(WorkspaceQuery::ListThreads(first.clone())))
    else {
        panic!()
    };
    assert_eq!(a.items.len(), 2);
    let mut next = first.clone();
    next.page.cursor = a.next_cursor.clone();
    let QueryReply::Workspace(WorkspaceReply::Threads(b)) =
        f.query(Query::Workspace(WorkspaceQuery::ListThreads(next.clone())))
    else {
        panic!()
    };
    assert_eq!(b.snapshot_revision, a.snapshot_revision);
    assert!(a.items.iter().all(|a| b.items.iter().all(|b| a.id != b.id)));
    let foreign = f
        .host
        .handle_query(
            &SessionId::new("session").unwrap(),
            &Query::Workspace(WorkspaceQuery::ListProjects(next.page.clone())),
        )
        .unwrap_err();
    assert_eq!(foreign.code, ErrorCode::Conflict);
    f.thread("thread-6", "run-6", "new");
    let stale = f
        .host
        .handle_query(
            &SessionId::new("session").unwrap(),
            &Query::Workspace(WorkspaceQuery::ListThreads(next)),
        )
        .unwrap_err();
    assert_eq!(stale.code, ErrorCode::Conflict);
    for limit in [0, 201, u32::MAX] {
        assert!(
            f.host
                .projection()
                .query(&Query::Workspace(WorkspaceQuery::ListProjects(
                    PageRequest {
                        cursor: None,
                        limit
                    }
                )))
                .is_err()
        );
    }
    let QueryReply::Execution(ExecutionReply::Runs(runs)) =
        f.query(Query::Execution(ExecutionQuery::ListRuns(ThreadQuery {
            thread: ThreadId::new("thread-1").unwrap(),
            page: Default::default(),
        })))
    else {
        panic!()
    };
    assert_eq!(runs.items.len(), 1);
    assert_eq!(runs.items[0].role, RunRole::Worker);
    assert!(matches!(
        f.query(Query::Lifecycle(LifecycleQuery::GetCoreStatus)),
        QueryReply::Lifecycle(LifecycleReply::CoreStatus(_))
    ));
}

#[test]
fn committed_bodies_are_lazy_utf8_chunked_and_unavailable_after_deletion() {
    let f = Fixture::new();
    let body = "a😀日本語".repeat(6000);
    let event = Event::new(MessageEvent::MessageCompleted {
        run_id: "run-1".into(),
        text: body.clone(),
    });
    f.store.handle().append_event(None, &event).unwrap();
    let QueryReply::Conversation(ConversationReply::CommittedHistory(page)) =
        f.query(history(Default::default()))
    else {
        panic!()
    };
    assert_eq!(page.items.len(), 1);
    let MessageContent::Body(reference) = &page.items[0].content else {
        panic!("large text must remain deferred")
    };
    let mut offset = ByteOffset(0);
    let mut joined = String::new();
    loop {
        let QueryReply::Projection(ProjectionReply::Body(chunk)) =
            f.query(Query::Projection(ProjectionQuery::ReadBody(ReadBody {
                body_id: reference.body_id.clone(),
                offset,
                max_bytes: 65536,
            })))
        else {
            panic!()
        };
        assert!(chunk.text.len() <= 65536);
        joined.push_str(&chunk.text);
        match chunk.next_offset {
            Some(next) => {
                assert!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    assert_eq!(joined, body);
    let QueryReply::Projection(ProjectionReply::Body(one)) =
        f.query(Query::Projection(ProjectionQuery::ReadBody(ReadBody {
            body_id: reference.body_id.clone(),
            offset: ByteOffset(1),
            max_bytes: 4,
        })))
    else {
        panic!()
    };
    assert_eq!(one.text, "😀");
    assert_eq!(one.next_offset, Some(ByteOffset(5)));
    let QueryReply::Projection(ProjectionReply::Body(eof)) =
        f.query(Query::Projection(ProjectionQuery::ReadBody(ReadBody {
            body_id: reference.body_id.clone(),
            offset: ByteOffset(body.len() as u64),
            max_bytes: 4,
        })))
    else {
        panic!()
    };
    assert!(eof.text.is_empty());
    assert!(eof.next_offset.is_none());
    let invalid = |offset, bytes| {
        f.host
            .projection()
            .query(&Query::Projection(ProjectionQuery::ReadBody(ReadBody {
                body_id: reference.body_id.clone(),
                offset: ByteOffset(offset),
                max_bytes: bytes,
            })))
    };
    assert!(invalid(2, 65536).is_err()); // inside the first emoji
    assert!(invalid(0, 65537).is_err());
    assert!(invalid(0, 3).is_err());
    f.tombstone();
    assert_eq!(invalid(0, 65536).unwrap_err().code, ErrorCode::NotFound);
    assert!(
        f.host
            .projection()
            .query(&history(Default::default()))
            .is_err()
    );
    let QueryReply::Workspace(WorkspaceReply::Threads(threads)) =
        f.query(Query::Workspace(WorkspaceQuery::ListThreads(ListThreads {
            project: common::project(),
            page: Default::default(),
        })))
    else {
        panic!()
    };
    assert!(threads.items.is_empty());
}

#[test]
fn durable_commits_wake_subscribers_and_use_the_history_index_not_bus_deltas() {
    let f = Fixture::new();
    let mut sub = f.subscribe();
    // A bus-only observation is not yet durable and must not become committed history.
    f.runtime
        .event_bus()
        .emit(Event::new(MessageEvent::MessageDelta {
            run_id: Some("run-1".into()),
            delta: "draft".into(),
        }));
    let event = Event::new(MessageEvent::MessageCompleted {
        run_id: "run-1".into(),
        text: "committed".into(),
    });
    f.store.handle().append_event(None, &event).unwrap();
    let envelope = f.rt.block_on(async {
        loop {
            let envelope = sub.recv().await.unwrap();
            if matches!(
                envelope.event,
                UiEvent::Conversation(ConversationEvent::MessageCommitted { .. })
            ) {
                break envelope;
            }
            assert!(!envelope.event.ends_subscription());
        }
    });
    assert!(envelope.cursor.sequence > sub.started.high_water.sequence);
    let UiEvent::Conversation(ConversationEvent::MessageCommitted { message }) = envelope.event
    else {
        panic!()
    };
    let QueryReply::Conversation(ConversationReply::CommittedHistory(page)) =
        f.query(history(Default::default()))
    else {
        panic!()
    };
    assert_eq!(page.items, vec![message]);
    let mut replay = f
        .host
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: Some(sub.started.high_water),
        })
        .unwrap();
    assert!(replay.started.snapshot_id.is_none());
    let replayed = f.rt.block_on(async {
        loop {
            let e = replay.recv().await.unwrap();
            if matches!(
                e.event,
                UiEvent::Conversation(ConversationEvent::MessageCommitted { .. })
            ) {
                break e;
            }
        }
    });
    assert_eq!(replayed.cursor, envelope.cursor);
}

#[test]
fn approval_decision_uses_registry_cas_and_emits_changed_with_ingress_provenance() {
    let mut f = Fixture::new();
    let mut sub = f.subscribe();
    let registry = ApprovalRegistry::for_bus(&f.runtime.event_bus());
    let approval = registry.register(
        ApprovalRequest {
            run_id: "run-1",
            thread_id: Some("thread-1"),
            tool_name: "shell",
            call_id: "call",
            input: Some(serde_json::json!({"command":"echo hi"})),
        },
        Duration::from_secs(3600),
    );
    let QueryReply::Approvals(ApprovalsReply::Approvals(page)) =
        f.query(Query::Approvals(ApprovalsQuery::ListApprovals(pending())))
    else {
        panic!()
    };
    let view = &page.items[0];
    assert!(view.summary.len() <= 1024);
    let complete = f
        .host
        .full_approval_input(&view.id, view.core_epoch, view.revision, &common::web())
        .unwrap()
        .unwrap();
    assert_eq!(complete["command"], "echo hi");
    let command = Command::Approvals(ApprovalsCommand::DecideToolApproval(DecideToolApproval {
        approval_id: view.id.clone(),
        core_epoch: view.core_epoch,
        approval_revision: view.revision,
        decision: ApprovalDecision::Approve,
    }));
    if let Command::Approvals(ApprovalsCommand::DecideToolApproval(decide)) = &command {
        let mut wrong = decide.clone();
        wrong.core_epoch = InstanceEpoch(wrong.core_epoch.get() + 1);
        assert_eq!(
            f.host
                .handle_command(
                    Command::Approvals(ApprovalsCommand::DecideToolApproval(wrong)),
                    common::web()
                )
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        let mut stale = decide.clone();
        stale.approval_revision = Revision(stale.approval_revision.get() + 1);
        assert_eq!(
            f.host
                .handle_command(
                    Command::Approvals(ApprovalsCommand::DecideToolApproval(stale)),
                    common::web()
                )
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
    }
    f.host
        .handle_command(command.clone(), common::web())
        .unwrap();
    assert_eq!(f.rt.block_on(approval.wait()), ApprovalState::Approved);
    assert_eq!(
        f.host
            .handle_command(command, common::cli())
            .unwrap_err()
            .code,
        ErrorCode::AlreadyResolved
    );
    let changed = f.rt.block_on(async {
        loop {
            let e = sub.recv().await.unwrap();
            if let UiEvent::Approvals(ApprovalsEvent::ApprovalChanged { approval }) = e.event
                && approval.state == workbench_protocol::views::ApprovalState::Approved
            {
                break approval;
            }
        }
    });
    assert_eq!(
        changed.decided_by.unwrap().surface,
        workbench_protocol::envelope::Surface::Web
    );
}

#[test]
fn protocol_answer_emits_changed_and_its_receipt_is_hidden_after_requester_deletion() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, store) = common::runtime_with_store(dir.path());
    let question = common::offline_question(&rt, dir.path(), &store);
    store
        .handle()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "thread-1".into(),
            project_id: Some(common::project().to_string()),
            project_root: Some(dir.path().to_string_lossy().into_owned()),
            root_run_id: Some(question.root_run_id.clone()),
            ..Default::default()
        }])
        .unwrap();
    let mut host = common::host_with_project(&rt, runtime, dir.path());
    let QueryReply::Questions(QuestionsReply::Questions(before)) = host
        .projection()
        .query(&Query::Questions(QuestionsQuery::ListQuestions(pending())))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(before.items.len(), 1);
    let mut sub = host
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap();
    let session = SessionId::new("answer-session").unwrap();
    let request = RequestId::new("answer-request").unwrap();
    host.handle_request(
        &session,
        &request,
        Command::Questions(QuestionsCommand::AnswerUserQuestion(AnswerUserQuestion {
            thread: ThreadId::new("thread-1").unwrap(),
            question_id: QuestionId::new(&question.id).unwrap(),
            answer: "JSON".into(),
            question_revision: Revision(0),
        })),
        common::web(),
    )
    .unwrap();
    let changed = rt.block_on(async {
        loop {
            let e = sub.recv().await.unwrap();
            if let UiEvent::Questions(QuestionsEvent::QuestionChanged { question }) = e.event
                && question
                    .answer
                    .as_ref()
                    .and_then(|a| a.answered_by.as_ref())
                    == Some(&common::web())
            {
                break question;
            }
        }
    });
    assert_eq!(changed.answer.unwrap().text, "JSON");
    assert_eq!(
        host.request_provenance(&session, &request),
        Some(common::web())
    );
    let db = rusqlite::Connection::open(dir.path().join("host.db")).unwrap();
    db.execute(
        "INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES (?1,'thread-1','family')",
        [&question.run_id],
    )
    .unwrap();
    assert!(matches!(
        host.command_receipt(&session, &request).unwrap().state,
        CommandReceiptState::Unknown
    ));
    assert_eq!(host.request_provenance(&session, &request), None);
}

#[test]
fn committed_history_pages_keep_their_high_water_while_new_messages_arrive() {
    let f = Fixture::new();
    for text in ["first", "second", "third"] {
        f.store
            .handle()
            .append_event(
                None,
                &Event::new(MessageEvent::MessageCompleted {
                    run_id: "run-1".into(),
                    text: text.into(),
                }),
            )
            .unwrap();
    }
    let QueryReply::Conversation(ConversationReply::CommittedHistory(first)) =
        f.query(history(PageRequest {
            cursor: None,
            limit: 2,
        }))
    else {
        panic!()
    };
    f.store
        .handle()
        .append_event(
            None,
            &Event::new(MessageEvent::MessageCompleted {
                run_id: "run-1".into(),
                text: "fourth".into(),
            }),
        )
        .unwrap();
    let QueryReply::Conversation(ConversationReply::CommittedHistory(next)) =
        f.query(history(PageRequest {
            cursor: first.next_cursor,
            limit: 2,
        }))
    else {
        panic!()
    };
    assert_eq!(next.snapshot_revision, first.snapshot_revision);
    assert_eq!(next.items.len(), 1);
    assert_eq!(next.items[0].index, Sequence(2));
    assert!(next.next_cursor.is_none());
    assert_eq!(
        next.items[0].content,
        MessageContent::Inline {
            text: "third".into()
        }
    );
}

#[test]
fn legacy_questions_discard_stale_routing_payload_and_preserve_unknown_answer_origin() {
    let f = Fixture::new();
    let q = UserQuestion {
        id: "q-1".into(),
        run_id: "run-1".into(),
        root_run_id: "run-1".into(),
        root_name: "chat:Worker:thread-1".into(),
        recipient_run_ids: vec![],
        title: "format?".into(),
        options: vec!["json".into()],
        blocking: true,
        answer: None,
        answer_origin: None,
        responder: Some(event_bus::lane::QuestionResponder::User),
        routing_revision: 7,
        management_revision: Some(3),
    };
    f.store.handle().create_user_question(&q).unwrap();
    let mut sub = f.subscribe();
    f.runtime.answer_user_question("q-1", "json").unwrap();
    let answered = f.rt.block_on(async {
        loop {
            let e = sub.recv().await.unwrap();
            if let UiEvent::Questions(QuestionsEvent::QuestionChanged { question }) = e.event
                && question.answer.is_some()
            {
                break question;
            }
        }
    });
    assert_eq!(answered.requester.thread.as_str(), "thread-1");
    assert_eq!(answered.routing_revision, Revision(0));
    assert_eq!(answered.management_revision, Revision(0));
    assert_eq!(answered.answer.unwrap().answered_by, None);
    let QueryReply::Questions(QuestionsReply::Questions(pending)) =
        f.query(Query::Questions(QuestionsQuery::ListQuestions(pending())))
    else {
        panic!()
    };
    assert!(pending.items.is_empty());
}

#[test]
fn snapshot_pages_remain_frozen_and_tombstones_revoke_old_pages_and_replay() {
    let f = Fixture::new();
    for i in 2..=30 {
        f.thread(&format!("thread-{i}"), &format!("run-{i}"), "next");
    }
    let mut sub = f.subscribe();
    let id = sub.started.snapshot_id.clone().unwrap();
    let page = sub.started.initial_page.as_ref().unwrap();
    assert!(page.next_cursor.is_some());
    f.thread("later", "run-later", "not in frozen snapshot");
    let next = f
        .host
        .projection()
        .snapshot_page(
            &id,
            &PageRequest {
                limit: 200,
                cursor: page.next_cursor.clone(),
            },
        )
        .unwrap();
    assert_eq!(next.snapshot_revision, page.snapshot_revision);
    assert!(
        next.items
            .iter()
            .all(|i| !matches!(i, SnapshotItem::Thread(t) if t.id.as_str() == "later"))
    );
    f.tombstone();
    assert_eq!(
        f.host
            .projection()
            .snapshot_page(&id, &PageRequest::default())
            .unwrap_err()
            .code,
        ErrorCode::ResyncRequired
    );
    let terminal = f.rt.block_on(sub.recv()).unwrap();
    assert!(matches!(
        terminal.event,
        UiEvent::Projection(ProjectionEvent::ResyncRequired {
            reason: ResyncReason::EpochChanged
        })
    ));
    assert!(f.rt.block_on(sub.recv()).is_none());
    let mut resumed = f
        .host
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: Some(sub.started.high_water),
        })
        .unwrap();
    assert!(
        resumed
            .started
            .initial_page
            .as_ref()
            .unwrap()
            .items
            .iter()
            .all(|i| !matches!(i, SnapshotItem::Thread(t) if t.id.as_str() == "thread-1"))
    );
    assert!(matches!(
        f.rt.block_on(resumed.recv()).unwrap().event,
        UiEvent::Projection(ProjectionEvent::ResyncRequired { .. })
    ));
}

#[test]
fn lane_questions_use_durable_requester_and_user_or_lead_routing() {
    use storage::{lane_delivery::*, lanes::*};
    let f = Fixture::new();
    f.thread("lead", "lead-root", "lead");
    for (i, policy) in [(1, LanePolicy::Attached), (2, LanePolicy::Detached)] {
        let thread = format!("lane-thread-{i}");
        let run = format!("lane-run-{i}");
        f.thread(&thread, &run, "lane");
        f.store
            .handle()
            .create_lane(NewLane {
                lane_id: format!("lane-{i}"),
                thread_id: thread.clone(),
                origin_thread_id: "lead".into(),
                origin_run_id: "lead-root".into(),
                creation_key: format!("key-{i}"),
                brief: LaneBrief {
                    verification_requirements: Default::default(),
                    objective: "test".into(),
                    inputs: vec![],
                    deliverables: vec![],
                    acceptance_criteria: vec!["routing is visible".into()],
                    constraints: vec![],
                    workspace: Default::default(),
                    report_policy: policy,
                },
                start_seed: "seed".into(),
                project_id: Some(common::project().to_string()),
                project_root: Some(f.dir.path().to_string_lossy().into_owned()),
                title: None,
                at_ns: 1,
            })
            .unwrap();
        let q = UserQuestion {
            id: format!("q-lane-{i}"),
            run_id: run.clone(),
            root_run_id: run.clone(),
            root_name: "lane".into(),
            recipient_run_ids: vec![],
            title: "decide?".into(),
            options: vec![],
            blocking: true,
            answer: None,
            answer_origin: None,
            responder: None,
            routing_revision: 0,
            management_revision: None,
        };
        f.store
            .handle()
            .create_lane_question(
                format!("lane-{i}"),
                q.clone(),
                QuestionRequester {
                    run_id: run.clone(),
                    root_run_id: run,
                    thread_id: thread,
                },
                LaneDeliveryBinding::Bound,
                2,
            )
            .unwrap();
    }
    let QueryReply::Questions(QuestionsReply::Questions(page)) =
        f.query(Query::Questions(QuestionsQuery::ListQuestions(pending())))
    else {
        panic!()
    };
    assert_eq!(page.items.len(), 2);
    for q in &page.items {
        assert_eq!(q.routing_revision, Revision(1));
        assert_eq!(q.management_revision, Revision(1));
        assert_eq!(
            q.raised_by,
            workbench_protocol::envelope::Provenance::agent(q.requester.run.clone())
        );
    }
    assert!(
        matches!(&page.items[0].target, QuestionTarget::Lead { lead_thread, lead_run } if lead_thread.as_str() == "lead" && lead_run.as_str() == "lead-root")
    );
    assert_eq!(page.items[0].requester.thread.as_str(), "lane-thread-1");
    assert_eq!(page.items[1].target, QuestionTarget::User);
    let lead = f
        .host
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::Thread {
                thread: ThreadId::new("lead").unwrap(),
            },
            after: None,
        })
        .unwrap();
    assert!(
        lead.started
            .initial_page
            .unwrap()
            .items
            .iter()
            .any(|i| matches!(i, SnapshotItem::Question(q) if q.id.as_str() == "q-lane-1"))
    );
}

#[test]
fn input_receipt_query_and_changed_event_share_provenance_and_hide_tombstones() {
    let f = Fixture::new();
    let mut sub = f.subscribe();
    f.store
        .handle()
        .append_host_input_accepted(
            &[],
            &[],
            None,
            &storage::HostInputReceipt {
                provenance: storage::HostInputProvenance::User,
                prompt_id: "submission".into(),
                run_id: "run-1".into(),
                root_run_id: "run-1".into(),
                text: "hello".into(),
                image_count: 0,
                status: storage::HostInputStatus::Accepted,
                reason: None,
                accepted_at_ns: 1,
                status_at_ns: 1,
                evidence_id: None,
            },
        )
        .unwrap();
    f.store
        .handle()
        .record_host_chat_submission(&storage::HostChatSubmission {
            submission_id: "submission".into(),
            fingerprint: "digest".into(),
            run_id: "run-1".into(),
            provenance_json: serde_json::to_string(&common::web()).unwrap(),
            accepted_at_ns: 1,
        })
        .unwrap();
    let query = Query::Conversation(ConversationQuery::GetInputReceipts(ThreadQuery {
        thread: ThreadId::new("thread-1").unwrap(),
        page: Default::default(),
    }));
    let QueryReply::Conversation(ConversationReply::InputReceipts(page)) = f.query(query.clone())
    else {
        panic!()
    };
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].sent_by, common::web());
    let changed = f.rt.block_on(async {
        loop {
            if let UiEvent::Conversation(ConversationEvent::InputReceiptChanged { receipt }) =
                sub.recv().await.unwrap().event
            {
                break receipt;
            }
        }
    });
    assert_eq!(changed, page.items[0]);
    f.store
        .handle()
        .mark_host_inputs_unknown("run-1", "ended")
        .unwrap();
    let QueryReply::Conversation(ConversationReply::InputReceipts(next)) = f.query(query.clone())
    else {
        panic!()
    };
    assert_eq!(next.items[0].state, InputReceiptState::Unknown);
    assert_ne!(next.snapshot_revision, page.snapshot_revision);
    let QueryReply::Workspace(WorkspaceReply::Thread(thread)) =
        f.query(Query::Workspace(WorkspaceQuery::GetThread(ThreadTarget {
            thread: ThreadId::new("thread-1").unwrap(),
        })))
    else {
        panic!()
    };
    assert_eq!(thread.root_run.unwrap().as_str(), "run-1");
    f.tombstone();
    let QueryReply::Conversation(ConversationReply::InputReceipts(deleted)) = f.query(query) else {
        panic!()
    };
    assert!(deleted.items.is_empty());
}

fn saved_event(f: &Fixture, at: u64, kind: impl Into<event_bus::EventKind>) -> i64 {
    let mut event = Event::new(kind);
    event.meta.wall_clock = std::time::UNIX_EPOCH + Duration::from_nanos(at);
    f.store.handle().append_event(None, &event).unwrap();
    f.runtime
        .with_storage_reader(|db| db.last_event_id())
        .unwrap()
        .unwrap()
}
fn input(f: &Fixture, id: &str, text: &str, at: i64, status: storage::HostInputStatus) {
    input_for(f, "run-1", id, text, at, status);
}
fn input_for(
    f: &Fixture,
    run: &str,
    id: &str,
    text: &str,
    at: i64,
    status: storage::HostInputStatus,
) {
    f.store
        .handle()
        .append_host_input_accepted(
            &[],
            &[],
            None,
            &storage::HostInputReceipt {
                provenance: storage::HostInputProvenance::User,
                prompt_id: id.into(),
                run_id: run.into(),
                root_run_id: run.into(),
                text: text.into(),
                image_count: 0,
                status,
                reason: None,
                accepted_at_ns: at,
                status_at_ns: at,
                evidence_id: None,
            },
        )
        .unwrap();
}
fn messages(f: &Fixture) -> Vec<CommittedMessage> {
    let QueryReply::Conversation(ConversationReply::CommittedHistory(page)) =
        f.query(history(PageRequest {
            cursor: None,
            limit: 200,
        }))
    else {
        panic!()
    };
    assert!(page.next_cursor.is_none());
    page.items
}
fn next_message(
    f: &Fixture,
    sub: &mut workbench_host::subscriptions::Subscription,
) -> CommittedMessage {
    f.rt.block_on(async {
        loop {
            match sub.recv().await.expect("open subscription").event {
                UiEvent::Conversation(ConversationEvent::MessageCommitted { message }) => {
                    break message;
                }
                UiEvent::Projection(ProjectionEvent::ResyncRequired { reason }) => {
                    panic!("unexpected resync: {reason:?}")
                }
                _ => {}
            }
        }
    })
}
#[test]
fn finish_is_committed_once_and_history_matches_stream_indices() {
    let f = Fixture::new();
    let mut sub = f.subscribe();
    saved_event(
        &f,
        5,
        MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: String::new(),
        },
    );
    saved_event(
        &f,
        10,
        MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: "answer".into(),
        },
    );
    f.host.projection().refresh().unwrap();
    let first = next_message(&f, &mut sub);
    saved_event(
        &f,
        20,
        MessageEvent::FinalResultPublished {
            run_id: "run-1".into(),
            text: "answer".into(),
        },
    );
    saved_event(
        &f,
        30,
        MessageEvent::FinalResultPublished {
            run_id: "run-1".into(),
            text: String::new(),
        },
    );
    saved_event(
        &f,
        40,
        MessageEvent::FinalResultPublished {
            run_id: "run-1".into(),
            text: "finish only".into(),
        },
    );
    f.host.projection().refresh().unwrap();
    let finish = next_message(&f, &mut sub);
    assert_eq!(finish.role, MessageRole::Assistant);
    assert_eq!(finish.index, Sequence(1));
    assert_eq!(
        finish.content,
        MessageContent::Inline {
            text: "finish only".into()
        }
    );
    assert_eq!(messages(&f), vec![first, finish]);
}
#[test]
fn delivered_followups_merge_by_acceptance_order_and_deduplicate_initial_prompt() {
    let f = Fixture::new();
    let mut sub = f.subscribe();
    input(
        &f,
        "initial",
        "hello",
        10,
        storage::HostInputStatus::Delivered,
    );
    saved_event(
        &f,
        20,
        event_bus::LifecycleEvent::TaskPromptPublished {
            run_id: "run-1".into(),
            agent_name: "Worker".into(),
            role: "Worker".into(),
            parent_run_id: None,
            prompt: "expanded".into(),
            user_text: Some("hello".into()),
            skills: vec![],
        },
    );
    saved_event(
        &f,
        30,
        MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: "first answer".into(),
        },
    );
    f.host.projection().refresh().unwrap();
    let first = next_message(&f, &mut sub);
    let answer = next_message(&f, &mut sub);
    input(
        &f,
        "second",
        "followup",
        40,
        storage::HostInputStatus::Delivered,
    );
    input(
        &f,
        "third",
        "hello",
        50,
        storage::HostInputStatus::Delivered,
    );
    for (id, status) in [
        ("pending", storage::HostInputStatus::Accepted),
        ("cancelled", storage::HostInputStatus::Withdrawn),
        ("dropped", storage::HostInputStatus::Dropped),
        ("unknown", storage::HostInputStatus::Unknown),
    ] {
        input(&f, id, id, 60, status);
    }
    // A durable Delivered receipt is enough, even without a saved bus notification.
    // The body-free notification wakes projection; its payload is not the body.
    f.runtime
        .event_bus()
        .emit(Event::new(event_bus::LifecycleEvent::HostInputDelivered {
            run_id: "run-1".into(),
            prompt_id: "second".into(),
        }));
    let second = next_message(&f, &mut sub);
    let third = next_message(&f, &mut sub);
    assert_eq!(
        [first.index, answer.index, second.index, third.index],
        [Sequence(0), Sequence(1), Sequence(2), Sequence(3)]
    );
    assert_eq!(
        second.content,
        MessageContent::Inline {
            text: "followup".into()
        }
    );
    assert_eq!(
        third.content,
        MessageContent::Inline {
            text: "hello".into()
        }
    );
    assert_eq!(messages(&f), vec![first, answer, second, third]);
}
#[test]
fn late_delivery_resynchronizes_instead_of_silently_reindexing_history() {
    let f = Fixture::new();
    input(
        &f,
        "late",
        "followup",
        10,
        storage::HostInputStatus::Accepted,
    );
    saved_event(
        &f,
        20,
        MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: "answer".into(),
        },
    );
    let mut sub = f.subscribe();
    let epoch = sub.started.high_water.core_epoch;
    f.store
        .handle()
        .transition_host_inputs(&[storage::HostInputTransition {
            prompt_id: "late".into(),
            status: storage::HostInputStatus::Delivered,
            reason: None,
        }])
        .unwrap();
    f.host.projection().refresh().unwrap();
    let resync = f.rt.block_on(async {
        loop {
            let e = sub.recv().await.unwrap();
            if matches!(
                e.event,
                UiEvent::Projection(ProjectionEvent::ResyncRequired { .. })
            ) {
                break e;
            }
        }
    });
    assert!(resync.cursor.core_epoch > epoch);
    assert!(f.rt.block_on(sub.recv()).is_none());
    let history = messages(&f);
    assert_eq!(history[0].role, MessageRole::User);
    assert_eq!(history[1].index, Sequence(1));
}
#[test]
fn history_cursor_rejects_retained_row_removal_or_deleted_run() {
    for direction in [HistoryDirection::Forward, HistoryDirection::Backward] {
        let history = |page| {
            let Query::Conversation(ConversationQuery::ReadCommittedHistory(mut q)) = history(page)
            else {
                unreachable!()
            };
            q.direction = direction;
            Query::Conversation(ConversationQuery::ReadCommittedHistory(q))
        };
        for delete_run in [false, true] {
            let f = Fixture::new();
            let first_id = saved_event(
                &f,
                10,
                MessageEvent::MessageCompleted {
                    run_id: "run-1".into(),
                    text: "first".into(),
                },
            );
            saved_event(
                &f,
                20,
                MessageEvent::MessageCompleted {
                    run_id: "run-1".into(),
                    text: "second".into(),
                },
            );
            if delete_run {
                f.thread("thread-1", "run-2", "First");
                saved_event(
                    &f,
                    30,
                    MessageEvent::MessageCompleted {
                        run_id: "run-2".into(),
                        text: "third".into(),
                    },
                );
            }
            let QueryReply::Conversation(ConversationReply::CommittedHistory(page)) =
                f.query(history(PageRequest {
                    cursor: None,
                    limit: 1,
                }))
            else {
                panic!()
            };
            let db = rusqlite::Connection::open(f.dir.path().join("host.db")).unwrap();
            if delete_run {
                db.execute("INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES ('run-1','thread-1','family')", []).unwrap();
            } else {
                db.execute("DELETE FROM events WHERE id=?1", [first_id])
                    .unwrap();
            }
            let err = f
                .host
                .handle_query(
                    &SessionId::new("session").unwrap(),
                    &history(PageRequest {
                        cursor: page.next_cursor,
                        limit: 1,
                    }),
                )
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Conflict);
        }
    }
}

#[test]
fn large_delivered_input_is_deferred_and_initial_prompt_still_deduplicates() {
    let f = Fixture::new();
    let body = "😀".repeat(20_000);
    input(
        &f,
        "initial-large",
        &body,
        10,
        storage::HostInputStatus::Delivered,
    );
    saved_event(
        &f,
        20,
        event_bus::LifecycleEvent::TaskPromptPublished {
            run_id: "run-1".into(),
            agent_name: "Worker".into(),
            role: "Worker".into(),
            parent_run_id: None,
            prompt: body.clone(),
            user_text: Some(body.clone()),
            skills: vec![],
        },
    );
    input(
        &f,
        "inline-boundary",
        &"a".repeat(16384),
        30,
        storage::HostInputStatus::Delivered,
    );
    input(
        &f,
        "deferred-boundary",
        &"b".repeat(16385),
        40,
        storage::HostInputStatus::Delivered,
    );
    let headers = f
        .runtime
        .with_storage_reader(|db| db.delivered_host_input_history("run-1"))
        .unwrap()
        .unwrap();
    assert_eq!(headers.len(), 3);
    assert_eq!(headers[1].text.as_ref().map(String::len), Some(16384));
    assert!(headers[2].text.is_none());
    assert!(headers[0].text.is_none());
    assert_eq!(headers[0].text_bytes, body.len() as u64);
    let items = messages(&f);
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].role, MessageRole::User);
    assert!(matches!(&items[1].content, MessageContent::Inline { text } if text.len() == 16384));
    assert!(matches!(&items[2].content, MessageContent::Body(_)));
    let MessageContent::Body(reference) = &items[0].content else {
        panic!("large receipt is deferred")
    };
    let QueryReply::Projection(ProjectionReply::Body(chunk)) =
        f.query(Query::Projection(ProjectionQuery::ReadBody(ReadBody {
            body_id: reference.body_id.clone(),
            offset: ByteOffset(0),
            max_bytes: 65536,
        })))
    else {
        panic!()
    };
    assert_eq!(chunk.text, body[..65536]);
    assert_eq!(chunk.next_offset, Some(ByteOffset(65536)));
    f.tombstone();
    let err = f
        .host
        .projection()
        .query(&Query::Projection(ProjectionQuery::ReadBody(ReadBody {
            body_id: reference.body_id.clone(),
            offset: ByteOffset(0),
            max_bytes: 65536,
        })))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[test]
fn approval_registered_and_decided_between_refreshes_still_emits_changed() {
    for web in [true, false] {
        // The current-thread executor cannot consume registration notifications
        // until recv() is awaited: both canonical mutations precede any refresh.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut f = Fixture::with_runtime(rt);
        let mut sub = f.subscribe();
        let registry = ApprovalRegistry::for_bus(&f.runtime.event_bus());
        let approval = registry.register(
            ApprovalRequest {
                run_id: "run-1",
                thread_id: Some("thread-1"),
                tool_name: "shell",
                call_id: "immediate",
                input: None,
            },
            Duration::from_secs(3600),
        );
        let pending = registry.get(approval.id()).unwrap();
        if web {
            f.host
                .handle_request(
                    &SessionId::new("immediate-session").unwrap(),
                    &RequestId::new("immediate-request").unwrap(),
                    Command::Approvals(ApprovalsCommand::DecideToolApproval(DecideToolApproval {
                        approval_id: ApprovalId::new(&pending.id.value).unwrap(),
                        core_epoch: InstanceEpoch(pending.id.core_epoch),
                        approval_revision: Revision(pending.revision),
                        decision: ApprovalDecision::Approve,
                    })),
                    common::web(),
                )
                .unwrap();
        } else {
            registry
                .resolve(
                    approval.id(),
                    pending.revision,
                    sandbox::approval::ApprovalDecision::Approve,
                    sandbox::approval::ApprovalProvenance::DESKTOP,
                )
                .unwrap();
        }
        let changed = f.rt.block_on(async {
            loop {
                let e = sub.recv().await.unwrap();
                if let UiEvent::Approvals(ApprovalsEvent::ApprovalChanged { approval }) = e.event
                    && approval.state == workbench_protocol::views::ApprovalState::Approved
                {
                    break approval;
                }
            }
        });
        assert_eq!(changed.id.as_str(), pending.id.value);
        assert_eq!(
            changed.decided_by.unwrap().surface,
            if web {
                workbench_protocol::envelope::Surface::Web
            } else {
                workbench_protocol::envelope::Surface::Desktop
            }
        );
        assert_eq!(f.rt.block_on(approval.wait()), ApprovalState::Approved);
    }
}

#[test]
fn deferred_multipart_finish_retains_distinct_descriptor_shapes_without_prereading_bodies() {
    let f = Fixture::new();
    let mut sub = f.subscribe();
    let meter = read_meter(&f);
    let part = "片".repeat(10_000);
    for index in 0..2 {
        saved_event(
            &f,
            10 + index as u64,
            MessageEvent::MessagePart {
                run_id: "run-1".into(),
                response_id: "large-response".into(),
                text: part.clone(),
                index,
                total: 2,
            },
        );
    }
    let committed = next_message(&f, &mut sub);
    saved_event(
        &f,
        12,
        MessageEvent::FinalResultPublished {
            run_id: "run-1".into(),
            text: part.repeat(2),
        },
    );
    let finish = next_message(&f, &mut sub);
    assert_eq!(finish.index, Sequence(1));
    let page = messages(&f);
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].run, committed.run);
    assert_eq!(page[0].index, committed.index);
    assert_eq!(page[0].role, committed.role);
    saved_event(
        &f,
        13,
        MessageEvent::FinalResultPublished {
            run_id: "run-1".into(),
            text: "別".repeat(20_000),
        },
    );
    let distinct = next_message(&f, &mut sub);
    assert_eq!(distinct.index.get(), 2);
    assert_eq!(messages(&f).len(), 3);
    assert_eq!(
        meter.counts(),
        storage::test_support::BodyReadCounts::default()
    );
    let MessageContent::Body(reference) = committed.content else {
        panic!()
    };
    let QueryReply::Projection(ProjectionReply::Body(chunk)) =
        f.query(Query::Projection(ProjectionQuery::ReadBody(ReadBody {
            body_id: reference.body_id,
            offset: ByteOffset(0),
            max_bytes: 65536,
        })))
    else {
        panic!()
    };
    assert_eq!(chunk.text, part.repeat(2));
    assert_eq!(
        meter.counts(),
        storage::test_support::BodyReadCounts {
            calls: 2,
            bytes: 60_000
        }
    );
}

fn current_thread_fixture() -> Fixture {
    Fixture::with_runtime(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
    )
}
fn read_meter(f: &Fixture) -> std::sync::Arc<storage::test_support::BodyReadMeter> {
    f.runtime
        .with_storage_reader(|db| Ok(storage::test_support::meter_full_body_reads(db)))
        .unwrap()
        .unwrap()
}
#[test]
fn history_and_subscription_do_not_resolve_missing_or_corrupt_page_external_bodies() {
    for multipart in [false, true] {
        for missing in [false, true] {
            let f = current_thread_fixture();
            saved_event(
                &f,
                5,
                MessageEvent::MessageCompleted {
                    run_id: "run-1".into(),
                    text: "first".into(),
                },
            );
            let part = "片".repeat(10_000);
            if multipart {
                for index in 0..2 {
                    saved_event(
                        &f,
                        10 + index as u64,
                        MessageEvent::MessagePart {
                            run_id: "run-1".into(),
                            response_id: "large".into(),
                            text: part.clone(),
                            index,
                            total: 2,
                        },
                    );
                }
            } else {
                saved_event(
                    &f,
                    10,
                    MessageEvent::MessageCompleted {
                        run_id: "run-1".into(),
                        text: part.repeat(2),
                    },
                );
            }
            saved_event(
                &f,
                12,
                MessageEvent::FinalResultPublished {
                    run_id: "run-1".into(),
                    text: part.repeat(2),
                },
            );
            let db = rusqlite::Connection::open(f.dir.path().join("host.db")).unwrap();
            // Deliberately damage only a content blob, never its event descriptor.
            db.pragma_update(None, "foreign_keys", false).unwrap();
            let damaged = db.execute(if missing {
                "DELETE FROM content_blobs WHERE id=(SELECT MIN(id) FROM content_blobs)"
            } else {
                "UPDATE content_blobs SET data=X'00' WHERE id=(SELECT MIN(id) FROM content_blobs)"
            }, []).unwrap();
            assert_eq!(damaged, 1);
            let meter = read_meter(&f);
            let mut sub = f.subscribe();
            let QueryReply::Conversation(ConversationReply::CommittedHistory(page)) =
                f.query(history(PageRequest {
                    cursor: None,
                    limit: 1,
                }))
            else {
                panic!()
            };
            assert_eq!(page.items.len(), 1);
            assert_eq!(
                page.items[0].content,
                MessageContent::Inline {
                    text: "first".into()
                }
            );
            assert!(page.next_cursor.is_some());
            // Single digest equality still deduplicates a finish whose blob is
            // unavailable; incomparable multipart descriptors remain separate.
            let all = messages(&f);
            assert_eq!(all.len(), if multipart { 3 } else { 2 });
            saved_event(
                &f,
                20,
                MessageEvent::MessageCompleted {
                    run_id: "run-1".into(),
                    text: "after".into(),
                },
            );
            let after = next_message(&f, &mut sub);
            assert_eq!(after.index, Sequence(if multipart { 3 } else { 2 }));
            assert_eq!(
                after.content,
                MessageContent::Inline {
                    text: "after".into()
                }
            );
            assert_eq!(
                meter.counts(),
                storage::test_support::BodyReadCounts::default()
            );
        }
    }
}
#[test]
fn small_history_pages_and_subscription_updates_read_no_large_input_bodies() {
    let f = current_thread_fixture();
    let body = "😀".repeat(20_000);
    for n in 0..3 {
        let run = format!("run-{}", n + 1);
        f.thread("thread-1", &run, "First");
        input_for(
            &f,
            &run,
            &format!("{run}-initial"),
            &body,
            10 + n * 100,
            storage::HostInputStatus::Delivered,
        );
        // Older events without user_text can contain expanded instructions:
        // receipt/run/order identity, not text equality, establishes the link.
        saved_event(
            &f,
            (20 + n * 100) as u64,
            event_bus::LifecycleEvent::TaskPromptPublished {
                run_id: run.clone(),
                agent_name: "Worker".into(),
                role: "Worker".into(),
                parent_run_id: None,
                prompt: "expanded instructions, not the human text".into(),
                user_text: None,
                skills: vec![],
            },
        );
        input_for(
            &f,
            &run,
            &format!("{run}-followup"),
            &body,
            40 + n * 100,
            storage::HostInputStatus::Delivered,
        );
    }
    let meter = read_meter(&f);
    let mut cursor = None;
    let mut items = vec![];
    for _ in 0..6 {
        let QueryReply::Conversation(ConversationReply::CommittedHistory(page)) =
            f.query(history(PageRequest { cursor, limit: 1 }))
        else {
            panic!()
        };
        assert_eq!(page.items.len(), 1);
        cursor = page.next_cursor;
        items.extend(page.items);
    }
    assert!(cursor.is_none());
    assert!(
        items
            .iter()
            .all(|m| m.role == MessageRole::User && matches!(m.content, MessageContent::Body(_)))
    );
    for n in 1..=3 {
        let run = format!("run-{n}");
        let indices: Vec<_> = items
            .iter()
            .filter(|m| m.run.as_str() == run)
            .map(|m| m.index)
            .collect();
        assert_eq!(indices, [Sequence(0), Sequence(1)]);
    }
    let mut sub = f.subscribe();
    input(
        &f,
        "new-followup",
        &body,
        1000,
        storage::HostInputStatus::Delivered,
    );
    f.runtime
        .event_bus()
        .emit(Event::new(event_bus::LifecycleEvent::HostInputDelivered {
            run_id: "run-1".into(),
            prompt_id: "new-followup".into(),
        }));
    let changed = next_message(&f, &mut sub);
    assert_eq!(changed.index, Sequence(2));
    assert!(matches!(changed.content, MessageContent::Body(_)));
    assert_eq!(
        meter.counts(),
        storage::test_support::BodyReadCounts::default()
    );
    // Positive control proves the meter measures actual full bytes, not the
    // bounded wire chunk, when the caller explicitly asks for one body.
    let MessageContent::Body(reference) = &items[0].content else {
        panic!()
    };
    let QueryReply::Projection(ProjectionReply::Body(chunk)) =
        f.query(Query::Projection(ProjectionQuery::ReadBody(ReadBody {
            body_id: reference.body_id.clone(),
            offset: ByteOffset(0),
            max_bytes: 65536,
        })))
    else {
        panic!()
    };
    assert_eq!(chunk.text.len(), 65536);
    assert_eq!(
        meter.counts(),
        storage::test_support::BodyReadCounts {
            calls: 1,
            bytes: body.len() as u64
        }
    );
}
#[test]
fn first_delivered_followup_is_not_mistaken_for_the_runs_initial_receipt() {
    let f = current_thread_fixture();
    input(
        &f,
        "z-initial",
        "initial",
        10,
        storage::HostInputStatus::Accepted,
    );
    input(
        &f,
        "a-followup",
        "followup",
        10,
        storage::HostInputStatus::Delivered,
    );
    saved_event(
        &f,
        20,
        event_bus::LifecycleEvent::TaskPromptPublished {
            run_id: "run-1".into(),
            agent_name: "Worker".into(),
            role: "Worker".into(),
            parent_run_id: None,
            prompt: "initial".into(),
            user_text: Some("initial".into()),
            skills: vec![],
        },
    );
    // Same timestamps and reverse lexical IDs must not override the durable
    // acceptance sequence when determining which submission supplied the task.
    let headers = f
        .runtime
        .with_storage_reader(|db| db.delivered_host_input_history("run-1"))
        .unwrap()
        .unwrap();
    assert_eq!(headers.len(), 1);
    assert!(!headers[0].first_in_run);
    let items = messages(&f);
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0].content,
        MessageContent::Inline {
            text: "followup".into()
        }
    );
    assert_eq!(
        items[1].content,
        MessageContent::Inline {
            text: "initial".into()
        }
    );
}

#[test]
fn receipt_link_deduplicates_initial_task_copies_but_keeps_changed_instructions() {
    let f = current_thread_fixture();
    input(
        &f,
        "initial",
        "human input",
        10,
        storage::HostInputStatus::Delivered,
    );
    for (at, prompt) in [
        (20, "expanded initial task"),
        (30, "changed instructions"),
        (40, "expanded initial task"),
    ] {
        saved_event(
            &f,
            at,
            event_bus::LifecycleEvent::TaskPromptPublished {
                run_id: "run-1".into(),
                agent_name: "Worker".into(),
                role: "Worker".into(),
                parent_run_id: None,
                prompt: prompt.into(),
                user_text: None,
                skills: vec![],
            },
        );
    }
    let items = messages(&f);
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0].content,
        MessageContent::Inline {
            text: "human input".into()
        }
    );
    assert_eq!(
        items[1].content,
        MessageContent::Inline {
            text: "changed instructions".into()
        }
    );
}

fn approval_input_request(ticket: &sandbox::approval::PendingApproval) -> GetApprovalInput {
    GetApprovalInput {
        approval_id: ApprovalId::new(&ticket.id().value).unwrap(),
        core_epoch: InstanceEpoch(ticket.id().core_epoch),
        approval_revision: Revision(1),
        offset: ByteOffset(0),
        max_bytes: 65_536,
    }
}
fn query_error(f: &Fixture, query: Query) -> ErrorCode {
    f.host
        .handle_query(&SessionId::new("session").unwrap(), &query)
        .unwrap_err()
        .code
}
#[test]
fn complete_approval_input_chunks_bind_identity_and_recheck_resolution() {
    let mut f = current_thread_fixture();
    let registry = ApprovalRegistry::for_bus(&f.runtime.event_bus());
    let input = serde_json::json!({"command":"あ".repeat(35_000)});
    let ticket = registry.register(
        ApprovalRequest {
            run_id: "run-1",
            thread_id: Some("thread-1"),
            tool_name: "shell",
            call_id: "large",
            input: Some(input.clone()),
        },
        Duration::from_secs(3600),
    );
    let mut request = approval_input_request(&ticket);
    let QueryReply::Approvals(ApprovalsReply::Approvals(list)) =
        f.query(Query::Approvals(ApprovalsQuery::ListApprovals(pending())))
    else {
        panic!()
    };
    let view = &list.items[0];
    assert!(view.summary_truncated);
    assert_eq!(view.input_bytes.get(), input.to_string().len() as u64);
    let mut complete = String::new();
    let mut second = None;
    loop {
        let QueryReply::Approvals(ApprovalsReply::ApprovalInput(chunk)) = f.query(
            Query::Approvals(ApprovalsQuery::GetApprovalInput(request.clone())),
        ) else {
            panic!()
        };
        QueryReply::Approvals(ApprovalsReply::ApprovalInput(chunk.clone()))
            .validate(&Default::default())
            .unwrap();
        assert_eq!(chunk.approval_id, request.approval_id);
        assert_eq!(chunk.core_epoch, request.core_epoch);
        assert_eq!(chunk.approval_revision, request.approval_revision);
        assert_eq!(chunk.offset.get(), complete.len() as u64);
        assert!(chunk.text.len() <= 65_536);
        complete.push_str(&chunk.text);
        let Some(next) = chunk.next_offset else { break };
        second.get_or_insert(next);
        request.offset = next;
    }
    assert_eq!(complete, input.to_string());
    request.offset = ByteOffset(complete.len() as u64);
    let QueryReply::Approvals(ApprovalsReply::ApprovalInput(eof)) = f.query(Query::Approvals(
        ApprovalsQuery::GetApprovalInput(request.clone()),
    )) else {
        panic!()
    };
    assert!(eof.text.is_empty());
    assert!(eof.next_offset.is_none());
    for (offset, max_bytes, code) in [
        (ByteOffset(13), 65_536, ErrorCode::InvalidRequest),
        (ByteOffset(u64::MAX), 65_536, ErrorCode::InvalidRequest),
        (ByteOffset(0), 3, ErrorCode::InvalidRequest),
        (ByteOffset(0), 65_537, ErrorCode::InvalidRequest),
    ] {
        let mut bad = request.clone();
        bad.offset = offset;
        bad.max_bytes = max_bytes;
        assert_eq!(
            query_error(&f, Query::Approvals(ApprovalsQuery::GetApprovalInput(bad))),
            code
        );
    }
    let mut bad = request.clone();
    bad.core_epoch = InstanceEpoch(request.core_epoch.get() + 1);
    assert_eq!(
        query_error(&f, Query::Approvals(ApprovalsQuery::GetApprovalInput(bad))),
        ErrorCode::StaleEpoch
    );
    let mut bad = request.clone();
    bad.approval_revision = Revision(2);
    assert_eq!(
        query_error(&f, Query::Approvals(ApprovalsQuery::GetApprovalInput(bad))),
        ErrorCode::StaleRevision
    );
    let mut bad = request.clone();
    bad.approval_id = ApprovalId::new("unknown").unwrap();
    assert_eq!(
        query_error(&f, Query::Approvals(ApprovalsQuery::GetApprovalInput(bad))),
        ErrorCode::NotFound
    );
    f.host
        .handle_request(
            &SessionId::new("session").unwrap(),
            &RequestId::new("resolve-input").unwrap(),
            Command::Approvals(ApprovalsCommand::DecideToolApproval(DecideToolApproval {
                approval_id: request.approval_id.clone(),
                core_epoch: request.core_epoch,
                approval_revision: request.approval_revision,
                decision: ApprovalDecision::Deny,
            })),
            common::web(),
        )
        .unwrap();
    request.offset = second.unwrap();
    assert_eq!(
        query_error(
            &f,
            Query::Approvals(ApprovalsQuery::GetApprovalInput(request))
        ),
        ErrorCode::AlreadyResolved
    );
    assert_eq!(f.rt.block_on(ticket.wait()), ApprovalState::Denied);
}

#[test]
fn approval_input_rejects_expired_cancelled_oversized_and_deleted_entries() {
    let f = current_thread_fixture();
    let registry = ApprovalRegistry::for_bus(&f.runtime.event_bus());
    let register = |input, timeout| {
        registry.register(
            ApprovalRequest {
                run_id: "run-1",
                thread_id: Some("thread-1"),
                tool_name: "shell",
                call_id: "input",
                input,
            },
            timeout,
        )
    };
    let expired = register(Some(serde_json::json!("input")), Duration::ZERO);
    assert_eq!(
        query_error(
            &f,
            Query::Approvals(ApprovalsQuery::GetApprovalInput(approval_input_request(
                &expired
            )))
        ),
        ErrorCode::AlreadyResolved
    );
    let cancelled = register(None, Duration::from_secs(3600));
    let request = approval_input_request(&cancelled);
    let QueryReply::Approvals(ApprovalsReply::ApprovalInput(empty)) = f.query(Query::Approvals(
        ApprovalsQuery::GetApprovalInput(request.clone()),
    )) else {
        panic!()
    };
    assert_eq!(empty.total_bytes, ByteOffset(0));
    assert!(empty.text.is_empty());
    drop(cancelled);
    assert_eq!(
        query_error(
            &f,
            Query::Approvals(ApprovalsQuery::GetApprovalInput(request))
        ),
        ErrorCode::AlreadyResolved
    );
    let oversized = register(
        Some(serde_json::json!("a".repeat(1024 * 1024))),
        Duration::from_secs(3600),
    );
    assert_eq!(
        query_error(
            &f,
            Query::Approvals(ApprovalsQuery::GetApprovalInput(approval_input_request(
                &oversized
            )))
        ),
        ErrorCode::LimitExceeded
    );
    let deleted = register(Some(serde_json::json!({"x":1})), Duration::from_secs(3600));
    let request = approval_input_request(&deleted);
    let QueryReply::Approvals(ApprovalsReply::Approvals(list)) =
        f.query(Query::Approvals(ApprovalsQuery::ListApprovals(pending())))
    else {
        panic!()
    };
    assert!(!list.items[0].summary_truncated);
    f.tombstone();
    assert_eq!(
        query_error(
            &f,
            Query::Approvals(ApprovalsQuery::GetApprovalInput(request))
        ),
        ErrorCode::NotFound
    );
}

#[test]
fn snapshot_query_reads_every_frozen_item_after_connection_closure() {
    let f = current_thread_fixture();
    for i in 2..=48 {
        f.thread(&format!("thread-{i}"), &format!("run-{i}"), "next");
    }
    let q = UserQuestion {
        id: "snapshot-question".into(),
        run_id: "run-1".into(),
        root_run_id: "run-1".into(),
        root_name: "chat:Worker:thread-1".into(),
        recipient_run_ids: vec![],
        title: "format?".into(),
        options: vec![],
        blocking: true,
        answer: None,
        answer_origin: None,
        responder: Some(event_bus::lane::QuestionResponder::User),
        routing_revision: 0,
        management_revision: None,
    };
    f.store.handle().create_user_question(&q).unwrap();
    let registry = ApprovalRegistry::for_bus(&f.runtime.event_bus());
    let _approval = registry.register(
        ApprovalRequest {
            run_id: "run-1",
            thread_id: Some("thread-1"),
            tool_name: "shell",
            call_id: "snapshot",
            input: Some(serde_json::json!({"command":"echo hi"})),
        },
        Duration::from_secs(3600),
    );
    let sub = f.subscribe();
    let id = sub.started.snapshot_id.clone().unwrap();
    let high_water = sub.started.high_water;
    let initial = sub.started.initial_page.as_ref().unwrap().clone();
    assert_eq!(initial.items.len(), 50);
    f.host
        .projection()
        .unsubscribe(&sub.started.subscription_id);
    drop(sub);
    f.thread("later", "run-later", "not in this cut");
    let mut items = initial.items;
    let mut cursor = initial.next_cursor;
    while let Some(next) = cursor {
        // A different HTTP session needs no live subscription identifier.
        let QueryReply::Projection(ProjectionReply::Snapshot(reply)) = f
            .host
            .handle_query(
                &SessionId::new("another-connection").unwrap(),
                &Query::Projection(ProjectionQuery::ReadSnapshot(ReadSnapshot {
                    snapshot_id: id.clone(),
                    page: PageRequest {
                        cursor: Some(next),
                        limit: 17,
                    },
                })),
            )
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(reply.snapshot_id, id);
        assert_eq!(reply.high_water, high_water);
        assert_eq!(reply.page.snapshot_revision, initial.snapshot_revision);
        cursor = reply.page.next_cursor;
        items.extend(reply.page.items);
    }
    assert_eq!(items.len(), 100);
    let mut keys = std::collections::BTreeSet::new();
    for item in items {
        let key = match item {
            SnapshotItem::CoreStatus(_) => "status".into(),
            SnapshotItem::Project(p) => format!("project:{}", p.id),
            SnapshotItem::Thread(t) => {
                assert_ne!(t.id.as_str(), "later");
                format!("thread:{}", t.id)
            }
            SnapshotItem::Run(r) => format!("run:{}", r.id),
            SnapshotItem::Question(q) => format!("question:{}", q.id),
            SnapshotItem::Approval(a) => format!("approval:{}", a.id),
        };
        assert!(keys.insert(key), "duplicate snapshot item");
    }
    assert_eq!(keys.iter().filter(|k| k.starts_with("thread:")).count(), 48);
    assert_eq!(
        keys.iter().filter(|k| k.starts_with("question:")).count(),
        1
    );
    assert_eq!(
        keys.iter().filter(|k| k.starts_with("approval:")).count(),
        1
    );
    // Count retention is independent of connection closure too.
    for _ in 0..4 {
        let _ = f.subscribe();
    }
    assert_eq!(
        query_error(
            &f,
            Query::Projection(ProjectionQuery::ReadSnapshot(ReadSnapshot {
                snapshot_id: id,
                page: PageRequest::default()
            }))
        ),
        ErrorCode::ResyncRequired
    );
}

#[test]
fn backward_history_pages_from_tail_are_frozen_chronological_and_metadata_only() {
    let f = current_thread_fixture();
    let meter = read_meter(&f);
    for i in 0..5 {
        saved_event(
            &f,
            10 + i,
            MessageEvent::MessageCompleted {
                run_id: "run-1".into(),
                text: format!("{i}{}", "片".repeat(10_000)),
            },
        );
    }
    let read = |request: ReadCommittedHistory| {
        let QueryReply::Conversation(ConversationReply::CommittedHistory(page)) = f.query(
            Query::Conversation(ConversationQuery::ReadCommittedHistory(request)),
        ) else {
            panic!()
        };
        page
    };
    let mut request = ReadCommittedHistory {
        thread: ThreadId::new("thread-1").unwrap(),
        run: None,
        direction: HistoryDirection::Backward,
        page: PageRequest {
            cursor: None,
            limit: 2,
        },
    };
    let tail = read(request.clone());
    assert_eq!(
        tail.items.iter().map(|m| m.index.get()).collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert!(
        tail.items
            .iter()
            .all(|m| matches!(m.content, MessageContent::Body(_)))
    );
    saved_event(
        &f,
        20,
        MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: "new after cut".into(),
        },
    );
    request.page.cursor = tail.next_cursor;
    let middle = read(request.clone());
    assert_eq!(middle.snapshot_revision, tail.snapshot_revision);
    assert_eq!(
        middle
            .items
            .iter()
            .map(|m| m.index.get())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    let mut wrong = request.clone();
    wrong.direction = HistoryDirection::Forward;
    assert_eq!(
        query_error(
            &f,
            Query::Conversation(ConversationQuery::ReadCommittedHistory(wrong))
        ),
        ErrorCode::Conflict
    );
    request.page.cursor = middle.next_cursor;
    let head = read(request.clone());
    assert_eq!(
        head.items.iter().map(|m| m.index.get()).collect::<Vec<_>>(),
        vec![0]
    );
    assert!(head.next_cursor.is_none());
    assert_eq!(
        meter.counts(),
        storage::test_support::BodyReadCounts::default()
    );
    request.page.cursor = None;
    assert_eq!(
        read(request.clone()).items.last().unwrap().index,
        Sequence(5)
    );
    f.tombstone();
    assert_eq!(
        query_error(
            &f,
            Query::Conversation(ConversationQuery::ReadCommittedHistory(request))
        ),
        ErrorCode::NotFound
    );
}

#[test]
fn approval_input_masks_before_chunking_and_accepts_exactly_one_mib() {
    let f = current_thread_fixture();
    let registry = ApprovalRegistry::for_bus(&f.runtime.event_bus());
    let secret = format!("sk-{}", "a".repeat(32));
    let raw = serde_json::json!(format!("{} {secret}", "x".repeat(65_520)));
    let ticket = registry.register(
        ApprovalRequest {
            run_id: "run-1",
            thread_id: Some("thread-1"),
            tool_name: "shell",
            call_id: "mask",
            input: Some(raw),
        },
        Duration::from_secs(3600),
    );
    let mut request = approval_input_request(&ticket);
    let mut complete = String::new();
    loop {
        let QueryReply::Approvals(ApprovalsReply::ApprovalInput(chunk)) = f.query(
            Query::Approvals(ApprovalsQuery::GetApprovalInput(request.clone())),
        ) else {
            panic!()
        };
        complete.push_str(&chunk.text);
        let Some(next) = chunk.next_offset else { break };
        request.offset = next;
    }
    assert!(!complete.contains(&secret));
    assert!(complete.contains("[REDACTED:openai-style-key]"));
    let largest = registry.register(
        ApprovalRequest {
            run_id: "run-1",
            thread_id: Some("thread-1"),
            tool_name: "shell",
            call_id: "maximum",
            input: Some(serde_json::json!("a".repeat(1024 * 1024 - 2))),
        },
        Duration::from_secs(3600),
    );
    let QueryReply::Approvals(ApprovalsReply::ApprovalInput(chunk)) = f.query(Query::Approvals(
        ApprovalsQuery::GetApprovalInput(approval_input_request(&largest)),
    )) else {
        panic!()
    };
    assert_eq!(chunk.total_bytes, ByteOffset(1024 * 1024));
    assert_eq!(chunk.text.len(), 65_536);
}
