//! L12 uses the product Evaluator entrance; only the external CLI is a fixture.
use super::observer::bounded;
use mock_openai::ScriptedResponse;
use serde_json::json;
use std::{io, path::Path, time::Duration};
use workbench_client::WorkbenchClient;
use workbench_protocol::{
    commands::{ApprovalDecision, Command, ComposerRole, DecideToolApproval, SendChat},
    events::{Subscribe, SubscriptionScope, UiEvent},
    ids::*,
    namespaces::{
        ConversationCommand,
        approvals::{ApprovalsCommand, ApprovalsEvent},
        execution::ExecutionEvent,
    },
    views::{ApprovalState, RunPhase},
};
const LIMIT: Duration = Duration::from_secs(30);
fn error(e: impl std::fmt::Debug) -> io::Error {
    io::Error::other(format!("{e:?}"))
}
pub async fn repository(project: &Path) -> io::Result<()> {
    bounded(LIMIT, "sample repository", async {
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "fixture",
            ],
        ] {
            let status = tokio::process::Command::new("git")
                .args(args)
                .current_dir(project)
                .status()
                .await?;
            if !status.success() {
                return Err(error(status));
            }
        }
        // Executable/socket fixtures are external input, not replayable source.
        std::fs::write(project.join(".git/info/exclude"), "*\n")?;
        Ok(())
    })
    .await
}
pub fn responses() -> Vec<ScriptedResponse> {
    let call = |id: &str, name: &str, args: serde_json::Value| {
        ScriptedResponse::tool_call(id, "mock-model", 0, id, name, [args.to_string()])
    };
    vec![
        ScriptedResponse::text_stream("sample", "mock-model", ["recorded sample"]),
        call(
            "delegate",
            "delegate",
            json!({"target":{"role":"evaluator"},"prompt":"Pin sample thread sample then start the candidate evaluation"}),
        ),
        call(
            "save",
            "eval_save_sample",
            json!({"thread_id":"sample","problem":"matrix fixture","focus_roles":["Worker"]}),
        ),
        call(
            "start",
            "eval_start",
            json!({"thread_id":"sample","candidate":"[[routing.routes.worker]]\nprofile = \"candidate\"\n","repeat":1}),
        ),
        ScriptedResponse::text_stream("done", "mock-model", ["evaluation launched"]),
    ]
}
fn input(thread: &str, role: ComposerRole) -> SendChat {
    SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new(thread).unwrap(),
        submission_id: SubmissionId::new(thread).unwrap(),
        composer_role: role,
        text: "run fixture".into(),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    }
}
pub async fn enter(client: &WorkbenchClient) -> io::Result<()> {
    let mut events = client
        .subscribe(Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .await
        .map_err(error)?;
    client
        .command(Command::Conversation(ConversationCommand::CreateExecution(
            input("sample", ComposerRole::Worker),
        )))
        .await
        .map_err(error)?;
    bounded(LIMIT, "sample run completion", async {
        while let Some(event) = events.next().await {
            if let UiEvent::Execution(ExecutionEvent::RunChanged { run }) = event.event
                && run.thread.as_str() == "sample"
            {
                if matches!(run.phase, RunPhase::Done | RunPhase::WaitingForUser) {
                    return Ok(());
                }
                if matches!(run.phase, RunPhase::Failed | RunPhase::Cancelled) {
                    return Err(error(run));
                }
            }
        }
        Err(io::Error::other("sample events closed"))
    })
    .await?;
    client
        .command(Command::Conversation(ConversationCommand::CreateExecution(
            input("matrix", ComposerRole::Orchestrator),
        )))
        .await
        .map_err(error)?;
    bounded(LIMIT, "eval_start approval", async {
        while let Some(event) = events.next().await {
            if let UiEvent::Approvals(ApprovalsEvent::ApprovalChanged { approval }) = event.event
                && approval.tool_name == "eval_start"
                && approval.state == ApprovalState::Pending
            {
                client
                    .command(Command::Approvals(ApprovalsCommand::DecideToolApproval(
                        DecideToolApproval {
                            approval_id: approval.id,
                            core_epoch: approval.core_epoch,
                            approval_revision: approval.revision,
                            decision: ApprovalDecision::Approve,
                        },
                    )))
                    .await
                    .map_err(error)?;
                return Ok(());
            }
        }
        Err(io::Error::other("eval approval events closed"))
    })
    .await
}
pub fn verify_argv(project: &Path) -> io::Result<()> {
    let text = std::fs::read_to_string(project.join(".eval-argv"))?;
    let args: Vec<_> = text.lines().collect();
    for expected in [["eval", "run"], ["--thread", "sample"], ["--repeat", "1"]] {
        if !args.windows(2).any(|pair| pair == expected) {
            return Err(error((&expected, &args)));
        }
    }
    for flag in ["--candidate", "--problem", "--output", "--user-config"] {
        if !args.contains(&flag) {
            return Err(error((flag, &args)));
        }
    }
    super::evidence::emit(
        "eval",
        "argv",
        &json!({"cell":"eval","external_cli_fixture":true,"argv":args,"core_caller":"groups::launch -> spawn_owned(ChildKind::Eval)"}),
    )?;
    Ok(())
}
