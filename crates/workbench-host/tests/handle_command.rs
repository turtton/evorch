//! `CommandHost::handle_command` against a real runtime: the checks every
//! ingress shares, submission deduplication, receipts and provenance.

mod common;

use std::sync::Arc;

use common::{cli, host_with_project, runtime, runtime_on_store, runtime_with_store, send, web};
use event_bus::EventBus;
use workbench_protocol::commands::{
    AnswerUserQuestion, ApprovalDecision, Command, CommandDisposition, DecideToolApproval,
    Delegation, ImageRef, SetAgentWebToolsEnabled, StopRun, SubmitGoal, ThreadTarget,
};
use workbench_protocol::envelope::{Actor, Provenance, Surface};
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::ids::{
    ApprovalId, AttachmentId, QuestionId, RequestId, RunId, SessionId, ThreadId,
};
use workbench_protocol::queries::{CommandReceiptState, Query, QueryReply};
use workbench_protocol::wire::{InstanceEpoch, Revision};

fn thread(id: &str) -> ThreadId {
    ThreadId::new(id).unwrap()
}

#[test]
fn a_project_the_host_does_not_know_is_not_found() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());
    let mut message = send("thread-1", "sub-1", "hello");
    message.project = workbench_protocol::ids::ProjectId::new("elsewhere").unwrap();

    let error = host
        .handle_command(Command::SendChat(message), web())
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::NotFound);
    assert!(host.service().runtime().list_agents().is_empty());
}

#[test]
fn a_new_message_is_accepted_and_remembers_who_sent_it() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());

    let reply = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();

    assert_eq!(reply.disposition, CommandDisposition::Accepted);
    assert_eq!(reply.submission_id.as_ref().unwrap().as_str(), "sub-1");
    assert!(reply.run_id.is_some());
    assert_eq!(host.submission_provenance("sub-1"), Some(web()));
    assert!(host.service().bindings.chat_runs.contains_key("thread-1"));
}

#[test]
fn resending_the_same_submission_changes_nothing_and_a_different_payload_conflicts() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());
    let first = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    let agents = host.service().runtime().list_agents().len();

    let again = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    let other_sender = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), cli())
        .unwrap();
    let conflict = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "bye")), web())
        .unwrap_err();

    assert_eq!(again.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(again.run_id, first.run_id);
    assert_eq!(other_sender.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(conflict.code, ErrorCode::Conflict);
    assert_eq!(host.service().runtime().list_agents().len(), agents);
    assert_eq!(
        host.submission_provenance("sub-1"),
        Some(web()),
        "a resend does not change who sent the original"
    );
}

#[test]
fn a_second_message_to_a_running_thread_is_a_follow_up_for_every_surface() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, _storage) = runtime_with_store(dir.path());
    let mut host = host_with_project(&rt, runtime, dir.path());
    let first = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "start")), web())
        .unwrap();

    let second = host
        .handle_command(Command::SendChat(send("thread-1", "sub-2", "more")), cli())
        .unwrap();

    assert_eq!(second.disposition, CommandDisposition::Accepted);
    assert_eq!(second.run_id, first.run_id);
    assert_eq!(host.service().runtime().list_agents().len(), 1);
    assert_eq!(host.submission_provenance("sub-2"), Some(cli()));
}

#[test]
fn a_rejected_send_is_not_remembered_and_may_be_retried() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());
    let mut with_image = send("thread-1", "sub-1", "look");
    with_image.images = vec![ImageRef {
        attachment_id: AttachmentId::new("att-1").unwrap(),
        mime_type: "image/png".into(),
    }];

    let error = host
        .handle_command(Command::SendChat(with_image), web())
        .unwrap_err();
    let retry = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "look")), web())
        .unwrap();

    assert_eq!(error.code, ErrorCode::UnsupportedCapability);
    assert_eq!(retry.disposition, CommandDisposition::Accepted);
}

#[test]
fn losing_thread_ownership_refuses_the_command_before_it_reaches_the_runtime() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());
    host.handle_command(Command::SendChat(send("thread-1", "sub-1", "start")), web())
        .unwrap();
    let owners = tempfile::tempdir().unwrap();
    let bus = Arc::new(EventBus::new(64));
    let owner = runtime::ownership::OwnerHost::open(owners.path(), Default::default(), bus.clone())
        .unwrap();
    owner.start("thread-1").unwrap();
    let permit = owner.owned_permit("thread-1").unwrap();
    let successor =
        runtime::ownership::OwnerHost::open(owners.path(), Default::default(), bus).unwrap();
    owner.handoff(&permit, &successor).unwrap();
    host.service_mut().set_ownership(Some(Arc::new(owner)));
    let run = host.service().bindings.chat_runs["thread-1"];

    for command in [
        Command::SendChat(send("thread-1", "sub-2", "more")),
        Command::StopChat(ThreadTarget {
            thread: thread("thread-1"),
        }),
        Command::CompactChat(ThreadTarget {
            thread: thread("thread-1"),
        }),
        Command::StopRun(StopRun {
            thread: thread("thread-1"),
            run: RunId::new(run.to_string()).unwrap(),
        }),
    ] {
        let error = host.handle_command(command, web()).unwrap_err();
        assert_eq!(error.code, ErrorCode::PermissionDenied);
    }
    assert!(host.submission_provenance("sub-2").is_none());
}

#[test]
fn stopping_a_thread_with_nothing_running_is_a_conflict_not_a_success() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());

    let stop_chat = host
        .handle_command(
            Command::StopChat(ThreadTarget {
                thread: thread("missing"),
            }),
            web(),
        )
        .unwrap_err();
    let stop_run = host
        .handle_command(
            Command::StopRun(StopRun {
                thread: thread("missing"),
                run: RunId::new("run-1").unwrap(),
            }),
            web(),
        )
        .unwrap_err();
    let deliver = host
        .handle_command(
            Command::DeliverFollowUpsNextTurn(ThreadTarget {
                thread: thread("missing"),
            }),
            web(),
        )
        .unwrap_err();

    assert_eq!(stop_chat.code, ErrorCode::Conflict);
    assert_eq!(stop_run.code, ErrorCode::Conflict);
    assert_eq!(deliver.code, ErrorCode::NotFound);
}

#[test]
fn stopping_a_running_thread_names_the_stopped_run() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());
    let started = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "start")), web())
        .unwrap();

    let stopped = host
        .handle_command(
            Command::StopChat(ThreadTarget {
                thread: thread("thread-1"),
            }),
            web(),
        )
        .unwrap();

    assert_eq!(stopped.run_id, started.run_id);
}

#[test]
fn a_provenance_that_contradicts_itself_is_refused() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());
    let agent_on_desktop = Provenance {
        actor: Actor::Agent,
        surface: Surface::Desktop,
        client_id: None,
        agent_run_id: None,
    };

    let error = host
        .handle_command(
            Command::SendChat(send("thread-1", "sub-1", "hello")),
            agent_on_desktop,
        )
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(host.service().runtime().list_agents().is_empty());
}

#[test]
fn commands_the_host_cannot_honour_yet_say_so() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());

    let approval = host
        .handle_command(
            Command::DecideToolApproval(DecideToolApproval {
                approval_id: ApprovalId::new("approval-1").unwrap(),
                core_epoch: InstanceEpoch(1),
                approval_revision: Revision(1),
                decision: ApprovalDecision::Approve,
            }),
            web(),
        )
        .unwrap_err();
    let delegated = host
        .handle_command(
            Command::SubmitGoal(SubmitGoal {
                project: common::project(),
                thread: thread("thread-1"),
                objective: "ship it".into(),
                references: Vec::new(),
                constraints: Vec::new(),
                delegation: Some(Delegation::Reference {
                    value: "packet-1".into(),
                }),
            }),
            web(),
        )
        .unwrap_err();

    assert_eq!(approval.code, ErrorCode::UnsupportedCapability);
    assert_eq!(delegated.code, ErrorCode::UnsupportedCapability);
    assert!(host.service().runtime().list_agents().is_empty());
}

#[test]
fn a_goal_joins_the_thread_like_the_desktop_one() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());

    let reply = host
        .handle_command(
            Command::SubmitGoal(SubmitGoal {
                project: common::project(),
                thread: thread("thread-1"),
                objective: "ship it".into(),
                references: Vec::new(),
                constraints: Vec::new(),
                delegation: Some(Delegation::Direct),
            }),
            web(),
        )
        .unwrap();

    assert!(reply.run_id.is_some());
    assert!(host.service().runtime().thread_goal("thread-1").is_some());
}

#[test]
fn the_web_tool_switch_reaches_the_runtime() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());

    let reply = host
        .handle_command(
            Command::SetAgentWebToolsEnabled(SetAgentWebToolsEnabled { enabled: true }),
            web(),
        )
        .unwrap();

    assert_eq!(reply.disposition, CommandDisposition::Accepted);
}

#[test]
fn an_unknown_question_is_not_found() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, _storage) = runtime_with_store(dir.path());
    let mut host = host_with_project(&rt, runtime, dir.path());

    let error = host
        .handle_command(
            Command::AnswerUserQuestion(AnswerUserQuestion {
                thread: thread("thread-1"),
                question_id: QuestionId::new("missing").unwrap(),
                answer: "yes".into(),
                question_revision: Revision(0),
            }),
            web(),
        )
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::NotFound);
}

#[test]
fn every_request_is_looked_up_by_its_receipt_within_its_session() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());
    let session = SessionId::new("session-1").unwrap();
    let other_session = SessionId::new("session-2").unwrap();
    let applied = RequestId::new("req-applied").unwrap();
    let rejected = RequestId::new("req-rejected").unwrap();
    let never_sent = RequestId::new("req-never").unwrap();
    let mut unknown_project = send("thread-1", "sub-2", "hello");
    unknown_project.project = workbench_protocol::ids::ProjectId::new("elsewhere").unwrap();

    let reply = host
        .handle_request(
            &session,
            &applied,
            Command::SendChat(send("thread-1", "sub-1", "hello")),
            web(),
        )
        .unwrap();
    let error = host
        .handle_request(
            &session,
            &rejected,
            Command::SendChat(unknown_project),
            web(),
        )
        .unwrap_err();

    let receipt = |session: &SessionId, id: &RequestId| match host
        .handle_query(
            session,
            &Query::GetCommandReceipt {
                request_id: id.clone(),
            },
        )
        .unwrap()
    {
        QueryReply::CommandReceipt(receipt) => receipt,
        other => panic!("not a receipt: {other:?}"),
    };
    assert_eq!(
        receipt(&session, &applied).state,
        CommandReceiptState::Applied { reply }
    );
    assert_eq!(error.correlation_id.as_ref(), Some(&rejected));
    assert_eq!(
        receipt(&session, &rejected).state,
        CommandReceiptState::Rejected { error }
    );
    assert_eq!(
        receipt(&session, &never_sent).state,
        CommandReceiptState::Unknown
    );
    assert_eq!(
        receipt(&other_session, &applied).state,
        CommandReceiptState::Unknown,
        "a request id means something only inside its own session"
    );
    assert_eq!(
        host.receipts().request_provenance(&session, &applied),
        Some(&web())
    );
}

#[test]
fn queries_other_than_receipts_are_not_served_yet() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let host = host_with_project(&rt, runtime(), dir.path());
    let session = SessionId::new("session-1").unwrap();

    let error = host
        .handle_query(&session, &Query::GetCoreStatus)
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::UnsupportedCapability);
}

#[test]
fn an_answered_question_accepts_the_same_answer_again_and_refuses_another() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, storage) = runtime_with_store(dir.path());
    // The requester is another incarnation, so the question has no live recipient.
    let requester = runtime_on_store(dir.path(), &storage);
    let question = rt.block_on(async {
        let run = requester
            .delegate_chat(
                "thread-1",
                runtime::Role::Worker,
                "Prepare the output".into(),
                runtime::RunConfig::default(),
            )
            .unwrap();
        let question = requester
            .request_user_question(run, "Which format?".into(), vec!["JSON".into()], true)
            .unwrap();
        requester.cancel(run).unwrap();
        requester.wait(run).await.unwrap();
        question
    });
    let mut host = host_with_project(&rt, runtime.clone(), dir.path());
    let answer = |text: &str| {
        Command::AnswerUserQuestion(AnswerUserQuestion {
            thread: thread("thread-1"),
            question_id: QuestionId::new(question.id.clone()).unwrap(),
            answer: text.into(),
            question_revision: Revision(0),
        })
    };

    let saved = host.handle_command(answer("JSON"), web()).unwrap();
    let repeated = host.handle_command(answer("JSON"), cli()).unwrap();
    let other = host.handle_command(answer("YAML"), cli()).unwrap_err();

    assert!(saved.run_id.is_some(), "the conversation resumed");
    assert_eq!(repeated.disposition, CommandDisposition::Accepted);
    assert_eq!(other.code, ErrorCode::AlreadyResolved);
    let run = host.service().bindings.chat_runs["thread-1"];
    runtime.cancel(run).unwrap();
    rt.block_on(async { runtime.wait(run).await.unwrap() });
}
