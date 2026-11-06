mod common;

use event_bus::{Event, MessageEvent, UserQuestion};
use runtime::AgentRuntime;
use sandbox::approval::{ApprovalRegistry, ApprovalRequest, ApprovalState};
use std::time::Duration;
use workbench_host::CommandHost;
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
        let rt = tokio::runtime::Runtime::new().unwrap();
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
    Query::ReadCommittedHistory(ReadCommittedHistory {
        thread: ThreadId::new("thread-1").unwrap(),
        run: None,
        page,
    })
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
    let QueryReply::Threads(a) = f.query(Query::ListThreads(first.clone())) else {
        panic!()
    };
    assert_eq!(a.items.len(), 2);
    let mut next = first.clone();
    next.page.cursor = a.next_cursor.clone();
    let QueryReply::Threads(b) = f.query(Query::ListThreads(next.clone())) else {
        panic!()
    };
    assert_eq!(b.snapshot_revision, a.snapshot_revision);
    assert!(a.items.iter().all(|a| b.items.iter().all(|b| a.id != b.id)));
    let foreign = f
        .host
        .handle_query(
            &SessionId::new("session").unwrap(),
            &Query::ListProjects(next.page.clone()),
        )
        .unwrap_err();
    assert_eq!(foreign.code, ErrorCode::Conflict);
    f.thread("thread-6", "run-6", "new");
    let stale = f
        .host
        .handle_query(
            &SessionId::new("session").unwrap(),
            &Query::ListThreads(next),
        )
        .unwrap_err();
    assert_eq!(stale.code, ErrorCode::Conflict);
    for limit in [0, 201, u32::MAX] {
        assert!(
            f.host
                .projection()
                .query(&Query::ListProjects(PageRequest {
                    cursor: None,
                    limit
                }))
                .is_err()
        );
    }
    let QueryReply::Runs(runs) = f.query(Query::ListRuns(ThreadQuery {
        thread: ThreadId::new("thread-1").unwrap(),
        page: Default::default(),
    })) else {
        panic!()
    };
    assert_eq!(runs.items.len(), 1);
    assert_eq!(runs.items[0].role, RunRole::Worker);
    assert!(matches!(
        f.query(Query::GetCoreStatus),
        QueryReply::CoreStatus(_)
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
    let QueryReply::CommittedHistory(page) = f.query(history(Default::default())) else {
        panic!()
    };
    assert_eq!(page.items.len(), 1);
    let MessageContent::Body(reference) = &page.items[0].content else {
        panic!("large text must remain deferred")
    };
    let mut offset = ByteOffset(0);
    let mut joined = String::new();
    loop {
        let QueryReply::Body(chunk) = f.query(Query::ReadBody(ReadBody {
            body_id: reference.body_id.clone(),
            offset,
            max_bytes: 65536,
        })) else {
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
    let QueryReply::Body(one) = f.query(Query::ReadBody(ReadBody {
        body_id: reference.body_id.clone(),
        offset: ByteOffset(1),
        max_bytes: 4,
    })) else {
        panic!()
    };
    assert_eq!(one.text, "😀");
    assert_eq!(one.next_offset, Some(ByteOffset(5)));
    let QueryReply::Body(eof) = f.query(Query::ReadBody(ReadBody {
        body_id: reference.body_id.clone(),
        offset: ByteOffset(body.len() as u64),
        max_bytes: 4,
    })) else {
        panic!()
    };
    assert!(eof.text.is_empty());
    assert!(eof.next_offset.is_none());
    let invalid = |offset, bytes| {
        f.host.projection().query(&Query::ReadBody(ReadBody {
            body_id: reference.body_id.clone(),
            offset: ByteOffset(offset),
            max_bytes: bytes,
        }))
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
    let QueryReply::Threads(threads) = f.query(Query::ListThreads(ListThreads {
        project: common::project(),
        page: Default::default(),
    })) else {
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
            if matches!(envelope.event, UiEvent::MessageCommitted { .. }) {
                break envelope;
            }
            assert!(!envelope.event.ends_subscription());
        }
    });
    assert!(envelope.cursor.sequence > sub.started.high_water.sequence);
    let UiEvent::MessageCommitted { message } = envelope.event else {
        panic!()
    };
    let QueryReply::CommittedHistory(page) = f.query(history(Default::default())) else {
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
            if matches!(e.event, UiEvent::MessageCommitted { .. }) {
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
    let QueryReply::Approvals(page) = f.query(Query::ListApprovals(pending())) else {
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
    let command = Command::DecideToolApproval(DecideToolApproval {
        approval_id: view.id.clone(),
        core_epoch: view.core_epoch,
        approval_revision: view.revision,
        decision: ApprovalDecision::Approve,
    });
    if let Command::DecideToolApproval(decide) = &command {
        let mut wrong = decide.clone();
        wrong.core_epoch = InstanceEpoch(wrong.core_epoch.get() + 1);
        assert_eq!(
            f.host
                .handle_command(Command::DecideToolApproval(wrong), common::web())
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        let mut stale = decide.clone();
        stale.approval_revision = Revision(stale.approval_revision.get() + 1);
        assert_eq!(
            f.host
                .handle_command(Command::DecideToolApproval(stale), common::web())
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
            if let UiEvent::ApprovalChanged { approval } = e.event
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
    let QueryReply::Questions(before) = host
        .projection()
        .query(&Query::ListQuestions(pending()))
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
        Command::AnswerUserQuestion(AnswerUserQuestion {
            thread: ThreadId::new("thread-1").unwrap(),
            question_id: QuestionId::new(&question.id).unwrap(),
            answer: "JSON".into(),
            question_revision: Revision(0),
        }),
        common::web(),
    )
    .unwrap();
    let changed = rt.block_on(async {
        loop {
            let e = sub.recv().await.unwrap();
            if let UiEvent::QuestionChanged { question } = e.event
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
    let QueryReply::CommittedHistory(first) = f.query(history(PageRequest {
        cursor: None,
        limit: 2,
    })) else {
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
    let QueryReply::CommittedHistory(next) = f.query(history(PageRequest {
        cursor: first.next_cursor,
        limit: 2,
    })) else {
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
            if let UiEvent::QuestionChanged { question } = e.event
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
    let QueryReply::Questions(pending) = f.query(Query::ListQuestions(pending())) else {
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
        ErrorCode::Conflict
    );
    let terminal = f.rt.block_on(sub.recv()).unwrap();
    assert!(matches!(
        terminal.event,
        UiEvent::ResyncRequired {
            reason: ResyncReason::EpochChanged
        }
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
        UiEvent::ResyncRequired { .. }
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
    let QueryReply::Questions(page) = f.query(Query::ListQuestions(pending())) else {
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
    let query = Query::GetInputReceipts(ThreadQuery {
        thread: ThreadId::new("thread-1").unwrap(),
        page: Default::default(),
    });
    let QueryReply::InputReceipts(page) = f.query(query.clone()) else {
        panic!()
    };
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].sent_by, common::web());
    let changed = f.rt.block_on(async {
        loop {
            if let UiEvent::InputReceiptChanged { receipt } = sub.recv().await.unwrap().event {
                break receipt;
            }
        }
    });
    assert_eq!(changed, page.items[0]);
    f.store
        .handle()
        .mark_host_inputs_unknown("run-1", "ended")
        .unwrap();
    let QueryReply::InputReceipts(next) = f.query(query.clone()) else {
        panic!()
    };
    assert_eq!(next.items[0].state, InputReceiptState::Unknown);
    assert_ne!(next.snapshot_revision, page.snapshot_revision);
    let QueryReply::Thread(thread) = f.query(Query::GetThread(ThreadTarget {
        thread: ThreadId::new("thread-1").unwrap(),
    })) else {
        panic!()
    };
    assert_eq!(thread.root_run.unwrap().as_str(), "run-1");
    f.tombstone();
    let QueryReply::InputReceipts(deleted) = f.query(query) else {
        panic!()
    };
    assert!(deleted.items.is_empty());
}
