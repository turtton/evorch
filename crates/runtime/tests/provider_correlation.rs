//! agent-loop → モデル → provider wire を貫く run_id 相関 threading の契約。
//!
//! ローカル mock OpenAI 互換サーバと実 `OpenAiCompatibleClient` を使い、
//! wire 経路を通した状態で attempt 観測イベントと ToolEvent の run_id stamp
//! を検証する。

mod support;

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agents::Role;
use async_trait::async_trait;
use event_bus::{AgentRunPhase, Event, EventBus, EventKind, ProviderEvent, ToolEvent};
use providers::{
    ChatRequest, ChatResponse, Message, ObservationContext, ProviderAuth, ProviderClient, ToolSpec,
    provider::openai_compatible::OpenAiCompatibleClient,
};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, RunConfig, RuntimeError};
use sandbox::DirectSandbox;
use serde_json::json;
use tools::ToolExecutor;

fn runtime_with(bus: &Arc<EventBus>, model: Arc<dyn AgentModel>) -> AgentRuntime {
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    AgentRuntime::new(Arc::clone(bus), executor, model)
}

async fn drain_events(receiver: &mut event_bus::EventReceiver) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(event) = tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await {
        match event {
            Ok(event) => events.push(event),
            Err(_) => break,
        }
    }
    events
}

/// OpenAI 互換の 1 リクエスト 1 応答モックサーバを起動し、base URL を返す。
fn spawn_mock_openai(responses: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("モックサーバを bind できる");
    let addr = listener.local_addr().expect("モックアドレスを取得できる");
    let script = Mutex::new(VecDeque::from(responses));
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let Some(response) = script
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
            else {
                continue;
            };
            read_request(&mut stream);
            let http = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            );
            stream
                .write_all(http.as_bytes())
                .expect("モック応答を書き込める");
        }
    });
    format!("http://{addr}")
}

/// リクエストヘッダと Content-Length 分の body を読み切る。
///
/// body を読み切らずに close すると RST によりクライアントが応答を
/// 読めなくなるため、応答前に送信内容を読み捨てる。
fn read_request(stream: &mut TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(header_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if buf.len() >= header_end + 4 + content_length {
                return;
            }
        }
        let read = stream.read(&mut chunk).expect("モックリクエストを読める");
        if read == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
}

/// production 実装と同じ「invocation → ChatRequest.observation」変換を行い、
/// 実 wire で attempt 観測への run_id stamp を検証できるようにするテスト用モデル。
struct ProviderCorrelatedModel {
    client: OpenAiCompatibleClient,
    auth: ProviderAuth,
}

impl ProviderCorrelatedModel {
    fn new(base_url: String, bus: Arc<EventBus>) -> Self {
        Self {
            client: OpenAiCompatibleClient::new(
                base_url,
                "mock-provider",
                Duration::from_secs(5),
                Some(bus),
            )
            .expect("mock provider クライアントを構築できる"),
            auth: ProviderAuth::new("test-key"),
        }
    }
}

#[async_trait]
impl AgentModel for ProviderCorrelatedModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let request = ChatRequest {
            model: "mock-model".to_string(),
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            temperature: None,
            max_tokens: None,
            observation: Some(ObservationContext {
                run_id: invocation.run_id.clone(),
            }),
        };
        self.client
            .send(&self.auth, &request)
            .await
            .map_err(|error| RuntimeError::Model {
                reason: error.to_string(),
            })
    }

    fn selected_model(&self, _role: Role) -> String {
        "mock-observation".to_string()
    }
}

// Given: tool_call → stop の 2 応答を返す mock OpenAI 互換サーバと observation を wire に載せるモデル
// When: agent-loop を Worker run として完了まで実行する
// Then: bus 観測される全 ProviderEvent attempt (RequestStarted/RequestCompleted) と
//       ToolEvent (ToolStarted/ToolCompleted) の run_id はその run の ID
#[tokio::test]
async fn agent_loop_stamps_run_id_on_provider_attempts_and_tool_events() {
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("out.txt");
    let tool_response = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "edit",
                        "arguments": json!({ "path": path, "new_string": "written" }).to_string()
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
    })
    .to_string();
    let stop_response = json!({
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
    })
    .to_string();

    let bus = Arc::new(EventBus::new(64));
    let base_url = spawn_mock_openai(vec![tool_response, stop_response]);
    let runtime = runtime_with(
        &bus,
        Arc::new(ProviderCorrelatedModel::new(base_url, Arc::clone(&bus))),
    );
    let mut events = bus.subscribe();

    let run_id =
        runtime.delegate_background(Role::Worker, "write".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let events = drain_events(&mut events).await;
    let expected_run_id = run_id.to_string();

    let attempt_run_ids = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(
                ProviderEvent::RequestStarted { run_id, .. }
                | ProviderEvent::RequestCompleted { run_id, .. },
            ) => Some(run_id),
            _ => None,
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        attempt_run_ids.len(),
        4,
        "tool_call と stop の 2 attempt 分 (started + completed) が観測される: {attempt_run_ids:?}"
    );
    for run_id in &attempt_run_ids {
        assert_eq!(
            run_id,
            &Some(expected_run_id.clone()),
            "attempt 観測イベントに run 相関が載る"
        );
    }

    let tool_run_ids = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Tool(
                ToolEvent::ToolStarted { run_id, .. } | ToolEvent::ToolCompleted { run_id, .. },
            ) => Some(run_id),
            _ => None,
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        tool_run_ids.len(),
        2,
        "edit ツールの started + completed が観測される: {tool_run_ids:?}"
    );
    for run_id in &tool_run_ids {
        assert_eq!(
            run_id,
            &Some(expected_run_id.clone()),
            "ToolEvent に run 相関が載る"
        );
    }
}
