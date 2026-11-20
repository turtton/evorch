//! Public wire only: the client protocol on the product's Unix socket, and the
//! HTTP traffic the product sends to its provider. Neither side is reached
//! through anything the product does not offer to every client.
use mock_openai::{RecordedRequest, ScriptedResponse, StreamingMockOpenAi};
use std::path::Path;
use workbench_client::{ClientConfig, UnixTransport, WorkbenchClient};
use workbench_protocol::{
    commands::*,
    events::*,
    handshake::{BuildInfo, ServerHello},
    ids::*,
    namespaces::{ConversationCommand, ExecutionEvent},
    views::RunPhase,
};

/// The provider side of the wire: a mock OpenAI-compatible server on loopback
/// that the product reaches through its own configuration.
pub struct MockProvider(pub StreamingMockOpenAi);
impl MockProvider {
    pub fn scripted(responses: Vec<ScriptedResponse>) -> Self {
        Self(StreamingMockOpenAi::spawn_with_prompt_cache(responses))
    }
    pub fn base_url(&self) -> String {
        self.0.base_url()
    }
    /// Chat-completion requests the product has sent so far, in order.
    pub fn chat_requests(&self) -> Vec<RecordedRequest> {
        self.0
            .recorded_requests()
            .into_iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .collect()
    }
    pub fn remaining_scripts(&self) -> usize {
        self.0.remaining_scripts()
    }
}

/// What a client learns from the handshake alone.
#[derive(Debug, Clone)]
pub struct HelloSummary {
    pub epoch: u64,
    pub core: String,
    pub server_build: BuildInfo,
}
impl From<&ServerHello> for HelloSummary {
    fn from(hello: &ServerHello) -> Self {
        Self {
            epoch: hello.instance_epoch.0,
            core: hello.core.as_str().to_owned(),
            server_build: hello.server_build.clone(),
        }
    }
}

pub async fn connect(socket: &Path, name: &str) -> WorkbenchClient {
    WorkbenchClient::connect(
        UnixTransport::connect(socket)
            .await
            .unwrap_or_else(|e| panic!("connect {}: {e}", socket.display())),
        ClientConfig::new(
            ClientId::new(name).unwrap(),
            BuildInfo {
                name: name.into(),
                version: "1".into(),
                revision: None,
            },
        ),
    )
    .await
    .unwrap_or_else(|e| panic!("handshake as {name}: {e:?}"))
}

pub async fn subscribe_all(client: &WorkbenchClient) -> workbench_client::Subscription {
    client
        .subscribe(Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .await
        .expect("subscribe")
}

pub fn send_chat(thread: &str, submission: &str, text: &str) -> Command {
    Command::Conversation(ConversationCommand::SendChat(SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new(thread).unwrap(),
        submission_id: SubmissionId::new(submission).unwrap(),
        composer_role: ComposerRole::Orchestrator,
        text: text.into(),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    }))
}

/// Waits for the run's own event: it waits for the user again, or it ended.
/// The subscription ending is a failure, never a reason to poll.
pub async fn until_waiting(
    subscription: &mut workbench_client::Subscription,
    run: &str,
) -> Result<(), String> {
    while let Some(event) = subscription.next().await {
        if let UiEvent::Execution(ExecutionEvent::RunChanged { run: summary }) = event.event
            && summary.id.as_str() == run
        {
            match summary.phase {
                RunPhase::WaitingForUser => return Ok(()),
                RunPhase::Failed | RunPhase::Cancelled | RunPhase::Done => {
                    return Err(format!("run ended: {:?}", summary.phase));
                }
                _ => (),
            }
        }
    }
    Err(format!("subscription ended: {:?}", subscription.end()))
}
