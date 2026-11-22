//! The sealed, feature-free product core, started through its own entrance
//! (`--foreground`), and a protocol client of its Unix socket.
use super::observer::bounded;
use serde_json::Value;
use std::{ffi::OsString, io, path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};
use workbench_client::{ClientConfig, Subscription, UnixTransport, WorkbenchClient};
use workbench_protocol::{
    commands::{Command, ComposerRole, SendChat},
    events::{Subscribe, SubscriptionScope, UiEvent},
    handshake::{BuildInfo, Capability, LocalSurface},
    ids::*,
    namespaces::{ConversationCommand, ExecutionEvent},
    views::RunPhase,
};

pub const LIMIT: Duration = Duration::from_secs(30);

pub fn error(value: impl std::fmt::Debug) -> io::Error {
    io::Error::other(format!("{value:?}"))
}

pub struct Core {
    pub client: WorkbenchClient,
    pub events: Subscription,
}

/// Spawns the sealed core. The caller pins the owner with `ProcessTree::new`
/// right away, before anything else can happen.
pub fn spawn(
    exe: &Path,
    root: &Path,
    project: &Path,
    env: &[(&str, OsString)],
) -> io::Result<tokio::process::Child> {
    let mut command = tokio::process::Command::new(exe);
    command.env_clear();
    for name in ["PATH", "LD_LIBRARY_PATH"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    for (name, value) in env {
        command.env(name, value);
    }
    command
        .args(["--config-dir", root.to_str().unwrap(), "--foreground"])
        .current_dir(project)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .stdin(Stdio::null())
        .spawn()
}

/// READY line, then a client that stays connected (detach is not the injection).
pub async fn connect(child: &mut tokio::process::Child) -> io::Result<Core> {
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let ready = bounded(LIMIT, "core READY", async {
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| io::Error::other("core exited before READY"))?;
        serde_json::from_str::<Value>(&line).map_err(io::Error::other)
    })
    .await?;
    if ready["status"] != "ready" {
        return Err(io::Error::other(format!("not ready: {ready}")));
    }
    let mut config = ClientConfig::new(
        ClientId::new("b3-08").unwrap(),
        BuildInfo {
            name: "b3-08".into(),
            version: "1".into(),
            revision: None,
        },
    );
    config.required_capabilities.push(Capability::Conversation);
    config.local_surface = Some(LocalSurface::Cli);
    let client = bounded(LIMIT, "client connect", async {
        let transport = UnixTransport::connect(ready["socket"].as_str().unwrap())
            .await
            .map_err(error)?;
        WorkbenchClient::connect(transport, config)
            .await
            .map_err(error)
    })
    .await?;
    let events = client
        .subscribe(Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .await
        .map_err(error)?;
    Ok(Core { client, events })
}

fn chat(thread: &str, submission: &str, text: &str) -> SendChat {
    SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new(thread).unwrap(),
        submission_id: SubmissionId::new(submission).unwrap(),
        composer_role: ComposerRole::Worker,
        text: text.into(),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    }
}

/// First turn: creates the thread's execution; returns its run id.
pub async fn create_execution(core: &Core, thread: &str, text: &str) -> io::Result<String> {
    let reply = core
        .client
        .command(Command::Conversation(ConversationCommand::CreateExecution(
            chat(thread, "first", text),
        )))
        .await
        .map_err(error)?;
    Ok(reply
        .run_id
        .ok_or_else(|| io::Error::other("no run id"))?
        .as_str()
        .to_owned())
}

/// A later turn of the same thread.
pub async fn send_chat(core: &Core, thread: &str, submission: &str, text: &str) -> io::Result<()> {
    core.client
        .command(Command::Conversation(ConversationCommand::SendChat(chat(
            thread, submission, text,
        ))))
        .await
        .map_err(error)?;
    Ok(())
}

/// The run's own event: it waits for the user again. The run ending
/// otherwise, or the stream closing, is a failure, never a reason to poll.
pub async fn until_waiting(core: &mut Core, run: &str) -> io::Result<()> {
    bounded(LIMIT, "run waiting for the user", async {
        while let Some(event) = core.events.next().await {
            if let UiEvent::Execution(ExecutionEvent::RunChanged { run: summary }) = event.event
                && summary.id.as_str() == run
            {
                match summary.phase {
                    RunPhase::WaitingForUser => return Ok(()),
                    RunPhase::Failed | RunPhase::Cancelled | RunPhase::Done => {
                        return Err(io::Error::other(format!("run ended: {:?}", summary.phase)));
                    }
                    _ => (),
                }
            }
        }
        Err(io::Error::other("event stream closed"))
    })
    .await
}
