//! In-memory transport for tests, with a scriptable fake core.
//!
//! The pipe carries raw bytes, not frames, so tests can deliver a frame in
//! pieces, several frames at once, or garbage, and exercise the same decoder
//! the Unix socket transport uses. Nothing here sleeps or times out: every
//! wait is for a message the other side actually sends.

use crate::error::TransportError;
use crate::transport::{FrameSink, FrameSource, Transport, framing_error};
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use workbench_protocol::envelope::{
    ReplyEnvelope, ReplyResult, RequestEnvelope, ServerFrame, decode_request,
};
use workbench_protocol::events::EventEnvelope;
use workbench_protocol::framing::{FrameDecoder, decode_payload, encode_frame};
use workbench_protocol::handshake::{ClientHello, HandshakeReply, ServerHello, WireLimits};
use workbench_protocol::ids::RequestId;
use workbench_protocol::limits::DEFAULT_FRAME_BYTES;

/// The client end of the in-memory pipe.
#[derive(Debug)]
pub struct FakeTransport {
    to_server: mpsc::UnboundedSender<Vec<u8>>,
    from_server: mpsc::UnboundedReceiver<Vec<u8>>,
    gate: watch::Receiver<bool>,
    blocked: mpsc::UnboundedSender<()>,
}

#[derive(Debug)]
pub struct FakeSink {
    to_server: mpsc::UnboundedSender<Vec<u8>>,
    gate: watch::Receiver<bool>,
    blocked: mpsc::UnboundedSender<()>,
}

#[derive(Debug)]
pub struct FakeSource {
    from_server: mpsc::UnboundedReceiver<Vec<u8>>,
    decoder: FrameDecoder,
}

impl FakeTransport {
    /// A connected pair: the transport for the client under test, and the fake core.
    pub fn pair() -> (FakeTransport, FakeServer) {
        let (to_server, from_client) = mpsc::unbounded_channel();
        let (to_client, from_server) = mpsc::unbounded_channel();
        let (gate_tx, gate_rx) = watch::channel(true);
        let (blocked_tx, blocked_rx) = mpsc::unbounded_channel();
        (
            FakeTransport {
                to_server,
                from_server,
                gate: gate_rx,
                blocked: blocked_tx,
            },
            FakeServer {
                from_client,
                to_client: Some(to_client),
                decoder: FrameDecoder::new(DEFAULT_FRAME_BYTES),
                gate: gate_tx,
                blocked: blocked_rx,
            },
        )
    }
}

impl Transport for FakeTransport {
    type Sink = FakeSink;
    type Source = FakeSource;

    fn split(self) -> (FakeSink, FakeSource) {
        (
            FakeSink {
                to_server: self.to_server,
                gate: self.gate,
                blocked: self.blocked,
            },
            FakeSource {
                from_server: self.from_server,
                decoder: FrameDecoder::new(DEFAULT_FRAME_BYTES),
            },
        )
    }
}

impl FrameSink for FakeSink {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        // While the fake core holds the write gate shut, the write is "in
        // progress": the client has handed the frame to the transport but the
        // core has not received it.
        if !*self.gate.borrow() {
            let _ = self.blocked.send(());
            self.gate
                .wait_for(|open| *open)
                .await
                .map_err(|_| TransportError::Disconnected)?;
        }
        self.to_server
            .send(frame.to_vec())
            .map_err(|_| TransportError::Disconnected)
    }
}

impl FrameSource for FakeSource {
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        loop {
            if let Some(payload) = self.decoder.next_payload().map_err(framing_error)? {
                return Ok(Some(payload));
            }
            match self.from_server.recv().await {
                Some(bytes) => self.decoder.push(&bytes),
                None if self.decoder.buffered() == 0 => return Ok(None),
                None => {
                    return Err(TransportError::ProtocolViolation(
                        "connection closed in the middle of a frame".to_owned(),
                    ));
                }
            }
        }
    }

    fn set_max_frame_bytes(&mut self, max_frame_bytes: u32) {
        self.decoder.set_max_frame_bytes(max_frame_bytes);
    }
}

/// A scriptable core. Dropping it, or calling [`FakeServer::close`], closes the connection.
#[derive(Debug)]
pub struct FakeServer {
    from_client: mpsc::UnboundedReceiver<Vec<u8>>,
    to_client: Option<mpsc::UnboundedSender<Vec<u8>>>,
    decoder: FrameDecoder,
    gate: watch::Sender<bool>,
    blocked: mpsc::UnboundedReceiver<()>,
}

impl FakeServer {
    /// Next payload the client wrote, or `None` once the client closed its end.
    pub async fn recv_payload(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.decoder.next_payload() {
                Ok(Some(payload)) => return Some(payload),
                Ok(None) => {}
                Err(error) => panic!("client sent a bad frame: {error}"),
            }
            let bytes = self.from_client.recv().await?;
            self.decoder.push(&bytes);
        }
    }

    /// The client's first frame.
    pub async fn recv_hello(&mut self) -> ClientHello {
        let payload = self.recv_payload().await.expect("client hello");
        decode_payload(&payload).expect("a valid client hello")
    }

    /// The next request, decoded and validated the way a real core would.
    pub async fn recv_request(&mut self) -> Option<RequestEnvelope> {
        let payload = self.recv_payload().await?;
        Some(
            decode_request(&payload, &WireLimits::default())
                .unwrap_or_else(|rejection| panic!("client sent a bad request: {rejection:?}")),
        )
    }

    /// Answer the handshake with `hello`.
    pub fn accept(&self, hello: &ServerHello) {
        self.send(&HandshakeReply::Accepted(hello.clone()));
    }

    /// Receive the client's hello, then accept it.
    pub async fn handshake(&mut self, hello: &ServerHello) -> ClientHello {
        let client = self.recv_hello().await;
        self.accept(hello);
        client
    }

    /// Send any value as one frame.
    pub fn send<T: Serialize>(&self, value: &T) {
        self.send_raw(encode_frame(value, u32::MAX).expect("encodable"));
    }

    /// Send bytes exactly as given: a partial frame, several frames, garbage.
    pub fn send_raw(&self, bytes: Vec<u8>) {
        if let Some(to_client) = &self.to_client {
            // A client that already hung up is not an error for a script.
            let _ = to_client.send(bytes);
        }
    }

    pub fn reply(&self, request_id: &RequestId, result: ReplyResult) {
        self.send(&ServerFrame::Reply(ReplyEnvelope {
            request_id: request_id.clone(),
            result,
        }));
    }

    pub fn event(&self, event: EventEnvelope) {
        self.send(&ServerFrame::Event(Box::new(event)));
    }

    /// Stop the client's writes at the transport: the next frame it writes is
    /// "in progress" and never reaches this server until [`FakeServer::open_writes`].
    pub fn hold_writes(&self) {
        self.gate.send_replace(false);
    }

    pub fn open_writes(&self) {
        self.gate.send_replace(true);
    }

    /// Wait until the client has started a write that [`FakeServer::hold_writes`] is holding.
    pub async fn write_blocked(&mut self) {
        self.blocked.recv().await.expect("client is alive");
    }

    /// Close the connection from the core's side.
    pub fn close(&mut self) {
        self.to_client = None;
    }
}
