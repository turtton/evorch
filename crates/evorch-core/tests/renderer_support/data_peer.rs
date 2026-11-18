//! A client of the core over its Unix socket, shared by the data-plane tests:
//! it drives renderer operations through control commands, waits on events of
//! its subscription (never a duration), and opens data connections.
use workbench_client::renderer::{DataChannel, DataClientError};
use workbench_client::{ClientConfig, ClientError, Subscription, UnixTransport, WorkbenchClient};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::ErrorCode,
    events::{Subscribe, SubscriptionScope},
    handshake::BuildInfo,
    ids::ClientId,
    namespaces::{
        artifacts::{ArtifactRef, ArtifactsCommand, ArtifactsResult},
        browser::*,
    },
    queries::{Query, QueryReply},
};

pub struct Peer {
    pub client: WorkbenchClient,
    pub events: Subscription,
    pub socket: std::path::PathBuf,
}
impl Peer {
    pub async fn connect(socket: &std::path::Path, name: &str) -> Self {
        let client = WorkbenchClient::connect(
            UnixTransport::connect(socket).await.unwrap(),
            ClientConfig::new(
                ClientId::new(name).unwrap(),
                BuildInfo {
                    name: "test".into(),
                    version: "1".into(),
                    revision: None,
                },
            ),
        )
        .await
        .unwrap_or_else(|error| panic!("{name} could not connect: {error:?}"));
        let events = client
            .subscribe(Subscribe {
                scope: SubscriptionScope::All,
                after: None,
            })
            .await
            .unwrap();
        Self {
            client,
            events,
            socket: socket.to_path_buf(),
        }
    }
    pub async fn accepted(&self, command: Command) -> Result<RendererId, ErrorCode> {
        let reply = self.client.command(command).await.map_err(code)?;
        match reply.result.as_deref() {
            Some(CommandResult::Browser(BrowserResult::Accepted { operation_id }))
            | Some(CommandResult::Artifacts(ArtifactsResult::Accepted { operation_id })) => {
                Ok(operation_id.clone())
            }
            other => panic!("unexpected receipt: {other:?}"),
        }
    }
    pub async fn browser(&self, command: BrowserCommand) -> Result<RendererId, ErrorCode> {
        self.accepted(Command::Browser(command)).await
    }
    pub async fn operation(&self, id: &RendererId) -> RendererOperationState {
        match self
            .client
            .query(Query::Browser(BrowserQuery::GetRendererOperation {
                operation_id: id.clone(),
            }))
            .await
            .unwrap()
        {
            QueryReply::Browser(BrowserReply::Operation(operation)) => operation.state,
            other => panic!("unexpected reply: {other:?}"),
        }
    }
    /// The operation's output; the subscription says when it changed.
    pub async fn output(&mut self, id: &RendererId) -> RendererOutput {
        loop {
            match self.operation(id).await {
                RendererOperationState::Running => {
                    self.events.next().await.expect("the stream stays open");
                }
                RendererOperationState::Completed { output } => return output,
                other => panic!("the operation did not complete: {other:?}"),
            }
        }
    }
    pub async fn session(&mut self) -> RendererSessionRef {
        let id = self
            .browser(BrowserCommand::CreateRendererSession {})
            .await
            .unwrap();
        match self.output(&id).await {
            RendererOutput::Session { session, .. } => session,
            other => panic!("unexpected output: {other:?}"),
        }
    }
    pub async fn open(
        &mut self,
        artifact: &ArtifactRef,
        session: &RendererSessionRef,
    ) -> RendererTab {
        let id = self
            .accepted(Command::Artifacts(ArtifactsCommand::OpenPreview {
                artifact: artifact.clone(),
                session: session.clone(),
                viewport: RendererViewport {
                    width: 1024,
                    height: 768,
                },
            }))
            .await
            .unwrap();
        match self.output(&id).await {
            RendererOutput::Tab(tab) => *tab,
            other => panic!("unexpected output: {other:?}"),
        }
    }
    pub async fn channel(&self) -> DataChannel {
        DataChannel::connect(&self.socket).await.unwrap()
    }
}
pub fn code(error: ClientError) -> ErrorCode {
    match error {
        ClientError::Server(error) => error.code,
        other => panic!("not a refusal of the core: {other:?}"),
    }
}
/// The code of a refused binding.
pub fn refusal<T>(result: Result<T, DataClientError>) -> ErrorCode {
    match result {
        Err(DataClientError::Bind(error)) => code(error),
        Err(other) => panic!("not a refused binding: {other:?}"),
        Ok(_) => panic!("the binding was accepted"),
    }
}
