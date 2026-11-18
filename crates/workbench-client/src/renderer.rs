//! Renderer data plane client (ADR 0045 stage 16a-2, F3).
//!
//! The control connection ([`crate::WorkbenchClient`]) carries only references
//! and receipts. The bytes of a renderer frame, a Capture PNG or an image
//! artifact travel on a dedicated connection that this module opens, binds to
//! the control connection and reads with bounded buffers.
//!
//! ```text
//! DataChannel::connect(path)        data hello, the core offers a channel id
//!   .bind_frames(&control, ..)      BindRendererData on the control connection
//!   -> FrameStream::next_frame()    one raw BGRA frame at a time, latest wins
//! DataChannel::connect(path)
//!   .bind_artifact(&control, ..)    BindArtifactContent
//!   -> BlobStream::next_chunk()     <= 64 KiB at a time, never the whole image
//! ```
//!
//! The data connection carries no identity: only the control command that
//! binds it is authorized, so a channel id nobody binds is worth nothing and
//! one bound by another client is not usable. The core enforces the time
//! limits (binding window, no-progress window) and reports them as an error
//! record; this module starts no timer of its own. To cancel, drop the stream:
//! that closes the connection and the core releases everything it held for it.
//!
//! Every length is checked against the protocol limits before anything is
//! allocated for it, and every transfer is checked chunk by chunk (one id,
//! contiguous offsets, an exact end). Any violation ends the stream; a
//! transfer is never resumed.

use crate::{ClientError, WorkbenchClient};
use std::path::Path;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use workbench_protocol::commands::Command;
use workbench_protocol::namespaces::artifacts::{ArtifactRef, ArtifactsCommand};
use workbench_protocol::namespaces::browser::data::{
    AckMeta, BlobStart, ChannelOffer, ChunkMeta, DataError, EndMeta, ErrorCode, ErrorMeta,
    FrameStart, Kind, PREFIX_LEN, Prefix, SequenceGuard, Transfer, encode_hello,
};
use workbench_protocol::namespaces::browser::{BrowserCommand, RendererDataSource, RendererId};

/// Why reading from a data connection failed.
#[derive(Debug, thiserror::Error)]
pub enum DataClientError {
    #[error("data connection failed: {0}")]
    Io(#[from] std::io::Error),
    /// The core sent something this protocol does not allow.
    #[error("data protocol violation: {0}")]
    Protocol(#[from] DataError),
    /// The core ended the connection with a reason.
    #[error("the core ended the data connection: {0:?}")]
    Remote(ErrorCode),
    /// The binding command failed on the control connection.
    #[error("binding the data connection failed: {0}")]
    Bind(#[from] ClientError),
    /// The connection ended inside a frame or a blob.
    #[error("the data connection ended inside a transfer")]
    Truncated,
    /// A blob is larger than the caller allows.
    #[error("the blob has {len} bytes, more than the {max} allowed")]
    TooLarge { len: u64, max: u64 },
}

/// A record read from the stream: its kind, its fixed-size metadata and (for a
/// chunk) its body, which was read into the caller's buffer.
struct Record {
    kind: Kind,
    meta: Vec<u8>,
    body_len: usize,
}

/// Reads one record. `Ok(None)` is an end of the stream between records.
async fn read_record<R: AsyncRead + Unpin>(
    read: &mut R,
    body: &mut Vec<u8>,
    append: bool,
) -> Result<Option<Record>, DataClientError> {
    let mut prefix = [0u8; PREFIX_LEN];
    if read.read(&mut prefix[..1]).await? == 0 {
        return Ok(None);
    }
    read.read_exact(&mut prefix[1..]).await.map_err(truncated)?;
    let prefix = Prefix::parse(&prefix)?;
    let mut meta = vec![0u8; prefix.meta_len];
    read.read_exact(&mut meta).await.map_err(truncated)?;
    if !append {
        body.clear();
    }
    read_body(read, body, prefix.body_len).await?;
    if prefix.kind == Kind::Error {
        return Err(DataClientError::Remote(ErrorMeta::decode(&meta)?.code));
    }
    Ok(Some(Record {
        kind: prefix.kind,
        meta,
        body_len: prefix.body_len,
    }))
}

/// Appends exactly `len` bytes to `body`, read straight into its spare capacity
/// (no zero fill, no intermediate buffer).
async fn read_body<R: AsyncRead + Unpin>(
    read: &mut R,
    body: &mut Vec<u8>,
    len: usize,
) -> Result<(), DataClientError> {
    body.reserve(len);
    let mut limited = (&mut *read).take(len as u64);
    while limited.limit() > 0 {
        if limited.read_buf(body).await? == 0 {
            return Err(DataClientError::Truncated);
        }
    }
    Ok(())
}

fn truncated(error: std::io::Error) -> DataClientError {
    if error.kind() == std::io::ErrorKind::UnexpectedEof {
        DataClientError::Truncated
    } else {
        DataClientError::Io(error)
    }
}

async fn expect_record<R: AsyncRead + Unpin>(
    read: &mut R,
    body: &mut Vec<u8>,
    kind: Kind,
) -> Result<Vec<u8>, DataClientError> {
    match read_record(read, body, false).await? {
        Some(record) if record.kind == kind => Ok(record.meta),
        Some(_) => Err(DataError::OutOfOrder("unexpected record").into()),
        None => Err(DataClientError::Truncated),
    }
}

async fn send_ack<W: AsyncWrite + Unpin>(write: &mut W, id: u64) -> Result<(), DataClientError> {
    write.write_all(&AckMeta { id }.encode()).await?;
    Ok(())
}

/// A data connection that the core offered a channel on and nothing has bound.
pub struct DataChannel {
    stream: UnixStream,
    channel: RendererId,
    bind_window_ms: u32,
}

impl DataChannel {
    /// Opens a data connection to the core's socket and reads the channel offer.
    ///
    /// # Errors
    /// The socket is unavailable, or the core did not offer a channel (it is
    /// at its limit of unbound connections, or it has no renderer).
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, DataClientError> {
        let mut stream = UnixStream::connect(path).await?;
        stream.write_all(&encode_hello()).await?;
        let mut body = Vec::new();
        let meta = expect_record(&mut stream, &mut body, Kind::Channel).await?;
        let offer = ChannelOffer::decode(&meta)?;
        Ok(Self {
            stream,
            channel: offer.channel,
            bind_window_ms: offer.bind_window_ms,
        })
    }

    /// The id to send in the binding command.
    pub fn channel_id(&self) -> &RendererId {
        &self.channel
    }

    /// How long the core keeps the channel open for binding.
    pub fn bind_window_ms(&self) -> u32 {
        self.bind_window_ms
    }

    async fn bind(
        self,
        control: &WorkbenchClient,
        command: Command,
    ) -> Result<UnixStream, DataClientError> {
        control.command(command).await?;
        Ok(self.stream)
    }

    /// Binds the connection to the frames of a subscription of this caller.
    ///
    /// # Errors
    /// The core refused the binding (the connection then ends by itself).
    pub async fn bind_frames(
        self,
        control: &WorkbenchClient,
        subscription: RendererId,
        token: RendererId,
    ) -> Result<FrameStream, DataClientError> {
        let command = Command::Browser(BrowserCommand::BindRendererData {
            channel_id: self.channel.clone(),
            source: RendererDataSource::Frames {
                subscription,
                token,
            },
        });
        let stream = self.bind(control, command).await?;
        Ok(FrameStream {
            stream,
            body: Vec::new(),
            order: SequenceGuard::default(),
        })
    }

    /// Binds the connection to the PNG of a Capture operation of this caller.
    ///
    /// # Errors
    /// The core refused the binding.
    pub async fn bind_capture(
        self,
        control: &WorkbenchClient,
        operation_id: RendererId,
        capture_id: RendererId,
    ) -> Result<BlobStream, DataClientError> {
        let command = Command::Browser(BrowserCommand::BindRendererData {
            channel_id: self.channel.clone(),
            source: RendererDataSource::Capture {
                operation_id,
                capture_id,
            },
        });
        let stream = self.bind(control, command).await?;
        BlobStream::open(stream).await
    }

    /// Binds the connection to the original bytes of an image artifact the
    /// caller may read.
    ///
    /// # Errors
    /// The core refused the binding.
    pub async fn bind_artifact(
        self,
        control: &WorkbenchClient,
        artifact: ArtifactRef,
    ) -> Result<BlobStream, DataClientError> {
        let command = Command::Artifacts(ArtifactsCommand::BindArtifactContent {
            channel_id: self.channel.clone(),
            artifact,
        });
        let stream = self.bind(control, command).await?;
        BlobStream::open(stream).await
    }
}

/// The frames of one subscription. One frame is in flight at a time and the
/// next is the latest one the core had when this receipt was read, so a slow
/// reader skips frames and never queues them.
pub struct FrameStream {
    stream: UnixStream,
    body: Vec<u8>,
    order: SequenceGuard,
}

impl FrameStream {
    /// Reads the next frame into `bytes` (replacing its contents) and returns
    /// its description. `Ok(None)`: the core ended the connection cleanly.
    ///
    /// The frame is acknowledged once whole. `bytes` is reserved only after
    /// the description passed every size check.
    ///
    /// # Errors
    /// The connection failed, ended inside a frame or broke the protocol.
    pub async fn next_frame_into(
        &mut self,
        bytes: &mut Vec<u8>,
    ) -> Result<Option<FrameStart>, DataClientError> {
        let Some(record) = read_record(&mut self.stream, &mut self.body, false).await? else {
            return Ok(None);
        };
        if record.kind != Kind::FrameStart {
            return Err(DataError::OutOfOrder("expected a frame").into());
        }
        let start = FrameStart::decode(&record.meta)?;
        start.validate()?;
        self.order.accept(start.sequence)?;
        bytes.clear();
        bytes.reserve_exact(start.total_bytes as usize);
        let mut transfer = Transfer::new(start.sequence, start.total_bytes);
        loop {
            let record = read_record(&mut self.stream, bytes, true)
                .await?
                .ok_or(DataClientError::Truncated)?;
            match record.kind {
                Kind::Chunk => {
                    transfer.chunk(ChunkMeta::decode(&record.meta)?, record.body_len)?;
                }
                Kind::End => {
                    transfer.end(EndMeta::decode(&record.meta)?)?;
                    break;
                }
                _ => return Err(DataError::OutOfOrder("inside a frame").into()),
            }
        }
        send_ack(&mut self.stream, start.sequence).await?;
        Ok(Some(start))
    }

    /// Like [`next_frame_into`](Self::next_frame_into) with a new buffer.
    ///
    /// # Errors
    /// See [`next_frame_into`](Self::next_frame_into).
    pub async fn next_frame(&mut self) -> Result<Option<Frame>, DataClientError> {
        let mut bytes = Vec::new();
        Ok(self
            .next_frame_into(&mut bytes)
            .await?
            .map(|start| Frame { start, bgra: bytes }))
    }
}

/// One raw frame: `width * height` pixels of BGRA8 in `stride`-byte rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub start: FrameStart,
    pub bgra: Vec<u8>,
}

/// One image: the original bytes of an artifact or a Capture PNG.
pub struct BlobStream {
    stream: UnixStream,
    body: Vec<u8>,
    start: BlobStart,
    transfer: Transfer,
    done: bool,
}

impl BlobStream {
    async fn open(mut stream: UnixStream) -> Result<Self, DataClientError> {
        let mut body = Vec::new();
        let meta = expect_record(&mut stream, &mut body, Kind::BlobStart).await?;
        let start = BlobStart::decode(&meta)?;
        let transfer = Transfer::new(start.transfer_id, start.total_bytes);
        Ok(Self {
            stream,
            body,
            start,
            transfer,
            done: false,
        })
    }

    /// The description of the blob: source, media type, size and (for an
    /// artifact) the SHA-256 digest of the stored bytes.
    pub fn start(&self) -> &BlobStart {
        &self.start
    }

    /// The next piece (at most 64 KiB). `Ok(None)` once the whole blob has
    /// arrived and been acknowledged.
    ///
    /// # Errors
    /// The connection failed, ended inside the blob or broke the protocol.
    pub async fn next_chunk(&mut self) -> Result<Option<&[u8]>, DataClientError> {
        if self.done {
            return Ok(None);
        }
        let record = read_record(&mut self.stream, &mut self.body, false)
            .await?
            .ok_or(DataClientError::Truncated)?;
        match record.kind {
            Kind::Chunk => {
                self.transfer
                    .chunk(ChunkMeta::decode(&record.meta)?, self.body.len())?;
                Ok(Some(&self.body))
            }
            Kind::End => {
                self.transfer.end(EndMeta::decode(&record.meta)?)?;
                send_ack(&mut self.stream, self.start.transfer_id).await?;
                self.done = true;
                Ok(None)
            }
            _ => Err(DataError::OutOfOrder("inside a blob").into()),
        }
    }

    /// Reads the whole blob, refusing one larger than `max` before reading it.
    ///
    /// # Errors
    /// The blob is larger than `max`, or reading failed.
    pub async fn read_to_end(mut self, max: u64) -> Result<Vec<u8>, DataClientError> {
        let len = self.start.total_bytes;
        if len > max {
            return Err(DataClientError::TooLarge { len, max });
        }
        let mut out = Vec::with_capacity(len as usize);
        while let Some(chunk) = self.next_chunk().await? {
            out.extend_from_slice(chunk);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
