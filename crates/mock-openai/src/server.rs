//! Blocking, one-request-per-connection HTTP transport for scripted completions.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

use crate::ScriptedResponse;

/// A parsed request, retained even when the response queue is exhausted.
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub body: Value,
    pub stream: bool,
}

/// Controls SSE writes, not TCP packet or client read boundaries.
#[derive(Clone, Copy, Debug, Default)]
pub enum WriteMode {
    WholeBody,
    #[default]
    FramePerWrite,
}

/// A local OpenAI-compatible fixture serving SSE or JSON from the same scripts.
pub struct StreamingMockOpenAi {
    base_url: String,
    address: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    scripts: Arc<Mutex<VecDeque<ScriptedResponse>>>,
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
        let models: Vec<_> = models
            .into_iter()
            .map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": "mock-openai"}))
            .collect();
        let models = json!({"object": "list", "data": models});
        let listener = TcpListener::bind("127.0.0.1:0")
            .unwrap_or_else(|error| panic!("mock_openai bind failed: {error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("mock_openai local address failed: {error}"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let scripts = Arc::new(Mutex::new(VecDeque::from(responses)));
        let shutdown = Arc::new(AtomicBool::new(false));
        let recorded = Arc::clone(&requests);
        let queued = Arc::clone(&scripts);
        let stopping = Arc::clone(&shutdown);
        let accept_thread = thread::spawn(move || {
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
                let result = (|| -> io::Result<()> {
                    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
                    let request = read_request(&mut stream)?;
                    let streaming = request.stream;
                    let listing_models = request.method == "GET" && request.path == "/v1/models";
                    // Poison recovery retains fixture evidence instead of panicking again.
                    recorded
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(request);
                    if listing_models {
                        return write_json(&mut stream, "200 OK", &models);
                    }
                    let response = queued.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                    match response {
                        Some(response) if streaming => write_sse(&mut stream, &response, mode),
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
            }
        });
        Self {
            base_url: format!("http://{address}/v1"),
            address,
            requests,
            scripts,
            shutdown,
            accept_thread: Some(accept_thread),
        }
    }

    /// Returns the API base URL, including `/v1`.
    pub fn base_url(&self) -> String {
        self.base_url.clone()
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
        stream: body["stream"] == true,
        body,
    })
}

fn write_sse(
    stream: &mut TcpStream,
    response: &ScriptedResponse,
    mode: WriteMode,
) -> io::Result<()> {
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n")?;
    match mode {
        WriteMode::WholeBody => stream.write_all(response.body().as_bytes())?,
        WriteMode::FramePerWrite => {
            for frame in response.frames() {
                stream.write_all(frame.as_bytes())?;
                stream.flush()?;
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
