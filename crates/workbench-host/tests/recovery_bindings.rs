//! Recovery publication fences are retryable and never become thread bindings.
mod common;
use common::*;
use workbench_host::{
    CommandHost, commands::CommandService, lane::ThreadBinding, projects::StaticProjects,
};
use workbench_protocol::{
    commands::{Command, ComposerRole, SendChat},
    errors::{ErrorCode, RetryAdvice},
    ids::{SubmissionId, ThreadId},
    namespaces::ConversationCommand,
};

fn host() -> CommandHost {
    let mut projects = StaticProjects::default();
    projects.insert(&project(), std::path::PathBuf::from("/tmp"));
    CommandHost::new(CommandService::new(
        runtime(),
        tokio::runtime::Handle::current(),
    ))
    .with_projects(projects)
}
fn bindings() -> Vec<(String, ThreadBinding)> {
    vec![(
        "t".into(),
        ThreadBinding {
            root: runtime::RunId::new(42),
            project_root: Some("/tmp".into()),
            goal_project: Some(project().to_string()),
            model_preference: None,
        },
    )]
}
fn send() -> Command {
    Command::Conversation(ConversationCommand::SendChat(SendChat {
        project: project(),
        thread: ThreadId::new("t").unwrap(),
        submission_id: SubmissionId::new("same-submission").unwrap(),
        composer_role: ComposerRole::Orchestrator,
        text: "follow up".into(),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    }))
}
#[tokio::test]
async fn recovery_fence_rejects_send_retryably_without_creating_or_binding_a_run() {
    let mut host = host();
    let guard = host.service().begin_root_recovery(bindings()).unwrap();
    for provenance in [
        web(),
        cli(),
        workbench_protocol::envelope::Provenance::human(
            workbench_protocol::envelope::Surface::Desktop,
            workbench_protocol::ids::ClientId::new("gui-1").unwrap(),
        ),
    ] {
        let error = host.handle_command(send(), provenance).unwrap_err();
        assert_eq!(error.code, ErrorCode::Busy);
        assert_eq!(error.retry, RetryAdvice::SameRequest);
        assert!(error.message.contains("restart_busy"));
    }
    assert!(host.service().runtime().list_agents().is_empty());
    assert!(host.service().bindings.root_of("t").is_none());
    assert!(host.service().lane_bindings().get("t").is_none());
    drop(guard);
    assert!(!host.service().root_recovery_pending("t"));
    // A refused submission has no success receipt and can be retried verbatim.
    let reply = host.handle_command(send(), web()).unwrap();
    assert!(reply.run_id.is_some());
    assert_eq!(host.service().runtime().list_agents().len(), 1);
    let run = host.service().runtime().list_agents()[0].run_id;
    host.service()
        .runtime()
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    host.service().runtime().wait(run).await.unwrap();
}
#[tokio::test]
async fn recovery_fence_is_removed_on_activation_error_unwind_and_task_cancellation() {
    let host = host();
    let failed = || -> Result<(), ()> {
        let _guard = host.service().begin_root_recovery(bindings()).unwrap();
        Err(())?;
        Ok(())
    };
    assert!(failed().is_err());
    assert!(!host.service().root_recovery_pending("t"));
    assert!(host.service().bindings.root_of("t").is_none());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = host.service().begin_root_recovery(bindings()).unwrap();
        panic!("activation panic");
    }));
    assert!(panic.is_err());
    assert!(!host.service().root_recovery_pending("t"));
    let guard = host.service().begin_root_recovery(bindings()).unwrap();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _guard = guard;
        entered.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    ready.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(!host.service().root_recovery_pending("t"));
    assert!(host.service().bindings.root_of("t").is_none());
}
#[tokio::test]
async fn recovery_publishes_only_on_success_and_never_overwrites_a_newer_binding() {
    let mut host = host();
    let guard = host.service().begin_root_recovery(bindings()).unwrap();
    host.service_mut().complete_root_recovery(guard);
    assert_eq!(
        host.service().bindings.root_of("t"),
        Some(runtime::RunId::new(42))
    );
    assert_eq!(
        host.service().lane_bindings().get("t").unwrap().root,
        runtime::RunId::new(42)
    );
    assert!(!host.service().root_recovery_pending("t"));
    host.service_mut()
        .bindings
        .chat_runs
        .insert("t".into(), runtime::RunId::new(99));
    assert!(host.service().begin_root_recovery(bindings()).is_err());
    assert_eq!(
        host.service().bindings.root_of("t"),
        Some(runtime::RunId::new(99))
    );
    assert!(!host.service().root_recovery_pending("t"));
}
