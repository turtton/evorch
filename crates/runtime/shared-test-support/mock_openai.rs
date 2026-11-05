use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use serde_json::Value;

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub authorization: Option<String>,
    pub body: Value,
}

pub struct RecordingMockOpenAi {
    base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl RecordingMockOpenAi {
    pub fn spawn(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("モックサーバを bind できる");
        let addr = listener.local_addr().expect("モックアドレスを取得できる");
        let script = Mutex::new(VecDeque::from(responses));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded_requests = Arc::clone(&requests);

        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let Some(response) = script
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop_front()
                else {
                    continue;
                };
                if let Some(request) = read_request(&mut stream) {
                    recorded_requests
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(request);
                }
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

        Self {
            base_url: format!("http://{addr}/v1"),
            requests,
        }
    }

    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

/// リクエストヘッダと Content-Length 分の body を読み切る。
fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(header_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buf[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                })
                .unwrap_or(0);
            if buf.len() >= header_end + 4 + content_length {
                let authorization = headers.lines().find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("authorization")
                            .then(|| value.trim().to_string())
                    })
                });
                let body_start = header_end + 4;
                let body = serde_json::from_slice(&buf[body_start..body_start + content_length])
                    .unwrap_or(Value::Null);
                return Some(RecordedRequest {
                    authorization,
                    body,
                });
            }
        }
        let read = stream.read(&mut chunk).expect("モックリクエストを読める");
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
}
