use std::sync::Arc;
use std::time::Duration;

use futures_util::{FutureExt, future::poll_fn};
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{CompactionResult, Compactor, ProviderError};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use super::{SUCCESS, request, seeded_store};

const HEADERS: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
const PROGRESS: &str = "data: {\"type\":\"response.in_progress\"}\n\n";

#[tokio::test(start_paused = true)]
async fn official_compaction_outlasts_normal_request_and_read_timeouts() {
    for normal_timeout in [CodexConfig::default().timeout, Duration::from_secs(1)] {
        let mut exchange = Exchange::start(normal_timeout).await;

        tokio::time::advance(Duration::from_secs(120)).await;
        exchange.assert_pending().await;
        exchange
            .write(&format!("{HEADERS}{}", chunk(SUCCESS)))
            .await;

        let result = exchange.request.await.unwrap().unwrap();
        assert_eq!(result.encrypted_content, "opaque+/=");
        assert_eq!(result.usage.input_tokens, 42);
    }
}

#[tokio::test(start_paused = true)]
async fn official_compaction_times_out_after_300_seconds_without_response() {
    let mut exchange = Exchange::start(Duration::from_secs(60)).await;

    tokio::time::advance(Duration::from_secs(299)).await;
    exchange.assert_pending().await;
    tokio::time::advance(Duration::from_secs(1)).await;

    exchange.assert_timeout().await;
}

#[tokio::test(start_paused = true)]
async fn official_compaction_times_out_after_600_seconds_despite_sse_progress() {
    let mut exchange = Exchange::start(Duration::from_secs(60)).await;
    exchange
        .write_progress(&format!("{HEADERS}{}", chunk(PROGRESS)))
        .await;

    for elapsed in [200, 200, 199] {
        tokio::time::advance(Duration::from_secs(elapsed)).await;
        exchange.assert_pending().await;
        exchange.write_progress(&chunk(PROGRESS)).await;
    }
    tokio::time::advance(Duration::from_secs(1)).await;

    exchange.assert_timeout().await;
}

fn chunk(body: &str) -> String {
    format!("{:x}\r\n{body}\r\n", body.len())
}

struct Exchange {
    socket: TcpStream,
    request: JoinHandle<Result<CompactionResult, ProviderError>>,
    checkpoints: mpsc::UnboundedSender<oneshot::Sender<()>>,
    polled: Arc<Notify>,
    _clock: ManualClock,
}

impl Exchange {
    async fn start(normal_timeout: Duration) -> Self {
        let clock = ManualClock::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (received, request_received) = oneshot::channel();
        let server = tokio::spawn(async move {
            // The client reads the model catalog once before its first request; answer it
            // on its own connection and keep the next one for the compaction request.
            let (mut reader, content_length) = loop {
                let (socket, _) = listener.accept().await.unwrap();
                socket.set_nodelay(true).unwrap();
                let mut reader = BufReader::new(socket);
                let mut content_length = None;
                let mut first_line = None;
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    first_line.get_or_insert_with(|| line.clone());
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        content_length = Some(value.trim().parse::<usize>().unwrap());
                    }
                }
                if first_line.is_some_and(|line| line.starts_with("GET ")) {
                    let body = r#"{"models":[]}"#;
                    reader
                        .get_mut()
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                    continue;
                }
                break (reader, content_length);
            };
            let mut body = vec![0; content_length.expect("JSON request has content-length")];
            reader.read_exact(&mut body).await.unwrap();
            received.send(reader.into_inner()).unwrap();
        });
        let client = CodexClient::with_config(
            CodexConfig {
                base_url: base_url.clone(),
                auth_base_url: base_url,
                timeout: normal_timeout,
                ..CodexConfig::default()
            },
            seeded_store(),
        )
        .unwrap();
        let (checkpoints, mut checkpoint_rx) = mpsc::unbounded_channel::<oneshot::Sender<()>>();
        let polled = Arc::new(Notify::new());
        let request_polled = polled.clone();
        let mut request = tokio::spawn(async move {
            let request = request();
            let mut compact = client.compact(&request);
            let compact = poll_fn(|context| {
                let result = compact.as_mut().poll(context);
                if result.is_pending() {
                    request_polled.notify_one();
                }
                result
            });
            tokio::pin!(compact);
            loop {
                tokio::select! {
                    biased;
                    result = &mut compact => return result,
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
            polled,
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
        let checkpoint = self.checkpoint();
        if checkpoint.await.is_err() {
            panic!(
                "compaction ended prematurely: {:?}",
                (&mut self.request).await
            );
        }
    }

    async fn assert_timeout(mut self) {
        assert!(
            self.checkpoint().await.is_err(),
            "compaction remained pending after its deadline"
        );
        assert_eq!(
            (&mut self.request).await.unwrap().unwrap_err(),
            ProviderError::Timeout
        );
    }

    async fn write(&self, response: &str) {
        self.socket.writable().await.unwrap();
        assert_eq!(
            self.socket.try_write(response.as_bytes()).unwrap(),
            response.len(),
            "the small SSE fixture must fit in the socket send buffer"
        );
    }

    async fn write_progress(&mut self, response: &str) {
        self.socket.writable().await.unwrap();
        while self.polled.notified().now_or_never().is_some() {}
        assert_eq!(
            self.socket.try_write(response.as_bytes()).unwrap(),
            response.len()
        );
        tokio::select! {
            biased;
            result = &mut self.request => panic!("compaction ended while receiving progress: {result:?}"),
            () = self.polled.notified() => {}
        }
        self.assert_pending().await;
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
