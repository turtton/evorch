//! 公式 OpenAI Responses の全体タイムアウトが非ストリーミングだけに掛かることを、
//! 手動で進める Tokio 時計と実 TCP で固定する契約テストです。
pub mod support;

use std::sync::Arc;
use std::time::Duration;

use event_bus::{EventBus, EventKind, EventReceiver, ProviderEvent, ProviderFailureKind};
use futures_util::{FutureExt, StreamExt};
use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::{ChatRequest, ProviderAuth, ProviderClient, ProviderError, StreamEvent};
use support::fixture;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::{Layer, layer::SubscriberExt};

const HEADERS: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
const PARTIAL: &str = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n";

// Given: a send whose SSE headers and first delta arrive before its non-streaming timeout
// When: the body stalls and the manual clock passes the timeout
// Then: send stays pending until the deadline, then fails with Timeout and emits neither usage
// nor a completed attempt
#[tokio::test(start_paused = true)]
async fn send_times_out_while_receiving_the_sse_body() {
    let mut exchange = Exchange::start(
        Duration::from_secs(10),
        |client, request, consumed| async move {
            // send hides display deltas; this trace is emitted only after a nonempty SSE
            // text delta has been read and decoded, not merely after headers or a poll.
            let subscriber = tracing_subscriber::registry().with(BodyConsumed(consumed));
            client
                .send(&ProviderAuth::new("sk-contract"), &request)
                .with_subscriber(subscriber)
                .await
        },
    )
    .await;
    exchange
        .write_progress(&format!("{HEADERS}{}", chunk(PARTIAL)))
        .await;

    tokio::time::advance(Duration::from_secs(9)).await;
    exchange.assert_pending().await;
    tokio::time::advance(Duration::from_secs(1)).await;

    let mut receiver = exchange.assert_finished_before_checkpoint().await;
    assert_eq!(
        exchange.request.await.unwrap().unwrap_err(),
        ProviderError::Timeout
    );
    let events = drain(&mut receiver);
    assert!(
        !events.iter().any(|event| matches!(
            event,
            EventKind::Usage(_) | EventKind::Provider(ProviderEvent::RequestCompleted { .. })
        )),
        "{events:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        EventKind::Provider(ProviderEvent::RequestFailed {
            failure: ProviderFailureKind::Timeout,
            ..
        })
    )));
}

// Given: a stream with a short non-streaming timeout whose first delta has arrived
// When: the manual clock passes far beyond that timeout before the rest of the SSE arrives
// Then: the stream stays open and completes with the full response and its usage
#[tokio::test(start_paused = true)]
async fn stream_outlasts_the_non_streaming_timeout() {
    let mut exchange = Exchange::start(
        Duration::from_secs(1),
        |client, request, consumed| async move {
            let mut stream = client
                .stream(&ProviderAuth::new("sk-contract"), &request)
                .await?;
            let first = stream.next().await.expect("first SSE delta")?;
            assert_eq!(
                first,
                StreamEvent::TextDelta {
                    text: "partial".into()
                }
            );
            consumed.notify_one();
            let mut events = vec![Ok(first)];
            events.extend(stream.collect::<Vec<_>>().await);
            Ok::<_, ProviderError>(events)
        },
    )
    .await;
    exchange
        .write_progress(&format!("{HEADERS}{}", chunk(PARTIAL)))
        .await;

    tokio::time::advance(Duration::from_secs(30)).await;
    exchange.assert_pending().await;
    exchange
        .write(&format!(
            "{}0\r\n\r\n",
            chunk(&fixture("openai", "responses_text.sse"))
        ))
        .await;

    let mut receiver = exchange.receiver.take().unwrap();
    let events = exchange
        .request
        .await
        .unwrap()
        .expect("stream を開始できる");
    let completed = events
        .into_iter()
        .map(|event| event.expect("stream は失敗しない"))
        .find_map(|event| match event {
            StreamEvent::Completed { response } => Some(response),
            _ => None,
        })
        .expect("stream は完了する");
    assert_eq!(completed.usage.input_tokens, 11);
    assert!(
        drain(&mut receiver)
            .iter()
            .any(|event| matches!(event, EventKind::Usage(_)))
    );
}

fn chunk(body: &str) -> String {
    format!("{:x}\r\n{body}\r\n", body.len())
}

fn request() -> ChatRequest {
    ChatRequest {
        responses_replay: None,
        model: "gpt-contract".into(),
        messages: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        reasoning_effort: None,
        service_tier: None,
        output_schema: None,
        observation: None,
    }
}

fn drain(receiver: &mut EventReceiver) -> Vec<EventKind> {
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().now_or_never() {
        events.push(event.expect("観測イベント").kind);
    }
    events
}

/// 実 TCP の 1 往復。server 側 socket へ任意の時点で応答を書き、要求 future が
/// 保留中かを checkpoint で確かめる。
struct Exchange<T> {
    socket: TcpStream,
    request: JoinHandle<T>,
    checkpoints: mpsc::UnboundedSender<oneshot::Sender<()>>,
    consumed: Arc<Notify>,
    receiver: Option<EventReceiver>,
    _clock: ManualClock,
}

impl<T: Send + std::fmt::Debug + 'static> Exchange<T> {
    async fn start<F, Fut>(timeout: Duration, call: F) -> Self
    where
        F: FnOnce(OpenAiClient, ChatRequest, Arc<Notify>) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send,
    {
        let clock = ManualClock::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (received, request_received) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            socket.set_nodelay(true).unwrap();
            let mut reader = BufReader::new(socket);
            let mut content_length = None;
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    content_length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut body = vec![0; content_length.expect("JSON request has content-length")];
            reader.read_exact(&mut body).await.unwrap();
            received.send(reader.into_inner()).unwrap();
        });
        let bus = Arc::new(EventBus::new(64));
        let receiver = bus.subscribe();
        let client = OpenAiClient::new(OpenAiConfig {
            base_url,
            timeout,
            event_bus: Some(bus),
        })
        .unwrap();
        let (checkpoints, mut checkpoint_rx) = mpsc::unbounded_channel::<oneshot::Sender<()>>();
        let consumed = Arc::new(Notify::new());
        let request_consumed = consumed.clone();
        let mut request = tokio::spawn(async move {
            let call = call(client, request(), request_consumed);
            tokio::pin!(call);
            loop {
                tokio::select! {
                    biased;
                    result = &mut call => return result,
                    checkpoint = checkpoint_rx.recv() => {
                        checkpoint.unwrap().send(()).unwrap();
                    }
                }
            }
        });
        let socket = tokio::select! {
            biased;
            result = &mut request => panic!("request ended before the server received it: {result:?}"),
            socket = request_received => socket.unwrap(),
        };
        server.await.unwrap();
        let mut exchange = Self {
            socket,
            request,
            checkpoints,
            consumed,
            receiver: Some(receiver),
            _clock: clock,
        };
        exchange.assert_pending().await;
        exchange
    }

    fn checkpoint(&self) -> oneshot::Receiver<()> {
        let (send, receive) = oneshot::channel();
        let _ = self.checkpoints.send(send);
        receive
    }

    async fn assert_pending(&mut self) {
        if self.checkpoint().await.is_err() {
            panic!("request ended prematurely: {:?}", (&mut self.request).await);
        }
    }

    /// 要求 future が checkpoint を受ける前に終わったことを確かめ、観測受信側を返す。
    async fn assert_finished_before_checkpoint(&mut self) -> EventReceiver {
        assert!(
            self.checkpoint().await.is_err(),
            "request remained pending after its deadline"
        );
        self.receiver.take().unwrap()
    }

    async fn write(&self, response: &str) {
        self.socket.writable().await.unwrap();
        assert_eq!(
            self.socket.try_write(response.as_bytes()).unwrap(),
            response.len(),
            "the small SSE fixture must fit in the socket send buffer"
        );
    }

    /// 書いた応答を要求 future が読み、なお保留中であることまで待つ。
    async fn write_progress(&mut self, response: &str) {
        self.write(response).await;
        tokio::select! {
            biased;
            result = &mut self.request => panic!("request ended while receiving progress: {result:?}"),
            () = self.consumed.notified() => {}
        }
        self.assert_pending().await;
    }
}

/// send does not expose deltas; observe the Responses decoder without changing
/// non-streaming TTFT telemetry or relying on transport scheduling.
struct BodyConsumed(Arc<Notify>);

impl<S: tracing::Subscriber> Layer<S> for BodyConsumed {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() == "providers::responses::text_delta_consumed" {
            self.0.notify_one();
        }
    }
}

// Paused Tokio time otherwise advances automatically while socket readiness is
// delivered by the OS. Keeping a runnable task makes advance() the only clock driver.
struct ManualClock(JoinHandle<()>);

impl ManualClock {
    fn new() -> Self {
        Self(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }))
    }
}

impl Drop for ManualClock {
    fn drop(&mut self) {
        self.0.abort();
    }
}
