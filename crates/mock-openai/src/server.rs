//! Blocking, one-request-per-connection HTTP transport for scripted completions.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

use crate::ScriptedResponse;

mod prompt_cache;

use prompt_cache::PromptCache;

const CODEX_BASE_PATH: &str = "/backend-api/codex";

/// A parsed request, retained even when the response queue is exhausted.
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    /// The Codex `chatgpt-account-id` header, identifying the account the request used.
    pub chatgpt_account_id: Option<String>,
    pub body: Value,
    pub stream: bool,
}

/// Holds a scripted response until the test explicitly releases it.
/// Arrival notifications are sent after the HTTP request has been recorded,
/// before any response bytes are written. Release the gate before dropping the
/// mock server, including on assertion failure, so its worker threads can join.
#[derive(Clone, Debug)]
pub struct ResponseGate {
    arrived: mpsc::Sender<()>,
    released: Arc<(Mutex<bool>, Condvar)>,
}

impl ResponseGate {
    /// Creates a gate and a receiver reporting each response waiting at it.
    pub fn new() -> (Self, mpsc::Receiver<()>) {
        let (arrived, arrivals) = mpsc::channel();
        (
            Self {
                arrived,
                released: Arc::new((Mutex::new(false), Condvar::new())),
            },
            arrivals,
        )
    }

    /// Releases all current and future responses sharing this gate.
    pub fn release(&self) {
        let (released, changed) = &*self.released;
        *released.lock().unwrap_or_else(|error| error.into_inner()) = true;
        changed.notify_all();
    }

    pub(crate) fn wait(&self) {
        let _ = self.arrived.send(());
        let (released, changed) = &*self.released;
        let released = released.lock().unwrap_or_else(|error| error.into_inner());
        drop(
            changed
                .wait_while(released, |released| !*released)
                .unwrap_or_else(|error| error.into_inner()),
        );
    }
}

/// Controls SSE writes, not TCP packet or client read boundaries.
#[derive(Clone, Debug, Default)]
pub enum WriteMode {
    WholeBody,
    #[default]
    FramePerWrite,
    /// Pause after the first frame until the test releases the remaining response.
    GatedAfterFirst(Arc<Mutex<std::sync::mpsc::Receiver<()>>>),
}

/// Responses requests to let through, then to reject with `401 Unauthorized`.
#[derive(Clone, Copy, Default)]
struct UnauthorizedPlan {
    skip: usize,
    reject: usize,
}

/// A local OpenAI-compatible fixture serving SSE or JSON from the same scripts.
pub struct StreamingMockOpenAi {
    base_url: String,
    address: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    scripts: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    oauth_token: Arc<Mutex<Option<Value>>>,
    unauthorized: Arc<Mutex<UnauthorizedPlan>>,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

impl StreamingMockOpenAi {
    /// Starts a fixture with each SSE frame written and flushed individually.
    ///
    /// # Panics
    /// Panics if the OS cannot create the listener or accept thread.
    pub fn spawn(responses: Vec<ScriptedResponse>) -> Self {
        Self::spawn_with(responses, WriteMode::default())
    }

    /// Starts a fixture with deterministic prefix-cache usage in JSON and SSE.
    ///
    /// `/v1/chat/completions`, `/v1/responses` and the Codex backend's
    /// `/backend-api/codex/responses` participate; the latter two are answered as
    /// Responses SSE with the usage in `response.completed`.
    /// One UTF-8 byte of canonical request settings and messages (Responses:
    /// `instructions`/`tools` settings and `input` items) is one synthetic
    /// token. The longest input prefix among the last 64 requests with the same
    /// model and `prompt_cache_key` is reported as cached. `stream` and
    /// `stream_options` do not affect this prefix. Scripted completion usage is
    /// preserved; scripted prompt usage is replaced.
    ///
    /// This fixture detects prompt-history mutations. It does not predict real
    /// tokenization, provider cache TTL/routing/block sizes, or billing.
    ///
    /// # Panics
    /// Panics if the OS cannot create the listener or accept thread.
    pub fn spawn_with_prompt_cache(responses: Vec<ScriptedResponse>) -> Self {
        Self::spawn_internal(
            responses,
            WriteMode::default(),
            vec!["mock-model".to_owned()],
            true,
        )
    }

    /// Like [`Self::spawn_with_prompt_cache`], listing `models` instead of `mock-model`.
    ///
    /// # Panics
    /// Panics if the OS cannot create the listener or accept thread.
    pub fn spawn_with_prompt_cache_models(
        responses: Vec<ScriptedResponse>,
        models: Vec<String>,
    ) -> Self {
        Self::spawn_internal(responses, WriteMode::default(), models, true)
    }

    /// Starts a fixture with the selected SSE write strategy.
    ///
    /// # Panics
    /// Panics if the OS cannot create the listener or accept thread. This
    /// test-fixture constructor returns `Self`, so setup failures are fatal.
    pub fn spawn_with(responses: Vec<ScriptedResponse>, mode: WriteMode) -> Self {
        Self::spawn_with_models(responses, mode, vec!["mock-model".to_owned()])
    }

    /// Starts a fixture serving the given model IDs in order, independently of scripts.
    ///
    /// # Panics
    /// Panics if the OS cannot create the listener or accept thread.
    pub fn spawn_with_models(
        responses: Vec<ScriptedResponse>,
        mode: WriteMode,
        models: Vec<String>,
    ) -> Self {
        Self::spawn_internal(responses, mode, models, false)
    }

    fn spawn_internal(
        responses: Vec<ScriptedResponse>,
        mode: WriteMode,
        models: Vec<String>,
        simulate_prompt_cache: bool,
    ) -> Self {
        let prompt_cache =
            simulate_prompt_cache.then(|| Arc::new(Mutex::new(PromptCache::default())));
        let models: Vec<_> = models
            .into_iter()
            .map(|id| {
                (
                    json!({"id": id, "object": "model", "created": 0, "owned_by": "mock-openai"}),
                    json!({"slug": id}),
                )
            })
            .collect();
        let (models, codex_models): (Vec<_>, Vec<_>) = models.into_iter().unzip();
        let models = json!({"object": "list", "data": models});
        let codex_models = json!({"models": codex_models});
        let listener = TcpListener::bind("127.0.0.1:0")
            .unwrap_or_else(|error| panic!("mock_openai bind failed: {error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("mock_openai local address failed: {error}"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let scripts = Arc::new(Mutex::new(VecDeque::from(responses)));
        let oauth_token = Arc::new(Mutex::new(None));
        let unauthorized = Arc::new(Mutex::new(UnauthorizedPlan::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let recorded = Arc::clone(&requests);
        let queued = Arc::clone(&scripts);
        let token = Arc::clone(&oauth_token);
        let rejecting = Arc::clone(&unauthorized);
        let stopping = Arc::clone(&shutdown);
        let accept_thread = thread::spawn(move || {
            let mut connections = Vec::new();
            while !stopping.load(Ordering::SeqCst) {
                let accepted = listener.accept();
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                let (mut stream, _) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => {
                        eprintln!("mock_openai accept failed: error={error}");
                        break;
                    }
                };
                let recorded = Arc::clone(&recorded);
                let queued = Arc::clone(&queued);
                let token = Arc::clone(&token);
                let rejecting = Arc::clone(&rejecting);
                let models = models.clone();
                let codex_models = codex_models.clone();
                let mode = mode.clone();
                let prompt_cache = prompt_cache.clone();
                connections.push(thread::spawn(move || {
                let result = (|| -> io::Result<()> {
                    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
                    let request = read_request(&mut stream)?;
                    let responses_api = request.method == "POST"
                        && (request.path == "/v1/responses"
                            || request.path == format!("{CODEX_BASE_PATH}/responses"));
                    // The Responses API answers with SSE regardless of the request flag.
                    let streaming = request.stream || responses_api;
                    let listed_path = request.path.split('?').next();
                    let listing_models =
                        request.method == "GET" && listed_path == Some("/v1/models");
                    let listing_codex_models = request.method == "GET"
                        && listed_path == Some(&format!("{CODEX_BASE_PATH}/models"));
                    let refreshing_token = request.method == "POST"
                        && listed_path.is_some_and(|path| path.ends_with("/oauth/token"));
                    let prompt = (prompt_cache.is_some()
                        && request.method == "POST"
                        && (request.path == "/v1/chat/completions" || responses_api))
                        .then(|| request.body.clone());
                    // Poison recovery retains fixture evidence instead of panicking again.
                    recorded
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(request);
                    if listing_models {
                        return write_json(&mut stream, "200 OK", &models);
                    }
                    if listing_codex_models {
                        return write_json(&mut stream, "200 OK", &codex_models);
                    }
                    if refreshing_token {
                        let token = token.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        return match token {
                            Some(token) => write_json(&mut stream, "200 OK", &token),
                            None => write_json(
                                &mut stream,
                                "401 Unauthorized",
                                &json!({"error": "unscripted token refresh"}),
                            ),
                        };
                    }
                    if responses_api && {
                        let mut plan = rejecting.lock().unwrap_or_else(|e| e.into_inner());
                        if plan.skip > 0 {
                            plan.skip -= 1;
                            false
                        } else if plan.reject > 0 {
                            plan.reject -= 1;
                            true
                        } else {
                            false
                        }
                    } {
                        return write_json(
                            &mut stream,
                            "401 Unauthorized",
                            &json!({"error": {"message": "scripted rejection", "type": "mock_openai"}}),
                        );
                    }
                    let response = queued.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                    let response = response.map(|response| {
                        if let (Some(cache), Some(prompt)) = (&prompt_cache, &prompt) {
                            let (input_tokens, cached_tokens) = cache
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .observe(prompt);
                            response.with_prompt_cache_usage(input_tokens, cached_tokens)
                        } else {
                            response
                        }
                    });
                    if let Some(response) = &response {
                        if let Some(gate) = &response.gate { gate.wait(); }
                        thread::sleep(response.delay);
                    }
                    match response {
                        Some(response) if responses_api => {
                            write_sse(&mut stream, &response.responses_frames(), mode)
                        }
                        Some(response) if streaming => {
                            write_sse(&mut stream, &response.frames(), mode)
                        }
                        Some(response) => {
                            write_json(&mut stream, "200 OK", &response.to_non_streaming_json())
                        }
                        None => write_json(
                            &mut stream,
                            "500 Internal Server Error",
                            &json!({
                                "error": {"message": "unscripted request", "type": "mock_openai"}
                            }),
                        ),
                    }
                })();
                if let Err(error) = result {
                    eprintln!("mock_openai request failed: error={error}");
                }
                }));
            }
            for connection in connections {
                let _ = connection.join();
            }
        });
        Self {
            base_url: format!("http://{address}/v1"),
            address,
            requests,
            scripts,
            oauth_token,
            unauthorized,
            shutdown,
            accept_thread: Some(accept_thread),
        }
    }

    /// Returns the API base URL, including `/v1`.
    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    /// Returns a Codex subscription backend base URL on this fixture.
    ///
    /// `{base}/responses` is served like `/v1/responses`, and `GET {base}/models`
    /// lists the fixture's models as the Codex catalog (`{"models": [{"slug"}]}`).
    /// Credentials and Codex headers are recorded but not checked.
    pub fn codex_base_url(&self) -> String {
        format!("http://{}{CODEX_BASE_PATH}", self.address)
    }

    /// Returns an owned snapshot of requests in arrival order.
    pub fn recorded_requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Returns the number of scripts not yet consumed.
    pub fn remaining_scripts(&self) -> usize {
        self.scripts.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Lets `skip` more Responses requests through, then answers the next `reject`
    /// with `401 Unauthorized`. A rejected request is recorded but consumes no
    /// response script and never reaches the prompt cache.
    pub fn reject_responses_unauthorized(&self, skip: usize, reject: usize) {
        *self.unauthorized.lock().unwrap_or_else(|e| e.into_inner()) =
            UnauthorizedPlan { skip, reject };
    }

    /// Answers every following `POST …/oauth/token` (a Codex token refresh against an
    /// auth base URL on this fixture) with `body`, until replaced.
    ///
    /// Refreshes are recorded but never consume a response script or touch the
    /// prompt cache. Before the first call, a refresh is answered `401 Unauthorized`.
    pub fn set_oauth_token_response(&self, body: Value) {
        *self.oauth_token.lock().unwrap_or_else(|e| e.into_inner()) = Some(body);
    }
}

impl Drop for StreamingMockOpenAi {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Err(error) = TcpStream::connect(self.address) {
            eprintln!("mock_openai shutdown wake failed: error={error}");
        }
        if let Some(handle) = self.accept_thread.take()
            && handle.join().is_err()
        {
            eprintln!("mock_openai accept thread panicked");
        }
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<RecordedRequest> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request method"))?
        .to_owned();
    let path = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request path"))?
        .to_owned();
    let mut content_length = 0;
    let mut authorization = None;
    let mut chatgpt_account_id = None;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if line == "\r\n" {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP header"))?;
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value
                .trim()
                .parse::<usize>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        } else if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.trim().to_owned());
        } else if name.eq_ignore_ascii_case("chatgpt-account-id") {
            chatgpt_account_id = Some(value.trim().to_owned());
        }
    }
    let mut bytes = vec![0; content_length];
    reader.read_exact(&mut bytes)?;
    let body: Value = if method == "GET" && bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
    };
    Ok(RecordedRequest {
        method,
        path,
        authorization,
        chatgpt_account_id,
        stream: body["stream"] == true,
        body,
    })
}

fn write_sse(stream: &mut TcpStream, frames: &[String], mode: WriteMode) -> io::Result<()> {
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n")?;
    match mode {
        WriteMode::WholeBody => stream.write_all(frames.concat().as_bytes())?,
        WriteMode::FramePerWrite => {
            for frame in frames {
                stream.write_all(frame.as_bytes())?;
                stream.flush()?;
            }
        }
        WriteMode::GatedAfterFirst(release) => {
            for (index, frame) in frames.iter().enumerate() {
                stream.write_all(frame.as_bytes())?;
                stream.flush()?;
                if index == 0 {
                    release
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))?;
                }
            }
        }
    }
    Ok(())
}

fn write_json(stream: &mut TcpStream, status: &str, body: &Value) -> io::Result<()> {
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
