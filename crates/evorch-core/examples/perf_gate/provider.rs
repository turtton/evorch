//! The model provider both arms talk to: an OpenAI-compatible chat-completions
//! server that answers by what the request says, not by queue position, and
//! stamps the moments the shell RTT is cut from.
//!
//! A scripted FIFO (mock-openai) cannot serve two active agents and the
//! measured probes at once: which request gets which response would depend on
//! timing. Here the last user message carries a marker (`perf:<kind>:...`) and
//! the answer is a function of the marker and of whether the request already
//! holds a tool result.
use mock_openai::ScriptedResponse;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
    task::JoinHandle,
};

/// Moments of one probe, all read from the controller's monotonic clock.
#[derive(Clone, Debug, Default)]
pub struct Mark {
    /// The tool result text the core sent back (first 2 KiB), so a workload that
    /// did not run as intended is rejected instead of timed.
    pub tool_result: Option<String>,
    /// The tool call was written and flushed to the core.
    pub tool_sent: Option<Instant>,
    /// The request that carries the tool result had arrived completely.
    pub followup: Option<Instant>,
    /// The closing text was written.
    pub final_sent: Option<Instant>,
}

struct Shared {
    marks: Mutex<HashMap<String, Mark>>,
    served: Mutex<u64>,
    notify: Notify,
    commands: Vec<String>,
    project: String,
    reply_bytes: usize,
}

pub struct Provider {
    pub base_url: String,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Provider {
    /// `commands[i]` is the shell command of probe workload `i`.
    pub async fn spawn(
        project: &Path,
        commands: Vec<String>,
        reply_bytes: usize,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let shared = Arc::new(Shared {
            marks: Mutex::new(HashMap::new()),
            served: Mutex::new(0),
            notify: Notify::new(),
            commands,
            project: project.display().to_string(),
            reply_bytes,
        });
        let state = Arc::clone(&shared);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(error) = serve(stream, &state).await {
                        eprintln!("perf-gate provider: {error}");
                    }
                });
            }
        });
        Ok(Self {
            base_url: format!("http://{address}/v1"),
            shared,
            task,
        })
    }

    pub fn served(&self) -> u64 {
        *self.shared.served.lock().unwrap()
    }

    pub fn mark(&self, id: &str) -> Mark {
        self.shared
            .marks
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// Waits until the closing text of probe `id` has been written.
    pub async fn wait_final(&self, id: &str, limit: Duration) -> io::Result<Mark> {
        let deadline = Instant::now() + limit;
        loop {
            let notified = self.shared.notify.notified();
            let mark = self.mark(id);
            if mark.final_sent.is_some() {
                return Ok(mark);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, notified).await.is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("provider never closed {id}"),
                ));
            }
        }
    }
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The marker of the newest user message and whether the request is a
/// follow-up (its last message is a tool result).
fn marker_of(body: &Value) -> (Option<String>, bool) {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let followup = messages.last().is_some_and(|m| m["role"] == "tool");
    for message in messages.iter().rev() {
        if message["role"] == "user" {
            let text = text_of(&message["content"]);
            if let Some(at) = text.find("perf:") {
                let token: String = text[at..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_'))
                    .collect();
                return (Some(token), followup);
            }
        }
    }
    (None, followup)
}

async fn read_request(stream: &mut TcpStream) -> io::Result<(String, String, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 16384];
    let header_end = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request = lines.next().unwrap_or_default().split_whitespace();
    let method = request.next().unwrap_or_default().to_owned();
    let path = request.next().unwrap_or_default().to_owned();
    let mut length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().map_err(io::Error::other)?;
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                return Err(io::Error::other("chunked request bodies are not supported"));
            }
        }
    }
    while buffer.len() < header_end + length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
    Ok((
        method,
        path,
        buffer[header_end..header_end + length].to_vec(),
    ))
}

async fn write_json(stream: &mut TcpStream, body: &Value) -> io::Result<()> {
    let body = body.to_string();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

fn reply(shared: &Shared, marker: Option<&str>, followup: bool) -> (ScriptedResponse, bool) {
    let text = |body: String| ScriptedResponse::text_stream("perf", "mock-model", [body]);
    let Some(marker) = marker else {
        return (text("ok".into()), true);
    };
    let mut parts = marker.split(':').skip(1);
    let kind = parts.next().unwrap_or_default();
    if followup {
        return (text("done".into()), true);
    }
    match kind {
        "prep" => {
            let body = "r".repeat(shared.reply_bytes);
            let fragments: Vec<String> = body
                .as_bytes()
                .chunks(1024)
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .collect();
            (
                ScriptedResponse::text_stream("perf", "mock-model", fragments),
                true,
            )
        }
        "small" => {
            let args = json!({"path": format!("{}/tracked.txt", shared.project)});
            (
                ScriptedResponse::tool_call(
                    "perf",
                    "mock-model",
                    0,
                    "call",
                    "read",
                    [args.to_string()],
                ),
                false,
            )
        }
        "tick" => {
            let n: usize = marker
                .rsplit(':')
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let args = json!({"path": format!("{}/files/f{:04}.txt", shared.project, n % 1000)});
            (
                ScriptedResponse::tool_call(
                    "perf",
                    "mock-model",
                    0,
                    "call",
                    "read",
                    [args.to_string()],
                ),
                false,
            )
        }
        "probe" => {
            let workload: usize = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let command = shared
                .commands
                .get(workload)
                .cloned()
                .unwrap_or_else(|| ":".into());
            let args = json!({"command": command, "cwd": shared.project, "timeout_ms": 60000});
            (
                ScriptedResponse::tool_call(
                    "perf",
                    "mock-model",
                    0,
                    "call",
                    "shell",
                    [args.to_string()],
                ),
                false,
            )
        }
        _ => (text("ok".into()), true),
    }
}

async fn serve(mut stream: TcpStream, shared: &Shared) -> io::Result<()> {
    let (method, path, body) = read_request(&mut stream).await?;
    let arrived = Instant::now();
    let path = path.split('?').next().unwrap_or_default().to_owned();
    if method == "GET" && path.ends_with("/models") {
        let models = json!({"object":"list","data":[{"id":"mock-model","object":"model","created":0,"owned_by":"perf-gate"}]});
        return write_json(&mut stream, &models).await;
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let (marker, followup) = marker_of(&body);
    if let (Some(marker), true) = (&marker, followup) {
        let mut marks = shared.marks.lock().unwrap();
        let result = body["messages"].as_array().and_then(|m| m.last()).map(|m| {
            text_of(&m["content"])
                .chars()
                .take(2048)
                .collect::<String>()
        });
        let mark = marks.entry(marker.clone()).or_default();
        mark.followup = Some(arrived);
        mark.tool_result = result;
    }
    let (response, closing) = reply(shared, marker.as_deref(), followup);
    let bytes = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n{}",
        response.body()
    );
    stream.write_all(bytes.as_bytes()).await?;
    stream.flush().await?;
    let sent = Instant::now();
    if let Some(marker) = &marker {
        let mut marks = shared.marks.lock().unwrap();
        let mark = marks.entry(marker.clone()).or_default();
        if closing {
            mark.final_sent = Some(sent);
        } else {
            mark.tool_sent = Some(sent);
        }
    }
    *shared.served.lock().unwrap() += 1;
    shared.notify.notify_waiters();
    stream.shutdown().await.ok();
    Ok(())
}
