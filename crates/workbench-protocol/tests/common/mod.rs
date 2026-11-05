//! Sample values for every wire variant, shared by the integration tests.
#![allow(dead_code)]

use workbench_protocol::commands::*;
use workbench_protocol::envelope::*;
use workbench_protocol::errors::*;
use workbench_protocol::events::*;
use workbench_protocol::handshake::*;
use workbench_protocol::ids::*;
use workbench_protocol::queries::*;
use workbench_protocol::views::*;
use workbench_protocol::wire::*;

pub fn project() -> ProjectId {
    ProjectId::new("proj-1").unwrap()
}
pub fn thread() -> ThreadId {
    ThreadId::new("thr-1").unwrap()
}
pub fn run() -> RunId {
    RunId::new("run-1").unwrap()
}
pub fn request(n: u32) -> RequestId {
    RequestId::new(format!("req-{n}")).unwrap()
}
pub fn session() -> SessionId {
    SessionId::new("sess-1").unwrap()
}
pub fn sub() -> SubscriptionId {
    SubscriptionId::new("sub-1").unwrap()
}
pub fn client() -> ClientId {
    ClientId::new("client-1").unwrap()
}
pub fn human() -> Provenance {
    Provenance::human(Surface::Web, client())
}
pub fn agent() -> Provenance {
    Provenance::agent(run())
}
pub fn cursor(epoch: u64, sequence: u64) -> EventCursor {
    EventCursor {
        core_epoch: InstanceEpoch(epoch),
        sequence: Sequence(sequence),
    }
}

pub fn status() -> CoreStatus {
    CoreStatus {
        state: CoreState::Ready,
        retention_reasons: vec![
            RetentionReason::ActiveTurn,
            RetentionReason::ConnectedClients,
        ],
        running: ActivityCounts {
            turns: 1,
            tool_calls: 2,
            waiting_lanes: 0,
            shell_jobs: 1,
            clients: 3,
        },
    }
}

pub fn build() -> BuildInfo {
    BuildInfo {
        name: "evorch".into(),
        version: "0.1.0".into(),
        revision: Some("abc1234".into()),
    }
}

pub fn client_hello() -> ClientHello {
    ClientHello {
        supported_versions: vec![ProtocolVersion::CURRENT],
        required_capabilities: vec![Capability::Commands.into(), Capability::Queries.into()],
        optional_capabilities: vec![Capability::SnapshotEvents.into()],
        client_build: build(),
        client_instance_id: client(),
        desired_core: Some(CoreIdentity::new("core-abc").unwrap()),
    }
}

pub fn server_hello() -> ServerHello {
    ServerHello {
        protocol_version: ProtocolVersion::CURRENT,
        capabilities: Capability::ALL.to_vec(),
        server_build: build(),
        core: CoreIdentity::new("core-abc").unwrap(),
        instance_epoch: InstanceEpoch(7),
        limits: WireLimits::default(),
        status: status(),
    }
}

pub fn rejected() -> HandshakeRejected {
    HandshakeRejected {
        reason: RejectReason::MissingCapabilities,
        message: "missing".into(),
        supported_versions: vec![ProtocolVersion::CURRENT],
        missing_capabilities: vec![CapabilityName::new("approvals.v2").unwrap()],
        recommended_action: RecommendedAction::UpgradeCore,
    }
}

pub fn pref() -> ModelPreference {
    ModelPreference {
        profile: "default".into(),
        model: Some("gpt".into()),
        reasoning_effort: Some("high".into()),
    }
}

pub fn send_chat() -> SendChat {
    SendChat {
        project: project(),
        thread: thread(),
        submission_id: SubmissionId::new("sub-a").unwrap(),
        composer_role: ComposerRole::Orchestrator,
        text: "hello\nworld".into(),
        skills: vec!["git".into()],
        images: vec![ImageRef {
            attachment_id: AttachmentId::new("att-1").unwrap(),
            mime_type: "image/png".into(),
        }],
        model_preference: Some(pref()),
        fork_seed: Some(ForkSeed {
            source_run_id: RunId::new("run-0").unwrap(),
            context_len: MessageCount(9_007_199_254_740_993),
        }),
    }
}

pub fn target() -> ThreadTarget {
    ThreadTarget { thread: thread() }
}

/// One sample per `Command` variant, in `Command::TYPES` order.
pub fn commands() -> Vec<Command> {
    vec![
        Command::SendChat(send_chat()),
        Command::ContinueChat(ContinueChat {
            project: project(),
            thread: thread(),
            composer_role: ComposerRole::Worker,
            model_preference: None,
        }),
        Command::CompactChat(target()),
        Command::StopChat(target()),
        Command::StopRun(StopRun {
            thread: thread(),
            run: run(),
        }),
        Command::DeliverFollowUpsNextTurn(target()),
        Command::WithdrawFollowUp(WithdrawFollowUp {
            thread: thread(),
            run: run(),
            submission_id: SubmissionId::new("sub-a").unwrap(),
        }),
        Command::SubmitGoal(SubmitGoal {
            project: project(),
            thread: thread(),
            objective: "ship it".into(),
            references: vec![PacketReference {
                kind: ReferenceKind::Issue,
                value: "#12".into(),
            }],
            constraints: vec!["no network".into()],
            delegation: Some(Delegation::Reference {
                value: "packet-3".into(),
            }),
        }),
        Command::SetGoalReview(SetGoalReview {
            thread: thread(),
            goal: GoalId::new("goal-1").unwrap(),
            enabled: true,
        }),
        Command::SetGoalChecksPaused(SetGoalChecksPaused {
            thread: thread(),
            goal: GoalId::new("goal-1").unwrap(),
            paused: false,
        }),
        Command::AnswerUserQuestion(AnswerUserQuestion {
            thread: thread(),
            question_id: QuestionId::new("q-1").unwrap(),
            answer: "yes".into(),
            question_revision: Revision(4),
        }),
        Command::DecideToolApproval(DecideToolApproval {
            approval_id: ApprovalId::new("ap-1").unwrap(),
            core_epoch: InstanceEpoch(7),
            approval_revision: Revision(2),
            decision: ApprovalDecision::Deny,
        }),
        Command::SetAgentWebToolsEnabled(SetAgentWebToolsEnabled { enabled: true }),
    ]
}

fn page<T>(items: Vec<T>) -> Page<T> {
    Page {
        items,
        next_cursor: Some(PageCursor::new("cur-2").unwrap()),
        snapshot_revision: Revision(100),
    }
}

pub fn page_req() -> PageRequest {
    PageRequest {
        cursor: Some(PageCursor::new("cur-1").unwrap()),
        limit: 25,
    }
}

pub fn pending() -> PendingQuery {
    PendingQuery {
        thread: Some(thread()),
        pending_only: true,
        page: page_req(),
    }
}

/// One sample per `Query` variant, in `Query::TYPES` order.
pub fn queries() -> Vec<Query> {
    vec![
        Query::GetCoreStatus,
        Query::ListProjects(page_req()),
        Query::ListThreads(ListThreads {
            project: project(),
            page: PageRequest::default(),
        }),
        Query::GetThread(target()),
        Query::ListRuns(ThreadQuery {
            thread: thread(),
            page: page_req(),
        }),
        Query::ListQuestions(pending()),
        Query::ListApprovals(pending()),
        Query::GetInputReceipts(ThreadQuery {
            thread: thread(),
            page: page_req(),
        }),
        Query::GetCommandReceipt {
            request_id: request(3),
        },
        Query::ReadCommittedHistory(ReadCommittedHistory {
            thread: thread(),
            run: Some(run()),
            page: page_req(),
        }),
        Query::ReadBody(ReadBody {
            body_id: BodyId::new("body-1").unwrap(),
            offset: ByteOffset(65_536),
            max_bytes: 65_536,
        }),
    ]
}

pub fn controls() -> Vec<Control> {
    vec![
        Control::Subscribe(Subscribe {
            scope: SubscriptionScope::Thread { thread: thread() },
            after: Some(cursor(7, 41)),
        }),
        Control::Unsubscribe {
            subscription_id: sub(),
        },
        Control::Ping { nonce: Nonce(1) },
        Control::Pong { nonce: Nonce(1) },
    ]
}

pub fn thread_summary() -> ThreadSummary {
    ThreadSummary {
        id: thread(),
        project: project(),
        title: "Fix login".into(),
        state: ThreadState::WaitingForUser,
        lead_thread: Some(ThreadId::new("thr-lead").unwrap()),
        revision: Revision(12),
        updated_at: UnixMillis(1_793_799_134_000),
    }
}

pub fn run_summary() -> RunSummary {
    RunSummary {
        id: run(),
        thread: thread(),
        parent_run: Some(RunId::new("run-0").unwrap()),
        role: RunRole::Worker,
        phase: RunPhase::WaitingForApproval,
        revision: Revision(3),
    }
}

pub fn question(target: QuestionTarget) -> QuestionView {
    QuestionView {
        id: QuestionId::new("q-1").unwrap(),
        requester: QuestionRequester {
            run: run(),
            thread: thread(),
            root_run: RunId::new("run-root").unwrap(),
        },
        target,
        management_revision: Revision(2),
        routing_revision: Revision(1),
        blocking: true,
        title: "Proceed?".into(),
        options: vec!["yes".into(), "no".into()],
        answer: Some(QuestionAnswer {
            text: "yes".into(),
            answered_by: human(),
        }),
        revision: Revision(5),
        raised_by: agent(),
    }
}

pub fn approval() -> ApprovalView {
    ApprovalView {
        id: ApprovalId::new("ap-1").unwrap(),
        core_epoch: InstanceEpoch(7),
        revision: Revision(2),
        thread: thread(),
        run: run(),
        tool_name: "shell".into(),
        summary: "rm -rf build".into(),
        state: ApprovalState::Pending,
        decided_by: Some(human()),
    }
}

pub fn receipt() -> InputReceipt {
    InputReceipt {
        submission_id: SubmissionId::new("sub-a").unwrap(),
        thread: thread(),
        run: Some(run()),
        state: InputReceiptState::Delivered,
        revision: Revision(8),
        sent_by: human(),
    }
}

pub fn message(content: MessageContent) -> CommittedMessage {
    CommittedMessage {
        thread: thread(),
        run: run(),
        index: Sequence(17),
        role: MessageRole::Assistant,
        content,
    }
}

pub fn body_ref() -> BodyRef {
    BodyRef {
        body_id: BodyId::new("body-1").unwrap(),
        total_bytes: ByteOffset(1 << 40),
    }
}

pub fn command_reply() -> CommandReply {
    CommandReply {
        disposition: CommandDisposition::Accepted,
        submission_id: Some(SubmissionId::new("sub-a").unwrap()),
        run_id: Some(run()),
        thread_revision: Some(Revision(13)),
    }
}

/// One sample per `UiEvent` variant, in `UiEvent::TYPES` order.
pub fn ui_events() -> Vec<UiEvent> {
    vec![
        UiEvent::CoreStatusChanged { status: status() },
        UiEvent::ThreadChanged {
            thread: thread_summary(),
        },
        UiEvent::RunChanged { run: run_summary() },
        UiEvent::InputReceiptChanged { receipt: receipt() },
        UiEvent::QuestionChanged {
            question: question(QuestionTarget::Lead {
                lead_thread: ThreadId::new("thr-lead").unwrap(),
                lead_run: RunId::new("run-lead").unwrap(),
            }),
        },
        UiEvent::ApprovalChanged {
            approval: approval(),
        },
        UiEvent::MessageCommitted {
            message: message(MessageContent::Body(body_ref())),
        },
        UiEvent::ResyncRequired {
            reason: ResyncReason::SlowSubscriber,
        },
        UiEvent::SubscriptionClosed {
            reason: CloseReason::CoreShuttingDown,
        },
    ]
}

pub fn event_envelope(event: UiEvent) -> EventEnvelope {
    EventEnvelope {
        subscription_id: sub(),
        cursor: cursor(7, 42),
        caused_by_request_id: Some(request(1)),
        event,
    }
}

pub fn error() -> WorkbenchError {
    WorkbenchError::stale_revision(Revision(1), Revision(2)).for_request(request(1))
}

pub fn query_replies() -> Vec<QueryReply> {
    vec![
        QueryReply::CoreStatus(status()),
        QueryReply::Projects(page(vec![ProjectSummary {
            id: project(),
            display_name: "evorch".into(),
            thread_count: 4,
            revision: Revision(1),
        }])),
        QueryReply::Threads(page(vec![thread_summary()])),
        QueryReply::Thread(ThreadView {
            summary: thread_summary(),
            root_run: Some(run()),
            active_run: None,
            pending_questions: 1,
            pending_approvals: 0,
            queued_follow_ups: 2,
        }),
        QueryReply::Runs(page(vec![run_summary()])),
        QueryReply::Questions(page(vec![question(QuestionTarget::User)])),
        QueryReply::Approvals(page(vec![approval()])),
        QueryReply::InputReceipts(page(vec![receipt()])),
        QueryReply::CommandReceipt(CommandReceipt {
            request_id: request(3),
            state: CommandReceiptState::Applied {
                reply: command_reply(),
            },
        }),
        QueryReply::CommittedHistory(page(vec![
            message(MessageContent::Inline { text: "hi".into() }),
            message(MessageContent::Body(body_ref())),
        ])),
        QueryReply::Body(BodyChunk {
            body_id: BodyId::new("body-1").unwrap(),
            offset: ByteOffset(0),
            text: "héllo".into(),
            total_bytes: ByteOffset(10),
            next_offset: Some(ByteOffset(6)),
        }),
    ]
}

pub fn reply_payloads() -> Vec<ReplyPayload> {
    vec![
        ReplyPayload::Command(command_reply()),
        ReplyPayload::Query(QueryReply::CoreStatus(status())),
        ReplyPayload::SubscriptionStarted(SubscriptionStarted {
            subscription_id: sub(),
            snapshot_id: Some(SnapshotId::new("snap-1").unwrap()),
            high_water: cursor(7, 40),
            initial_page: Some(Page {
                items: vec![
                    SnapshotItem::CoreStatus(status()),
                    SnapshotItem::Thread(thread_summary()),
                ],
                next_cursor: None,
                snapshot_revision: Revision(99),
            }),
        }),
        ReplyPayload::Unsubscribed {
            subscription_id: sub(),
        },
        ReplyPayload::Pong { nonce: Nonce(1) },
        ReplyPayload::Ack,
    ]
}

pub fn envelope(n: u32, body: RequestBody) -> RequestEnvelope {
    RequestEnvelope {
        request_id: request(n),
        session_id: session(),
        body,
    }
}
