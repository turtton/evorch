pub mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use event_bus::{Event, EventBus, EventKind, MessageEvent};
use futures_util::FutureExt;
use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::{
    ChatRequest, ChatResponse, ContentBlock, ProviderAuth, ProviderClient, ProviderError,
};
use support::{json_response, next_event};
use tcp_mock::{TcpBehavior, TcpMockServer};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

const PARTIAL: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n";
const FULL: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" tail\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

async fn send(base_url: &str, bus: &EventBus) -> Result<ChatResponse, ProviderError> {
    let client = OpenAiClient::new(OpenAiConfig {
        base_url: base_url.to_owned(),
        ..OpenAiConfig::default()
    })
    .expect("OpenAI client");
    let request = ChatRequest {
        model: "gpt-contract".into(),
        messages: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        observation: None,
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        client.send_streaming(&ProviderAuth::new("sk-contract"), &request, bus),
    )
    .await
    .expect("既定 backoff を含めても10秒以内に終了する")
}

#[tokio::test]
async fn mid_stream_disconnect_retries_and_preserves_display() {
    // Given: 部分表示後に切断し、次の接続で同じ先頭から再送する。
    let server = TcpMockServer::start(vec![
        TcpBehavior::PartialThenDrop(PARTIAL),
        TcpBehavior::FullSse(FULL),
    ]);
    let bus = EventBus::new(32);
    let mut receiver = bus.subscribe();

    // When: 実クライアントで既定ポリシーのストリーミング完了を待つ。
    let response = send(&server.base_url, &bus).await.expect("再試行成功");

    // Then: 応答と表示は一致し、再送した先頭も余分な末尾も表示しない。
    assert_eq!(
        response.message.content,
        vec![ContentBlock::Text {
            text: "partial tail".into()
        }]
    );
    // 完了後の標識まで全イベントを読むことで、期待文字数以降の重複も検出する。
    let terminal = Event::new(MessageEvent::MessageDelta {
        delta: String::new(),
        run_id: Some("test-terminal".into()),
    });
    bus.emit(terminal.clone());
    let mut display = String::new();
    loop {
        let event = next_event(&mut receiver).await;
        if event == terminal {
            break;
        }
        match event.kind {
            EventKind::Message(MessageEvent::MessageDelta { delta, .. }) => {
                display.push_str(&delta)
            }
            other => panic!("予期しない表示イベント: {other:?}"),
        }
    }
    assert_eq!(display, "partial tail");
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn mid_frame_disconnect_retries_and_deduplicates_complete_prefix() {
    // Given: 完全な接頭辞の次の JSON フレーム途中で切断し、次の接続で完了する。
    let server = TcpMockServer::start(vec![
        TcpBehavior::PartialThenDrop(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
            "data: {\"choices\":",
        )),
        TcpBehavior::FullSse(FULL),
    ]);
    let bus = EventBus::new(32);
    let mut receiver = bus.subscribe();
    // When: 実 HTTP と send_streaming を通して完了を待つ。
    let result = send(&server.base_url, &bus).await;
    // Then: 2接続で成功し、完全な接頭辞は一度だけ表示する。
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
    let mut display = String::new();
    while let Some(event) = receiver.recv().now_or_never() {
        match event.expect("表示イベント").kind {
            EventKind::Message(MessageEvent::MessageDelta { delta, .. }) => {
                display.push_str(&delta);
            }
            other => panic!("予期しない表示イベント: {other:?}"),
        }
    }
    assert_eq!(display, "partial tail");
}

#[tokio::test]
async fn transport_error_retries_then_succeeds() {
    // Given: 最初の接続は応答せず切断し、次は完了する。
    let server = TcpMockServer::start(vec![
        TcpBehavior::DropNoResponse,
        TcpBehavior::FullSse(FULL),
    ]);
    let bus = EventBus::new(8);
    // When: 実 HTTP 経由で送信する。
    let result = send(&server.base_url, &bus).await;
    // Then: transport 障害を再試行し、2接続で成功する。
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn consecutive_failures_stop_with_retries_exhausted() {
    // Given: 全接続が HTTP 応答を返さず切断する。
    let server = TcpMockServer::start(vec![TcpBehavior::DropNoResponse]);
    let bus = EventBus::new(8);
    // When: 実 HTTP 経由で送信する。
    let result = send(&server.base_url, &bus).await;
    // Then: transport の原因を保持して3回で停止する。
    assert!(
        matches!(result, Err(ProviderError::RetriesExhausted { attempts: 3, last })
        if matches!(*last, ProviderError::Transport { .. }))
    );
    assert_eq!(server.connections.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn auth_error_is_not_retried() {
    // Given: 認証エラーを返す HTTP サーバー。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(json_response(401, r#"{"error":"unauthorized"}"#))
        .expect(1)
        .mount(&server)
        .await;
    let bus = EventBus::new(8);
    // When: 実 HTTP 経由で送信する。
    let result = send(&server.uri(), &bus).await;
    // Then: ラップされていない401を返し、要求は1回だけ。
    assert!(matches!(
        result,
        Err(ProviderError::Http { status: 401, .. })
    ));
    server.verify().await;
}

#[tokio::test]
async fn server_error_500_retries_then_succeeds() {
    // Given: 最初は500、次の接続は正常な SSE 完了。
    let server = TcpMockServer::start(vec![TcpBehavior::ServerError, TcpBehavior::FullSse(FULL)]);
    let bus = EventBus::new(8);
    // When: 実 HTTP 経由で送信する。
    let result = send(&server.base_url, &bus).await;
    // Then: 500を再試行し、2接続で成功する。
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
}

/// 実 HTTP の切断を接続ごとの応答表で再現する TCP モック。
///
/// 共有 support へ置くと利用しないテストバイナリで dead_code になるため、
/// 利用するこのバイナリ専用のモジュールとして保持する (CI の --all-targets 対応)。
mod tcp_mock {
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread::JoinHandle;
    use std::time::Duration;

    /// 接続ごとの応答。表の末尾は以降の接続でも繰り返す。
    pub enum TcpBehavior {
        DropNoResponse,
        PartialThenDrop(&'static str),
        FullSse(&'static str),
        ServerError,
    }

    /// 実 HTTP の切断を再現し、スコープ終了時にスレッドを停止する。
    pub struct TcpMockServer {
        pub base_url: String,
        pub connections: Arc<AtomicUsize>,
        shutdown: Option<mpsc::Sender<()>>,
        worker: Option<JoinHandle<io::Result<()>>>,
    }

    impl TcpMockServer {
        pub fn start(behaviors: Vec<TcpBehavior>) -> Self {
            assert!(!behaviors.is_empty(), "応答表は空にできない");
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let base_url = format!("http://{}", listener.local_addr().expect("address"));
            listener.set_nonblocking(true).expect("nonblocking");
            let connections = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&connections);
            let (shutdown, stop) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                loop {
                    match stop.try_recv() {
                        Ok(()) | Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                    let (mut stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            // 接続待ちも終了通知で起こし、accept による永久待機を避ける。
                            match stop.recv_timeout(Duration::from_millis(5)) {
                                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            }
                        }
                        Err(error) => return Err(error),
                    };
                    let index = counter.fetch_add(1, Ordering::SeqCst);
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                    let mut reader = BufReader::new(&stream);
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line)? == 0 {
                            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                        }
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':')
                            && name.eq_ignore_ascii_case("content-length")
                        {
                            length = value.trim().parse::<usize>().expect("content-length");
                        }
                    }
                    reader.read_exact(&mut vec![0; length])?;
                    match &behaviors[index.min(behaviors.len() - 1)] {
                        TcpBehavior::DropNoResponse => {}
                        TcpBehavior::PartialThenDrop(body) | TcpBehavior::FullSse(body) => {
                            // close-delimited HTTP は正常終了し、DONE の欠落だけを再現する。
                            write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
                            )?;
                        }
                        TcpBehavior::ServerError => {
                            stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":\"failed\"}")?;
                        }
                    }
                }
            });
            Self {
                base_url,
                connections,
                shutdown: Some(shutdown),
                worker: Some(worker),
            }
        }
    }

    impl Drop for TcpMockServer {
        fn drop(&mut self) {
            drop(self.shutdown.take());
            if let Some(worker) = self.worker.take() {
                let result = worker.join();
                // 元の assertion の panic を二重 panic で隠さない。
                if !std::thread::panicking() {
                    result.expect("TCP mock thread").expect("TCP mock I/O");
                }
            }
        }
    }
}
