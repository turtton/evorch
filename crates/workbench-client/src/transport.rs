//! How frames reach the core.
//!
//! A transport is split into a sink and a source so one task can write while
//! another reads; neither blocks the other. Both deal in whole frames: the
//! sink receives an already encoded frame (length prefix included) and the
//! source returns payloads with the prefix removed and the size checked.

use crate::error::TransportError;
use std::future::Future;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use workbench_protocol::framing::{FrameDecoder, FramingError};
use workbench_protocol::limits::DEFAULT_FRAME_BYTES;

/// Writes encoded frames. Called from exactly one task.
pub trait FrameSink: Send + 'static {
    /// Write one complete frame. A failure means the connection is unusable.
    fn send_frame(
        &mut self,
        frame: &[u8],
    ) -> impl Future<Output = Result<(), TransportError>> + Send;
}

/// Reads frame payloads. Called from exactly one task.
pub trait FrameSource: Send + 'static {
    /// The next payload, or `None` when the peer closed cleanly between frames.
    fn recv_frame(
        &mut self,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, TransportError>> + Send;

    /// Apply the limit negotiated by the handshake to later frames.
    fn set_max_frame_bytes(&mut self, max_frame_bytes: u32);
}

/// A bidirectional frame channel to a core.
pub trait Transport: Send + 'static {
    type Sink: FrameSink;
    type Source: FrameSource;

    fn split(self) -> (Self::Sink, Self::Source);
}

pub(crate) fn framing_error(error: FramingError) -> TransportError {
    match error {
        FramingError::FrameTooLarge { len, max } => TransportError::FrameTooLarge { len, max },
        other => TransportError::ProtocolViolation(other.to_string()),
    }
}

/// Frames over a Unix domain socket (desktop GUI and CLI to the core).
#[derive(Debug)]
pub struct UnixTransport {
    stream: UnixStream,
}

impl UnixTransport {
    pub fn new(stream: UnixStream) -> Self {
        Self { stream }
    }

    /// Connect to the core's socket. Whether a core is actually behind it is
    /// decided by the handshake, not by the socket file existing.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, TransportError> {
        UnixStream::connect(path)
            .await
            .map(Self::new)
            .map_err(|_| TransportError::Disconnected)
    }
}

#[derive(Debug)]
pub struct UnixSink {
    write: OwnedWriteHalf,
}

#[derive(Debug)]
pub struct UnixSource {
    read: OwnedReadHalf,
    decoder: FrameDecoder,
}

impl Transport for UnixTransport {
    type Sink = UnixSink;
    type Source = UnixSource;

    fn split(self) -> (UnixSink, UnixSource) {
        let (read, write) = self.stream.into_split();
        (
            UnixSink { write },
            UnixSource {
                read,
                decoder: FrameDecoder::new(DEFAULT_FRAME_BYTES),
            },
        )
    }
}

impl FrameSink for UnixSink {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        self.write
            .write_all(frame)
            .await
            .map_err(|_| TransportError::Disconnected)?;
        self.write
            .flush()
            .await
            .map_err(|_| TransportError::Disconnected)
    }
}

impl FrameSource for UnixSource {
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            if let Some(payload) = self.decoder.next_payload().map_err(framing_error)? {
                return Ok(Some(payload));
            }
            let read = self
                .read
                .read(&mut chunk)
                .await
                .map_err(|_| TransportError::Disconnected)?;
            if read == 0 {
                return if self.decoder.buffered() == 0 {
                    Ok(None)
                } else {
                    Err(TransportError::ProtocolViolation(
                        "connection closed in the middle of a frame".to_owned(),
                    ))
                };
            }
            self.decoder.push(&chunk[..read]);
        }
    }

    fn set_max_frame_bytes(&mut self, max_frame_bytes: u32) {
        self.decoder.set_max_frame_bytes(max_frame_bytes);
    }
}
