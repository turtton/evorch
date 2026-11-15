//! A fake renderer backend and helpers shared by the protocol tests. No CEF
//! and no sleeping: every wait is on a message the code under test sends.

#![allow(dead_code)]

use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use renderer_ipc::codec::{
    CONTROL_LIMITS, Kind, Message, encode_meta, read_message, write_message,
};
use renderer_ipc::*;
use serde_json::{Value, json};

pub const NEVER: Duration = Duration::from_secs(3600);

pub struct Fake {
    ctx: BackendContext,
    pub calls: Arc<Mutex<Vec<String>>>,
    entered: SyncSender<()>,
    release: Mutex<Receiver<()>>,
    shutdown_seen: SyncSender<()>,
}

/// What a test holds to steer and observe the fake.
pub struct Handles {
    pub calls: Arc<Mutex<Vec<String>>>,
    /// Receives one message each time a `gate` request reaches the backend.
    pub entered: Receiver<()>,
    /// Releases one waiting `gate` request.
    pub release: Sender<()>,
    /// Receives one message when the backend is told to shut down.
    pub shutdown_seen: Receiver<()>,
}

pub fn fake_init() -> (
    Handles,
    impl FnOnce(BackendContext) -> Result<Fake, RemoteError> + Send + 'static,
) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (entered_tx, entered_rx) = sync_channel(64);
    let (release_tx, release_rx) = channel();
    let (shutdown_tx, shutdown_rx) = sync_channel(4);
    let handles = Handles {
        calls: Arc::clone(&calls),
        entered: entered_rx,
        release: release_tx,
        shutdown_seen: shutdown_rx,
    };
    let init = move |ctx| {
        Ok(Fake {
            ctx,
            calls,
            entered: entered_tx,
            release: Mutex::new(release_rx),
            shutdown_seen: shutdown_tx,
        })
    };
    (handles, init)
}

impl Fake {
    fn note(&self, call: impl Into<String>) {
        self.calls.lock().unwrap().push(call.into());
    }
}

impl Backend for Fake {
    fn open_artifact(
        &self,
        open: &OpenArtifact,
        files: Vec<(String, String, Vec<u8>)>,
    ) -> Result<Viewport, RemoteError> {
        let names: Vec<_> = files
            .iter()
            .map(|(path, mime, bytes)| format!("{path}:{mime}:{}", bytes.len()))
            .collect();
        self.note(format!("open {} [{}]", open.tab, names.join(",")));
        Ok(open.viewport)
    }

    fn resize(&self, tab: TabId, viewport: Viewport) -> Result<(), RemoteError> {
        self.note(format!(
            "resize {tab} {}x{}",
            viewport.width, viewport.height
        ));
        Ok(())
    }

    fn set_streaming(&self, tab: TabId, enabled: bool) -> Result<(), RemoteError> {
        self.note(format!("streaming {tab} {enabled}"));
        Ok(())
    }

    fn capture(&self, tab: TabId, _timeout: Duration) -> Result<Vec<u8>, RemoteError> {
        self.note(format!("capture {tab}"));
        Ok(b"\x89PNG fake".to_vec())
    }

    fn cdp(
        &self,
        tab: TabId,
        method: &str,
        params: Value,
        _timeout: Duration,
    ) -> Result<Value, RemoteError> {
        self.note(format!("cdp {tab} {method}"));
        match params["op"].as_str().unwrap_or("echo") {
            "gate" => {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                Ok(json!({"gated": true}))
            }
            "large_frame" => {
                let width = params["width"].as_u64().unwrap() as u32;
                let height = params["height"].as_u64().unwrap() as u32;
                let fill = params["fill"].as_u64().unwrap() as u8;
                let pixels = vec![fill; width as usize * height as usize * 4];
                let published = self.ctx.frames.publish(tab, width, height, &pixels);
                Ok(json!({"published": published}))
            }
            "frame" => {
                let fill = params["fill"].as_u64().unwrap() as u8;
                let published = self.ctx.frames.publish(tab, 2, 2, &[fill; 16]);
                Ok(json!({"published": published}))
            }
            "crash" => {
                self.ctx.events.tab_gone(tab, TabGoneReason::Crashed);
                Ok(Value::Null)
            }
            "huge" => Ok(json!({"text": "x".repeat(2 * 1024 * 1024)})),
            _ => Ok(params),
        }
    }

    fn settle(&self, tab: TabId, _timeout: Duration) -> Result<(), RemoteError> {
        self.note(format!("settle {tab}"));
        Ok(())
    }

    fn take_notices(&self, tab: TabId) -> Result<Notices, RemoteError> {
        self.note(format!("notices {tab}"));
        Ok(Notices {
            blocked: vec!["request http://x (artifacts are offline)".into()],
            dialogs: vec![],
        })
    }

    fn close_tab(&self, tab: TabId) {
        self.note(format!("close {tab}"));
    }

    fn shutdown(&self) {
        self.note("shutdown");
        let _ = self.shutdown_seen.try_send(());
    }
}

pub struct Connected {
    pub client: RendererClient,
    pub helper: JoinHandle<ExitReason>,
    pub fake: Handles,
}

pub const GENERATION: RendererGeneration = RendererGeneration(7);

pub fn connect_fake() -> Connected {
    let (control_core, control_helper) = UnixStream::pair().unwrap();
    let (frames_core, frames_helper) = UnixStream::pair().unwrap();
    let (fake, init) = fake_init();
    let helper = serve_in_thread(control_helper, frames_helper, ServeConfig::default(), init);
    let (client, info) =
        RendererClient::connect(control_core, frames_core, ClientConfig::new(GENERATION)).unwrap();
    assert_eq!(info.protocol, PROTOCOL_VERSION);
    Connected {
        client,
        helper,
        fake,
    }
}

pub fn index_only() -> (OpenArtifact, Vec<u8>) {
    OpenArtifact::with_files(
        SessionId(1),
        TabId(1),
        Viewport {
            width: 64,
            height: 48,
        },
        30,
        5000,
        vec![(
            "index.html".into(),
            "text/html".into(),
            b"<p>hi</p>".to_vec(),
        )],
    )
}

pub fn open_tab(client: &RendererClient) {
    let (open, body) = index_only();
    client.open_artifact(open, &body).unwrap();
}

/// A hand-driven peer for tests that need to say something the real client or
/// server never would.
pub struct RawPeer {
    pub control: UnixStream,
    pub frames: UnixStream,
}

pub fn raw_request(
    stream: &UnixStream,
    generation: RendererGeneration,
    id: u64,
    request: Request,
    body: &[u8],
) {
    let frame = RequestFrame {
        generation,
        id: RequestId(id),
        request,
    };
    write_message(
        stream,
        Kind::Request,
        &encode_meta(&frame).unwrap(),
        body,
        CONTROL_LIMITS,
    )
    .unwrap();
}

pub fn raw_response(
    stream: &UnixStream,
    generation: RendererGeneration,
    id: u64,
    outcome: Outcome,
    body: &[u8],
) {
    let frame = ResponseFrame {
        generation,
        id: RequestId(id),
        outcome,
    };
    write_message(
        stream,
        Kind::Response,
        &encode_meta(&frame).unwrap(),
        body,
        CONTROL_LIMITS,
    )
    .unwrap();
}

pub fn read_control(stream: &UnixStream) -> Message {
    read_message(stream, CONTROL_LIMITS, Vec::new(), NEVER).unwrap()
}

pub fn read_request_frame(stream: &UnixStream) -> (RequestFrame, Vec<u8>) {
    let message = read_control(stream);
    assert_eq!(message.kind, Kind::Request);
    (serde_json::from_slice(&message.meta).unwrap(), message.body)
}

pub fn read_response_frame(stream: &UnixStream) -> (ResponseFrame, Vec<u8>) {
    let message = read_control(stream);
    assert_eq!(message.kind, Kind::Response);
    (serde_json::from_slice(&message.meta).unwrap(), message.body)
}

pub fn hello_ok() -> Outcome {
    Outcome::Ok {
        reply: Reply::Hello {
            protocol: PROTOCOL_VERSION,
            helper: "raw".into(),
        },
    }
}

/// Starts the real server on one end of two socket pairs and returns the
/// other ends for a test to drive by hand.
pub fn serve_raw(config: ServeConfig) -> (RawPeer, Handles, JoinHandle<ExitReason>) {
    let (control_core, control_helper) = UnixStream::pair().unwrap();
    let (frames_core, frames_helper) = UnixStream::pair().unwrap();
    let (fake, init) = fake_init();
    let helper = serve_in_thread(control_helper, frames_helper, config, init);
    (
        RawPeer {
            control: control_core,
            frames: frames_core,
        },
        fake,
        helper,
    )
}

pub fn raw_hello(peer: &RawPeer) {
    raw_request(
        &peer.control,
        GENERATION,
        1,
        Request::Hello {
            protocol: PROTOCOL_VERSION,
        },
        &[],
    );
    let (response, _) = read_response_frame(&peer.control);
    assert!(matches!(
        response.outcome,
        Outcome::Ok {
            reply: Reply::Hello { .. }
        }
    ));
}

/// A 16-byte message header with arbitrary announced lengths.
pub fn raw_header(kind: u8, meta_len: u32, body_len: u32) -> Vec<u8> {
    let mut head = b"ERIP".to_vec();
    head.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    head.push(kind);
    head.push(0);
    head.extend_from_slice(&meta_len.to_le_bytes());
    head.extend_from_slice(&body_len.to_le_bytes());
    head
}
