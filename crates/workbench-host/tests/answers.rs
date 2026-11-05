//! Who saved a question's answer (ADR 0045 section 6): the first save is kept
//! with its provenance, a repeat never takes it over, and it stays out of the
//! model input.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{cli, host_with_project, offline_question, runtime_with_store, web};
use event_bus::EventBus;
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RuntimeError};
use tokio::sync::Notify;
use tools::ToolExecutor;
use workbench_host::commands::{HostCommand, HostEvent, RejectKind};
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::{AnswerUserQuestion, Command, CommandDisposition};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::ids::{QuestionId, ThreadId};
use workbench_protocol::wire::Revision;

fn answer(question: &event_bus::UserQuestion, text: &str) -> Command {
    Command::AnswerUserQuestion(AnswerUserQuestion {
        thread: ThreadId::new("thread-1").unwrap(),
        question_id: QuestionId::new(question.id.clone()).unwrap(),
        answer: text.into(),
        question_revision: Revision(0),
    })
}

fn cancel(host: &workbench_host::CommandHost, rt: &tokio::runtime::Runtime) {
    let run = host.service().bindings.chat_runs["thread-1"];
    host.service().runtime().cancel(run).unwrap();
    rt.block_on(async { host.service().runtime().wait(run).await.unwrap() });
}

#[test]
fn the_first_answer_keeps_its_provenance_and_a_repeat_does_not_take_it() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, storage) = runtime_with_store(dir.path());
    let question = offline_question(&rt, dir.path(), &storage);
    let mut host = host_with_project(&rt, runtime, dir.path());
    assert!(host.answer_provenance(&question.id).is_none());

    host.handle_command(answer(&question, "JSON"), web())
        .unwrap();
    let repeat = host
        .handle_command(answer(&question, "JSON"), cli())
        .unwrap();
    let other = host
        .handle_command(answer(&question, "YAML"), cli())
        .unwrap_err();

    assert_eq!(repeat.disposition, CommandDisposition::Accepted);
    assert_eq!(other.code, ErrorCode::AlreadyResolved);
    assert_eq!(host.answer_provenance(&question.id), Some(web()));
    cancel(&host, &rt);
}

#[test]
fn a_desktop_answer_is_remembered_and_a_later_web_repeat_does_not_replace_it() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, storage) = runtime_with_store(dir.path());
    let question = offline_question(&rt, dir.path(), &storage);
    let mut host = host_with_project(&rt, runtime, dir.path());
    let desktop = local_desktop();

    let events = host.submit_local(
        HostCommand::AnswerUserQuestion {
            thread_id: "thread-1".into(),
            question_id: question.id.clone(),
            answer: "JSON".into(),
        },
        &desktop,
    );
    host.handle_command(answer(&question, "JSON"), web())
        .unwrap();

    assert!(events.iter().any(|event| matches!(
        event,
        HostEvent::UserAnswerSaved {
            newly_saved: true,
            ..
        }
    )));
    assert_eq!(host.answer_provenance(&question.id), Some(desktop.clone()));
    cancel(&host, &rt);
}

#[test]
fn an_answer_stored_before_this_host_saw_it_is_credited_to_nobody() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, storage) = runtime_with_store(dir.path());
    let question = offline_question(&rt, dir.path(), &storage);
    runtime.answer_user_question(&question.id, "JSON").unwrap();
    let mut host = host_with_project(&rt, runtime, dir.path());

    let repeat = host
        .handle_command(answer(&question, "JSON"), web())
        .unwrap();

    assert_eq!(repeat.disposition, CommandDisposition::Accepted);
    assert!(host.answer_provenance(&question.id).is_none());
}

#[derive(Default)]
struct RecordingModel {
    first_request: Mutex<Option<String>>,
    received: Notify,
}

#[async_trait]
impl AgentModel for RecordingModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.first_request
            .lock()
            .unwrap()
            .get_or_insert_with(|| format!("{messages:?}\n{tools:?}"));
        self.received.notify_one();
        std::future::pending().await
    }

    fn selected_model(&self, role: Role, _: Option<&str>) -> String {
        format!("test-{}", role.name().to_lowercase())
    }
}

/// The first model request of the conversation an answer resumes, with the
/// question id (different on every run) masked.
fn resumed_request(provenance: &Provenance, through_desktop_door: bool) -> String {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("host.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let model = Arc::new(RecordingModel::default());
    let bus = Arc::new(EventBus::new(64));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    let question = offline_question(&rt, dir.path(), &storage);
    let mut host = host_with_project(&rt, runtime, dir.path());
    if through_desktop_door {
        let events = host.submit_local(
            HostCommand::AnswerUserQuestion {
                thread_id: "thread-1".into(),
                question_id: question.id.clone(),
                answer: "JSON".into(),
            },
            provenance,
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, HostEvent::CommandRejected { kind, .. } if *kind != RejectKind::Busy)),
            "{events:?}"
        );
    } else {
        host.handle_command(answer(&question, "JSON"), provenance.clone())
            .unwrap();
    }
    rt.block_on(model.received.notified());
    let request = model.first_request.lock().unwrap().clone().unwrap();
    // The question id is different on every run and is the only thing normalised.
    request.replace(&question.id, "<question>")
}

/// Equal, or the first place they differ.
fn assert_same(left: &str, right: &str) {
    let at = left
        .char_indices()
        .zip(right.chars())
        .find(|((_, a), b)| a != b)
        .map_or(left.len().min(right.len()), |((index, _), _)| index);
    let window = |text: &str| {
        text.chars()
            .skip(at.saturating_sub(80))
            .take(200)
            .collect::<String>()
    };
    assert!(
        left == right,
        "first difference at {at}:\n{}\n{}",
        window(left),
        window(right)
    );
}

#[test]
fn the_conversation_an_answer_resumes_sees_the_same_input_whoever_answered() {
    let from_web = resumed_request(&web(), false);
    let from_cli = resumed_request(&cli(), false);
    let from_desktop = resumed_request(&local_desktop(), true);

    assert!(from_web.contains("Answer: JSON"));
    assert!(
        from_web.contains("Prepare the output"),
        "the conversation resumed with the context its requester saved"
    );
    assert_same(&from_web, &from_cli);
    assert_same(&from_web, &from_desktop);
    assert!(!from_web.contains("web-1"));
}
