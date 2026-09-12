use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use event_bus::{
    AgentRunPhase, EventBus, EventKind, MessageEvent, ProviderEvent, ProviderFailureKind,
};
use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::{ChatRequest, ProviderAuth, ProviderClient};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RuntimeError};

struct HttpModel(OpenAiClient);

#[async_trait::async_trait]
impl AgentModel for HttpModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        Err(RuntimeError::Model {
            reason: "stream required".into(),
        })
    }
    async fn complete_streaming(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
        bus: &EventBus,
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.0
            .send_streaming(
                &ProviderAuth::new("test"),
                &ChatRequest {
                    model: "test".into(),
                    messages: messages.to_vec(),
                    tools: tools.to_vec(),
                    temperature: None,
                    max_tokens: None,
                    observation: Some(providers::ObservationContext {
                        run_id: invocation.run_id.clone(),
                    }),
                },
                bus,
            )
            .await
            .map_err(|error| RuntimeError::Model {
                reason: error.to_string(),
            })
    }
    fn selected_model(&self, _: Role) -> String {
        "http".into()
    }
}

async fn failed_stream(tail: &'static str, cancel: bool) {
    // Given: an HTTP server gated after one valid delta with no completed response.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (release, gate) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut reader = BufReader::new(&stream);
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("headers");
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().expect("length");
            }
        }
        reader.read_exact(&mut vec![0; length]).expect("body");
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n").expect("partial");
        gate.recv_timeout(Duration::from_secs(5)).expect("release");
        if !cancel {
            stream.write_all(tail.as_bytes()).expect("tail");
        }
    });
    let bus = Arc::new(EventBus::new(64));
    let mut receiver = bus.subscribe();
    let client = OpenAiClient::new(OpenAiConfig {
        base_url: format!("http://{address}"),
        event_bus: Some(bus.clone()),
        ..OpenAiConfig::default()
    })
    .expect("client");
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(HttpModel(client)),
    );
    // When: the loop receives a partial delta, followed by EOF, invalid SSE, or cancellation.
    let run = runtime.delegate_background(Role::Worker, "go".into(), RunConfig::default());
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let EventKind::Message(MessageEvent::MessageDelta { delta, run_id }) =
                receiver.recv().await.expect("bus").kind
            {
                assert_eq!(delta, "partial");
                assert_eq!(run_id, Some(run.to_string()));
                break;
            }
        }
    })
    .await
    .expect("partial visible");
    let before = runtime
        .inspect_agent(run)
        .expect("inspection")
        .message_count;
    if cancel {
        runtime.cancel(run).expect("cancel");
    } else {
        release.send(()).expect("release");
    }
    let phase = tokio::time::timeout(Duration::from_secs(3), runtime.wait(run))
        .await
        .expect("termination")
        .expect("wait");
    if cancel {
        release.send(()).expect("release");
    }
    server.join().expect("server");
    // Then: failed partial display never becomes committed assistant history.
    assert_eq!(phase, AgentRunPhase::Error);
    assert_eq!(
        runtime
            .inspect_agent(run)
            .expect("inspection")
            .message_count,
        before
    );
    if cancel {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let EventKind::Provider(ProviderEvent::RequestFailed { failure, .. }) =
                    receiver.recv().await.expect("bus").kind
                {
                    assert_eq!(failure, ProviderFailureKind::Other);
                    break;
                }
            }
        })
        .await
        .expect("cancel failure observation");
    }
}

#[tokio::test]
async fn partial_eof_does_not_commit_history_over_http() {
    failed_stream("", false).await;
}

#[tokio::test]
async fn stream_error_does_not_commit_history_over_http() {
    failed_stream("data: invalid-json\n\n", false).await;
}

#[tokio::test]
async fn cancellation_does_not_commit_history_over_http() {
    failed_stream("", true).await;
}
