use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent, MessageEvent};
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

fn serve(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<serde_json::Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("write timeout");
            let mut reader = BufReader::new(&stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).expect("headers") > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().expect("length");
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("body");
            requests.push(serde_json::from_slice(&body).expect("request JSON"));
            stream.write_all(response.as_bytes()).expect("response");
        }
        requests
    });
    (format!("http://{address}"), server)
}

fn runtime(base_url: String, bus: &Arc<EventBus>) -> AgentRuntime {
    let client = OpenAiClient::new(OpenAiConfig {
        base_url,
        event_bus: Some(bus.clone()),
        ..OpenAiConfig::default()
    })
    .expect("client");
    AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        Arc::new(HttpModel(client)),
    )
}

const HEADERS: &str =
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
const PARTIAL: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n";

#[tokio::test]
async fn retry_success_commits_full_history_without_duplicate_deltas() {
    // Given: 初回は部分表示で切断し、再試行は同じ接頭辞と末尾を返して完了する。
    // 次ターンを空の応答で終え、実際に送信された確定履歴を検査する。
    let (url, server) = serve(vec![
        format!("{HEADERS}{PARTIAL}"),
        format!(
            "{HEADERS}{PARTIAL}data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\" tail\"}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        ),
        format!(
            "{HEADERS}data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
        ),
    ]);
    let bus = Arc::new(EventBus::new(64));
    let mut receiver = bus.subscribe();
    let runtime = runtime(url, &bus);

    // When: 実際のランタイムをHTTPモデルで実行し、終端まで待つ。
    let run = runtime.delegate_background(Role::Worker, "go".into(), RunConfig::default());
    let phase = tokio::time::timeout(Duration::from_secs(3), runtime.wait(run))
        .await
        .expect("termination")
        .expect("wait");

    // Then: 正常完了し、表示の接頭辞は重複せず、履歴は成功した応答全体となる。
    assert_eq!(phase, AgentRunPhase::Done);
    let mut text = String::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = receiver.recv().await.expect("bus");
            if let EventKind::Message(MessageEvent::MessageDelta { delta, run_id }) = &event.kind {
                assert_eq!(run_id.as_deref(), Some(run.to_string().as_str()));
                text.push_str(delta);
            }
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
                &event.kind
                && run_id == &run.to_string()
            {
                assert_ne!(*to, AgentRunPhase::Error);
                if *to == AgentRunPhase::Done {
                    break;
                }
            }
        }
    })
    .await
    .expect("terminal event");
    assert_eq!(text, "partial tail");
    let requests = server.join().expect("server");
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["messages"], requests[1]["messages"]);
    let assistants: Vec<_> = requests[2]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] == "assistant")
        .collect();
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0]["content"], "partial tail");
}

#[tokio::test]
async fn retry_exhaustion_escalates_attempt_count_to_error_phase() {
    // Given: すべての試行でHTTP応答を返さず接続を切断する。
    let (url, server) = serve(vec![String::new(); 3]);
    let bus = Arc::new(EventBus::new(64));
    let mut receiver = bus.subscribe();
    let runtime = runtime(url, &bus);

    // When: 既定の3試行を使い切るまでランタイムを実行する。
    let run = runtime.delegate_background(Role::Worker, "go".into(), RunConfig::default());
    let phase = tokio::time::timeout(Duration::from_secs(3), runtime.wait(run))
        .await
        .expect("termination")
        .expect("wait");

    // Then: GUIへ届くError理由に試行回数が保持される。
    assert_eq!(phase, AgentRunPhase::Error);
    let reason = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Error,
                reason,
                ..
            }) = receiver.recv().await.expect("bus").kind
                && run_id == run.to_string()
            {
                break reason.expect("failure reason");
            }
        }
    })
    .await
    .expect("terminal event");
    assert!(
        reason.contains("stream failed after 3 attempts"),
        "{reason}"
    );
    assert_eq!(server.join().expect("server").len(), 3);
}
