//! Existing protocol entrances, not direct calls to ownership APIs.
use super::observer::bounded;
use std::{io, time::Duration};
use workbench_client::{Subscription, WorkbenchClient};
use workbench_protocol::{
    commands::Command,
    events::{Subscribe, SubscriptionScope, UiEvent},
    ids::*,
    namespaces::{mcp::*, pull_requests::*},
    queries::{Query, QueryReply},
};
const LIMIT: Duration = Duration::from_secs(30);
fn error(value: impl std::fmt::Debug) -> io::Error {
    io::Error::other(format!("{value:?}"))
}

async fn operation(
    client: &WorkbenchClient,
    events: &mut Subscription,
    command: McpCommand,
) -> io::Result<McpOperationState> {
    let id = command.operation_id().clone();
    client.command(Command::Mcp(command)).await.map_err(error)?;
    bounded(LIMIT, "MCP operation completion", async {
        while let Some(event) = events.next().await {
            if let UiEvent::Mcp(McpEvent::OperationCompleted { operation }) = event.event
                && operation.operation_id == id
            {
                return Ok(operation.state);
            }
        }
        Err(io::Error::other("MCP event stream closed"))
    })
    .await
}
pub async fn mcp(client: &WorkbenchClient) -> io::Result<()> {
    let project = ProjectId::new("p").unwrap();
    let scope = McpScope {
        project: project.clone(),
        server_id: "fixture".into(),
    };
    let mut events = client
        .subscribe(Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .await
        .map_err(error)?;
    let state = operation(
        client,
        &mut events,
        McpCommand::Refresh(Refresh {
            operation_id: McpOperationId::new("refresh").unwrap(),
            project: project.clone(),
        }),
    )
    .await?;
    let McpOperationState::Succeeded {
        outcome: McpOutcome::Settings { revision },
    } = state
    else {
        return Err(error(state));
    };
    let state = operation(
        client,
        &mut events,
        McpCommand::PreviewApproval(PreviewApproval {
            operation_id: McpOperationId::new("preview").unwrap(),
            scope: scope.clone(),
            expected_revision: revision.clone(),
        }),
    )
    .await?;
    let McpOperationState::Succeeded {
        outcome: McpOutcome::ApprovalPreview { preview },
    } = state
    else {
        return Err(error(state));
    };
    let state = operation(
        client,
        &mut events,
        McpCommand::ConfirmApproval(ConfirmApproval {
            operation_id: McpOperationId::new("confirm").unwrap(),
            preview_id: preview.preview_id,
            scope: preview.scope,
            expected_revision: preview.source_revision,
            expected_fingerprint: preview.fingerprint,
        }),
    )
    .await?;
    if !matches!(state, McpOperationState::Succeeded { .. }) {
        return Err(error(state));
    }
    client
        .command(Command::Mcp(McpCommand::TestConnection(TestConnection {
            operation_id: McpOperationId::new("test").unwrap(),
            scope,
            expected_revision: revision,
        })))
        .await
        .map_err(error)?;
    Ok(())
}
pub async fn pull_request(client: &WorkbenchClient) -> io::Result<()> {
    let thread = ThreadId::new("matrix").unwrap();
    let reply = client
        .query(Query::PullRequests(
            PullRequestsQuery::GetThreadPullRequest {
                thread: thread.clone(),
            },
        ))
        .await
        .map_err(error)?;
    let QueryReply::PullRequests(PullRequestsReply::Thread(view)) = reply else {
        return Err(error(reply));
    };
    client
        .command(Command::PullRequests(PullRequestsCommand::LinkPullRequest(
            LinkPullRequest {
                operation_id: OperationId::new("link").unwrap(),
                thread,
                locator: "https://github.com/fixture/repo/pull/1".into(),
                expected_revision: view.lifecycle_revision,
            },
        )))
        .await
        .map_err(error)?;
    Ok(())
}

/// Returns the live client as well: client detach is not the injection.
pub async fn enter_caller(
    cell: &str,
    stdout: tokio::process::ChildStdout,
    listener: &tokio::net::UnixListener,
    project: &std::path::Path,
) -> io::Result<(i32, WorkbenchClient)> {
    use super::product::{check, peer_ready};
    use serde_json::Value;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use workbench_client::{ClientConfig, UnixTransport};
    use workbench_protocol::{
        commands::{ComposerRole, SendChat},
        handshake::BuildInfo,
        namespaces::ConversationCommand,
    };
    let mut lines = BufReader::new(stdout).lines();
    let ready = bounded(LIMIT, "core READY", async {
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| io::Error::other("core exited before READY"))?;
        serde_json::from_str::<Value>(&line).map_err(io::Error::other)
    })
    .await?;
    check(ready["status"] == "ready", format!("not ready: {ready}"))?;
    let mut config = ClientConfig::new(
        ClientId::new("b3").unwrap(),
        BuildInfo {
            name: "b3".into(),
            version: "1".into(),
            revision: None,
        },
    );
    config
        .required_capabilities
        .push(workbench_protocol::handshake::Capability::Conversation);
    config.local_surface = Some(workbench_protocol::handshake::LocalSurface::Cli);
    let client = bounded(LIMIT, "client connect", async {
        let transport = UnixTransport::connect(ready["socket"].as_str().unwrap())
            .await
            .map_err(|e| io::Error::other(format!("{e:?}")))?;
        WorkbenchClient::connect(transport, config)
            .await
            .map_err(|e| io::Error::other(format!("{e:?}")))
    })
    .await?;
    let input = SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("matrix").unwrap(),
        submission_id: SubmissionId::new("start").unwrap(),
        composer_role: ComposerRole::Worker,
        text: "run fixture".into(),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    };
    let peer = if cell == "eval_cli" {
        super::evaluation::enter(&client).await?;
        peer_ready(listener).await?
    } else if cell == "eval" {
        super::evaluation::enter(&client).await?;
        let peer = peer_ready(listener).await?;
        super::evaluation::verify_argv(project)?;
        peer
    } else if cell == "mcp" {
        mcp(&client).await?;
        peer_ready(listener).await?
    } else if cell == "pull_request" {
        client
            .command(workbench_protocol::commands::Command::Conversation(
                ConversationCommand::CreateExecution(input),
            ))
            .await
            .map_err(|e| io::Error::other(format!("{e:?}")))?;
        pull_request(&client).await?;
        peer_ready(listener).await?
    } else {
        tokio::select! {
            reply = client.command(workbench_protocol::commands::Command::Conversation(
                ConversationCommand::CreateExecution(input))) => {
                reply.map_err(|e| io::Error::other(format!("{e:?}")))?;
                peer_ready(listener).await?
            },
            peer = peer_ready(listener) => peer?,
        }
    };
    Ok((peer, client))
}
