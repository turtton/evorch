//! 契約テスト共通のサポートユーティリティ。
//!
//! 各統合テストバイナリから `mod support;` で取り込んで使う。

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use event_bus::{Event, EventKind, EventReceiver, ProviderEvent, UsageEvent};
use wiremock::ResponseTemplate;

/// `tests/fixtures/<provider>/<name>` の内容を読み込んで返す。
///
/// # Panics
/// フィクスチャが存在しない・読めない場合にパニックする (テスト失敗として扱う)。
pub fn fixture(provider: &str, name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(provider)
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("フィクスチャの読み込みに失敗しました: {path:?}: {err}"))
}

/// SSE 本文を返す 200 応答テンプレートを生成する。
///
/// Content-Type は `text/event-stream`。
pub fn sse_response(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// 指定ステータスの JSON 応答テンプレートを生成する。
///
/// Content-Type は `application/json`。
pub fn json_response(status: u16, body: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(body, "application/json")
}

/// バスから次の usage イベントを受信して返す。
///
/// # Panics
/// 受信に失敗した場合、または受信したイベントが [`EventKind::Usage`]
/// でない場合に文脈付きでパニックする。
pub async fn next_usage_event(rx: &mut EventReceiver) -> UsageEvent {
    match next_event(rx).await.kind {
        EventKind::Usage(usage) => usage,
        other => panic!("Usage イベントを期待しましたが、別のイベントを受信しました: {other:?}"),
    }
}

/// バスから次の provider イベントを受信して返す。
///
/// # Panics
/// 受信に失敗した場合、または受信したイベントが [`EventKind::Provider`]
/// でない場合に文脈付きでパニックする。
pub async fn next_provider_event(rx: &mut EventReceiver) -> ProviderEvent {
    match next_event(rx).await.kind {
        EventKind::Provider(provider) => provider,
        other => panic!("Provider イベントを期待しましたが、別のイベントを受信しました: {other:?}"),
    }
}

/// バスから次のイベントをタイムアウト付きで受信する。
///
/// # Panics
/// 1 秒以内に受信できない場合、または受信処理が失敗した場合にパニックする。
pub async fn next_event(rx: &mut EventReceiver) -> Event {
    tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("1 秒以内にイベントを受信できる")
        .unwrap_or_else(|err| panic!("イベントの受信に失敗しました: {err:?}"))
}

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
