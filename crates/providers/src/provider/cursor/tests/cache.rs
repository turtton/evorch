//! Event-driven Connect fixture. Cache hits derive from the actual received
//! prompt blobs and action, independently of production request construction.
use super::*;
use std::collections::HashMap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, mpsc},
};

type Mailbox = (mpsc::Sender<Vec<u8>>, Option<mpsc::Receiver<Vec<u8>>>);
type ObservedInput = (Vec<u8>, Vec<Vec<u8>>, u64);
#[derive(Default)]
struct ServerState {
    mailboxes: Mutex<HashMap<String, Mailbox>>,
    cache: Mutex<HashMap<Vec<u8>, Vec<Vec<u8>>>>,
    inputs: Mutex<Vec<ObservedInput>>,
}
impl ServerState {
    async fn sender(&self, id: &str) -> mpsc::Sender<Vec<u8>> {
        self.mailboxes
            .lock()
            .await
            .entry(id.into())
            .or_insert_with(|| {
                let (tx, rx) = mpsc::channel(32);
                (tx, Some(rx))
            })
            .0
            .clone()
    }
    async fn receiver(&self, id: &str) -> mpsc::Receiver<Vec<u8>> {
        self.sender(id).await;
        self.mailboxes
            .lock()
            .await
            .get_mut(id)
            .unwrap()
            .1
            .take()
            .unwrap()
    }
}
struct Harness {
    url: String,
    state: Arc<ServerState>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Harness {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(ServerState::default());
        let shared = state.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    incoming=listener.accept() => { let (socket,_)=incoming.unwrap(); let shared=shared.clone(); connections.spawn(async move { handle(socket,shared).await; }); },
                    joined=connections.join_next(), if !connections.is_empty() => { joined.unwrap().unwrap(); }
                }
            }
        });
        Self { url, state, task }
    }
}
async fn read_request(socket: &mut TcpStream) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let offset = loop {
        if let Some(offset) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
            break offset + 4;
        }
        let n = socket.read(&mut buffer).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&buffer[..n]);
        assert!(bytes.len() < 1024 * 1024);
    };
    let header = std::str::from_utf8(&bytes[..offset]).unwrap();
    let path = header
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let size: usize = header
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < offset + size {
        let n = socket.read(&mut buffer).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&buffer[..n]);
    }
    (path, bytes[offset..offset + size].to_vec())
}
async fn send_frame(socket: &mut TcpStream, message: Proto) {
    let bytes = wire::frame(&message.0);
    send_chunk(socket, &bytes).await;
}
async fn send_chunk(socket: &mut TcpStream, bytes: &[u8]) {
    socket
        .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
        .await
        .unwrap();
    socket.write_all(bytes).await.unwrap();
    socket.write_all(b"\r\n").await.unwrap();
}
async fn handle(mut socket: TcpStream, state: Arc<ServerState>) {
    let (path, body) = read_request(&mut socket).await;
    if path.ends_with("/GetUsableModels") || path.ends_with("/AvailableModels") {
        socket
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        return;
    }
    if path.ends_with("/BidiAppend") {
        let id = wire::string(child(&body, 2), 1).unwrap();
        let data = child(&body, 4).to_vec();
        state.sender(&id).await.send(data).await.unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        return;
    }
    assert!(path.ends_with("/RunSSE"));
    let id = wire::string(&body[5..], 1).unwrap();
    let mut rx = state.receiver(&id).await;
    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/connect+proto\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
    let initial = rx.recv().await.unwrap();
    let run = child(&initial, 1);
    let conversation = wire::string(run, 5).unwrap();
    let mut identity = conversation.as_bytes().to_vec();
    identity.extend_from_slice(child(run, 3));
    identity.extend_from_slice(child(run, 9));
    send_frame(
        &mut socket,
        Proto::new().message(2, Proto::new().integer(1, 1).message(10, Proto::new())),
    )
    .await;
    let context = rx.recv().await.unwrap();
    let context = child(child(&context, 2), 10);
    identity.extend_from_slice(context);
    let root_ids: Vec<_> = wire::fields(child(run, 1))
        .unwrap()
        .into_iter()
        .filter(|f| f.number == 1)
        .map(|f| f.data.to_vec())
        .collect();
    let mut input = Vec::new();
    for (index, blob_id) in root_ids.iter().enumerate() {
        send_frame(
            &mut socket,
            Proto::new().message(
                4,
                Proto::new()
                    .integer(1, index as u64 + 100)
                    .message(2, Proto::new().bytes(1, blob_id)),
            ),
        )
        .await;
        let blob = rx.recv().await.unwrap();
        input.push(child(child(child(&blob, 3), 2), 1).to_vec());
    }
    if let Some(user) = wire::nested(child(run, 2), 1).unwrap() {
        let text = wire::string(child(user, 1), 1).unwrap();
        input.push(
            serde_json::to_vec(&json!({"role":"user","content":[{"type":"text","text":text}]}))
                .unwrap(),
        );
    }
    let cached = {
        let mut cache = state.cache.lock().await;
        let previous = cache.get(&identity);
        let reused = previous.map_or(0, |previous| {
            previous
                .iter()
                .zip(&input)
                .take_while(|(a, b)| a == b)
                .map(|(value, _)| value.len() as u64)
                .sum::<u64>()
        });
        cache.insert(identity.clone(), input.clone());
        reused
    };
    let round = {
        let mut inputs = state.inputs.lock().await;
        let round = inputs.len();
        inputs.push((identity, input.clone(), cached));
        round
    };
    if round == 0 {
        let args = Proto::new()
            .string(1, "lookup")
            .string(3, "cache_call")
            .string(5, "lookup")
            .message(
                2,
                Proto::new()
                    .string(1, "query")
                    .message(2, wire::encode_value(&json!("cache"))),
            );
        send_frame(
            &mut socket,
            Proto::new().message(2, Proto::new().integer(1, 2).message(11, args)),
        )
        .await;
        let handoff = rx.recv().await.unwrap();
        assert!(wire::nested(child(&handoff, 2), 11).unwrap().is_some());
    } else {
        send_frame(
            &mut socket,
            Proto::new().message(
                1,
                Proto::new().message(1, Proto::new().string(1, "Complete")),
            ),
        )
        .await;
    }
    // Server checkpoints intentionally use placeholders. Canonical history must
    // still reconstruct the next request's immutable prompt prefix.
    send_frame(
        &mut socket,
        Proto::new().message(
            3,
            Proto::new()
                .bytes(1, b"server-placeholder")
                .string(4, "old tool call"),
        ),
    )
    .await;
    let used = input.iter().map(|v| v.len() as u64).sum();
    send_frame(
        &mut socket,
        Proto::new().message(
            1,
            Proto::new().message(
                14,
                Proto::new()
                    .integer(1, used)
                    .integer(2, 4)
                    .integer(3, cached),
            ),
        ),
    )
    .await;
    send_chunk(&mut socket, &[2, 0, 0, 0, 2, b'{', b'}']).await;
    socket.write_all(b"0\r\n\r\n").await.unwrap();
}

#[tokio::test]
async fn cursor_cache_prefix_survives_tool_handoff_and_later_user_turn() {
    let harness = Harness::start().await;
    let store = Arc::new(InMemoryCursorTokenStore::new());
    store.save(&token()).unwrap();
    let client = CursorClient::new(
        CursorConfig {
            base_url: harness.url.clone(),
            event_bus: None,
        },
        store,
    )
    .unwrap();
    let mut request = request();
    let first = client.send(&ProviderAuth::new(""), &request).await.unwrap();
    assert_eq!(first.finish_reason, FinishReason::ToolUse);
    assert_eq!(first.usage.cache_read_tokens, 0);
    request.messages.push(first.message);
    request.messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_call_id: "cache_call".into(),
            content: vec![ToolResultContent::Text {
                text: "cache result".into(),
            }],
            is_error: false,
        }],
    });
    let second = client.send(&ProviderAuth::new(""), &request).await.unwrap();
    assert!(second.usage.cache_read_tokens > 0);
    request.messages.push(second.message);
    request.messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: "Continue with the prior answer".into(),
        }],
    });
    let third = client.send(&ProviderAuth::new(""), &request).await.unwrap();
    assert!(third.usage.cache_read_tokens > second.usage.cache_read_tokens);
    let inputs = harness.state.inputs.lock().await;
    assert_eq!(inputs.len(), 3);
    for pair in inputs.windows(2) {
        assert_eq!(
            pair[0].0, pair[1].0,
            "conversation/model/tool schema/rules affinity remains stable"
        );
        assert!(
            pair[1].1.starts_with(&pair[0].1),
            "actual wire prompt blobs preserve sent prefix"
        );
        assert_eq!(
            pair[1].2,
            pair[0].1.iter().map(|v| v.len() as u64).sum::<u64>()
        );
    }
    let second: Vec<Value> = inputs[1]
        .1
        .iter()
        .map(|bytes| serde_json::from_slice(bytes).unwrap())
        .collect();
    assert_eq!(second[2]["content"][0]["toolCallId"], "cache_call");
    assert_eq!(second[3]["content"][0]["result"], "cache result");
}
