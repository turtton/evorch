//! The renderer data plane's binary records (16a-2 F3).
//!
//! Pixels and image bytes never travel in the JSON control protocol. A client
//! opens a *dedicated* connection to the same local socket, and that connection
//! speaks only the records defined here. Nothing on it is trusted until the
//! client's **control** connection binds it (`BindRendererData`,
//! `BindArtifactContent`): the control connection is where provenance is known.
//!
//! ```text
//! first message (the only length-prefixed one, so the core can tell it from a
//! control `ClientHello` before it parses anything):
//!     u32 BE 16, then one record prefix with kind = Hello and no meta/body
//! record:
//!     magic "EVRD", version u16, kind u16, meta_len u32, body_len u32   (16 bytes, LE)
//!     meta (fixed layout per kind, little-endian, at most 1 KiB)
//!     body (Chunk only, at most 64 KiB)
//! ```
//!
//! Every length is checked against its limit by [`Prefix::parse`] before the
//! reader allocates anything, and every meta has a fixed size, so a hostile or
//! confused peer cannot make the receiver reserve memory by lying. A frame or a
//! blob is `Start`, a run of in-order `Chunk`s and `End`; [`Transfer`] enforces
//! the order, offsets and totals, so a duplicated, missing, reordered or
//! mixed-up chunk is an error, and a connection that sees one is closed (it is
//! never resumed into another frame).
//!
//! This module is pure: it parses and builds bytes and holds the order
//! checker. Sockets, budgets and authorization live in the core, the host and
//! the client.

use super::{MAX_RENDERER_TABS, RendererId, RendererViewport};

pub const MAGIC: [u8; 4] = *b"EVRD";
pub const VERSION: u16 = 1;
pub const PREFIX_LEN: usize = 16;
/// Largest meta of any record.
pub const MAX_META_BYTES: usize = 1024;
/// Largest `Chunk` body. A write or read unit, not a limit on a frame.
pub const CHUNK_BYTES: usize = 64 * 1024;
/// A full-size raw frame: 3840 x 2160 BGRA.
pub const MAX_FRAME_BYTES: u64 = 3840 * 2160 * 4;
/// Stored image artifacts (the same bound as `runtime::artifacts::MAX_IMAGE_BYTES`).
pub const MAX_IMAGE_BLOB_BYTES: u64 = 8 * 1024 * 1024;
/// A Capture PNG (the renderer helper's control body bound).
pub const MAX_CAPTURE_BLOB_BYTES: u64 = 16 * 1024 * 1024;
/// Frame subscriptions held at once (pending, active and closing).
pub const MAX_FRAME_SUBSCRIPTIONS: usize = 8;
/// Frame transfers one tab's data may be fanned out to is bounded by the
/// subscription limit; the tab limit is the browser namespace's.
pub const MAX_FRAME_TABS: usize = MAX_RENDERER_TABS;
/// Data connections that connected but are not bound yet.
pub const MAX_UNBOUND_CONNECTIONS: usize = 8;
/// Blob transfers (artifact images and Capture PNGs) at once.
pub const MAX_BLOB_TRANSFERS: usize = 4;
/// How long a connection may wait for its binding.
pub const BIND_WINDOW_MS: u32 = 30_000;
/// How long a transfer may make no progress before its connection is closed.
pub const NO_PROGRESS_MS: u32 = 5_000;

const FORMAT_BGRA8: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DataError {
    #[error("not a renderer data record")]
    BadMagic,
    #[error("unsupported data protocol version {0}")]
    UnsupportedVersion(u16),
    #[error("unknown record kind {0}")]
    UnknownKind(u16),
    #[error("{field} of {len} bytes exceeds the limit of {max}")]
    TooLarge {
        field: &'static str,
        len: u64,
        max: u64,
    },
    #[error("{0} is not allowed for this record")]
    UnexpectedLength(&'static str),
    #[error("malformed {0}")]
    Malformed(&'static str),
    #[error("record out of order: {0}")]
    OutOfOrder(&'static str),
}

/// What a record is. The numbers are the wire values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    /// Client to core, once: opens a data connection.
    Hello = 1,
    /// Core to client: the one-time channel the control connection must bind.
    Channel = 2,
    FrameStart = 3,
    BlobStart = 4,
    Chunk = 5,
    End = 6,
    /// Client to core: the whole frame or blob arrived.
    Ack = 7,
    /// Core to client, last: the connection ends after it.
    Error = 8,
}

impl Kind {
    pub fn from_wire(value: u16) -> Option<Self> {
        Some(match value {
            1 => Self::Hello,
            2 => Self::Channel,
            3 => Self::FrameStart,
            4 => Self::BlobStart,
            5 => Self::Chunk,
            6 => Self::End,
            7 => Self::Ack,
            8 => Self::Error,
            _ => return None,
        })
    }

    /// The meta length of this kind. Every kind has a fixed layout.
    pub const fn meta_len(self) -> usize {
        match self {
            Self::Hello => 0,
            Self::Channel => ChannelOffer::LEN,
            Self::FrameStart => FrameStart::LEN,
            Self::BlobStart => BlobStart::LEN,
            Self::Chunk => ChunkMeta::LEN,
            Self::End => EndMeta::LEN,
            Self::Ack => AckMeta::LEN,
            Self::Error => ErrorMeta::LEN,
        }
    }

    /// Whether a record of this kind may carry a body.
    pub const fn has_body(self) -> bool {
        matches!(self, Self::Chunk)
    }
}

/// A parsed record prefix, already checked against every limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefix {
    pub kind: Kind,
    pub meta_len: usize,
    pub body_len: usize,
}

impl Prefix {
    /// Checks magic, version, kind and both lengths. Call this before
    /// allocating anything for the rest of the record.
    pub fn parse(bytes: &[u8; PREFIX_LEN]) -> Result<Self, DataError> {
        if bytes[..4] != MAGIC {
            return Err(DataError::BadMagic);
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != VERSION {
            return Err(DataError::UnsupportedVersion(version));
        }
        let wire = u16::from_le_bytes([bytes[6], bytes[7]]);
        let kind = Kind::from_wire(wire).ok_or(DataError::UnknownKind(wire))?;
        let meta_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        let body_len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
        if meta_len > MAX_META_BYTES {
            return Err(DataError::TooLarge {
                field: "meta",
                len: meta_len as u64,
                max: MAX_META_BYTES as u64,
            });
        }
        if body_len > CHUNK_BYTES {
            return Err(DataError::TooLarge {
                field: "body",
                len: body_len as u64,
                max: CHUNK_BYTES as u64,
            });
        }
        if meta_len != kind.meta_len() {
            return Err(DataError::UnexpectedLength("meta"));
        }
        if body_len != 0 && !kind.has_body() {
            return Err(DataError::UnexpectedLength("body"));
        }
        Ok(Self {
            kind,
            meta_len,
            body_len,
        })
    }
}

fn prefix_bytes(kind: Kind, meta_len: usize, body_len: usize) -> [u8; PREFIX_LEN] {
    let mut out = [0u8; PREFIX_LEN];
    out[..4].copy_from_slice(&MAGIC);
    out[4..6].copy_from_slice(&VERSION.to_le_bytes());
    out[6..8].copy_from_slice(&(kind as u16).to_le_bytes());
    out[8..12].copy_from_slice(&(meta_len as u32).to_le_bytes());
    out[12..16].copy_from_slice(&(body_len as u32).to_le_bytes());
    out
}

/// The client's first message: a length-prefixed (u32 BE) record prefix. The
/// length prefix is the framing the control protocol uses, so the core reads
/// the first message of every connection the same way and then tells the two
/// apart with [`is_hello`].
pub fn encode_hello() -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + PREFIX_LEN);
    out.extend_from_slice(&(PREFIX_LEN as u32).to_be_bytes());
    out.extend_from_slice(&prefix_bytes(Kind::Hello, 0, 0));
    out
}

/// Whether the payload of a connection's first frame is a data hello. A control
/// `ClientHello` is JSON and never starts with the magic.
pub fn is_hello(first_frame: &[u8]) -> bool {
    first_frame.len() == PREFIX_LEN
        && first_frame[..4] == MAGIC
        && <&[u8; PREFIX_LEN]>::try_from(first_frame)
            .ok()
            .is_some_and(|p| matches!(Prefix::parse(p), Ok(prefix) if prefix.kind == Kind::Hello))
}

/// Serializes one record. `meta` must have the kind's exact length and `body`
/// must be empty unless the kind carries one.
///
/// # Panics
/// On a meta or body that breaks the kind's layout. These are programming
/// errors of the sender; use [`Prefix::parse`] for untrusted bytes.
pub fn encode_record(kind: Kind, meta: &[u8], body: &[u8]) -> Vec<u8> {
    assert_eq!(meta.len(), kind.meta_len(), "meta layout of {kind:?}");
    assert!(body.len() <= CHUNK_BYTES, "chunk body of {kind:?}");
    assert!(body.is_empty() || kind.has_body(), "body of {kind:?}");
    let mut out = Vec::with_capacity(PREFIX_LEN + meta.len() + body.len());
    out.extend_from_slice(&prefix_bytes(kind, meta.len(), body.len()));
    out.extend_from_slice(meta);
    out.extend_from_slice(body);
    out
}

fn raw_id(id: &RendererId) -> [u8; 16] {
    // A `RendererId` is 32 lowercase hex digits, checked on construction.
    let mut out = [0u8; 16];
    for (byte, pair) in out.iter_mut().zip(id.as_str().as_bytes().chunks(2)) {
        *byte = (hex_value(pair[0]) << 4) | hex_value(pair[1]);
    }
    out
}

fn id_from_raw(raw: [u8; 16]) -> RendererId {
    let mut text = String::with_capacity(32);
    for byte in raw {
        text.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
        text.push(char::from(b"0123456789abcdef"[usize::from(byte & 15)]));
    }
    RendererId::new(text).expect("32 lowercase hex digits")
}

fn hex_value(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        b'A'..=b'F' => digit - b'A' + 10,
        _ => 0,
    }
}

fn digest_raw(hex: &str) -> Result<[u8; 32], DataError> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err(DataError::Malformed("digest"));
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(bytes.chunks(2)) {
        *byte = (hex_value(pair[0]) << 4) | hex_value(pair[1]);
    }
    Ok(out)
}

struct Reader<'a>(&'a [u8]);
impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let (head, rest) = self.0.split_at(N);
        self.0 = rest;
        head.try_into().expect("split at N")
    }
    fn u8(&mut self) -> u8 {
        self.take::<1>()[0]
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take())
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take())
    }
}

fn exact(meta: &[u8], len: usize, what: &'static str) -> Result<(), DataError> {
    if meta.len() == len {
        Ok(())
    } else {
        Err(DataError::Malformed(what))
    }
}

/// Core to client, after the hello: the channel the control connection binds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelOffer {
    pub channel: RendererId,
    /// How long the binding stays possible, in milliseconds.
    pub bind_window_ms: u32,
}
impl ChannelOffer {
    pub const LEN: usize = 16 + 4;
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        out.extend_from_slice(&raw_id(&self.channel));
        out.extend_from_slice(&self.bind_window_ms.to_le_bytes());
        encode_record(Kind::Channel, &out, &[])
    }
    pub fn decode(meta: &[u8]) -> Result<Self, DataError> {
        exact(meta, Self::LEN, "channel offer")?;
        let mut r = Reader(meta);
        Ok(Self {
            channel: id_from_raw(r.take()),
            bind_window_ms: r.u32(),
        })
    }
}

/// A raw frame of one tab. `stride` is `width * 4` and the chunks cover
/// `total_bytes` exactly; both are checked by [`FrameStart::validate`] before
/// the receiver allocates the frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameStart {
    pub core_epoch: u64,
    pub generation: u64,
    pub tab: RendererId,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub total_bytes: u64,
}
impl FrameStart {
    pub const LEN: usize = 8 + 8 + 16 + 8 + 4 + 4 + 4 + 1 + 3 + 8;
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        out.extend_from_slice(&self.core_epoch.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&raw_id(&self.tab));
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.stride.to_le_bytes());
        out.push(FORMAT_BGRA8);
        out.extend_from_slice(&[0; 3]);
        out.extend_from_slice(&self.total_bytes.to_le_bytes());
        encode_record(Kind::FrameStart, &out, &[])
    }
    /// Parses the layout. Call [`validate`](Self::validate) before allocating.
    pub fn decode(meta: &[u8]) -> Result<Self, DataError> {
        exact(meta, Self::LEN, "frame start")?;
        let mut r = Reader(meta);
        let core_epoch = r.u64();
        let generation = r.u64();
        let tab = id_from_raw(r.take());
        let sequence = r.u64();
        let (width, height, stride) = (r.u32(), r.u32(), r.u32());
        let format = r.u8();
        let padding: [u8; 3] = r.take();
        if format != FORMAT_BGRA8 || padding != [0; 3] {
            return Err(DataError::Malformed("frame format"));
        }
        Ok(Self {
            core_epoch,
            generation,
            tab,
            sequence,
            width,
            height,
            stride,
            total_bytes: r.u64(),
        })
    }
    /// The size checks a receiver makes before reserving the frame: the
    /// dimensions are in the viewport range, the stride is exactly `width * 4`
    /// and the total is exactly `stride * height` (all in 64-bit arithmetic, so
    /// nothing overflows) and within [`MAX_FRAME_BYTES`].
    pub fn validate(&self) -> Result<(), DataError> {
        RendererViewport {
            width: self.width,
            height: self.height,
        }
        .validate()
        .map_err(|_| DataError::Malformed("frame size"))?;
        if u64::from(self.stride) != u64::from(self.width) * 4 {
            return Err(DataError::Malformed("frame stride"));
        }
        let expected = u64::from(self.stride) * u64::from(self.height);
        if self.total_bytes != expected {
            return Err(DataError::Malformed("frame total"));
        }
        if self.total_bytes > MAX_FRAME_BYTES {
            return Err(DataError::TooLarge {
                field: "frame",
                len: self.total_bytes,
                max: MAX_FRAME_BYTES,
            });
        }
        Ok(())
    }
}

/// Where a blob came from. A receiver treats the two differently: an artifact
/// carries the digest of the stored bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BlobSource {
    Artifact = 1,
    Capture = 2,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaType {
    Png = 1,
    Jpeg = 2,
    Gif = 3,
    Webp = 4,
}
impl MediaType {
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
    pub fn from_mime(mime: &str) -> Option<Self> {
        Some(match mime {
            "image/png" => Self::Png,
            "image/jpeg" => Self::Jpeg,
            "image/gif" => Self::Gif,
            "image/webp" => Self::Webp,
            _ => return None,
        })
    }
    fn from_wire(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Png,
            2 => Self::Jpeg,
            3 => Self::Gif,
            4 => Self::Webp,
            _ => return None,
        })
    }
}

/// An image the client reads through its connection: the original bytes of a
/// stored artifact, or a Capture PNG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobStart {
    pub core_epoch: u64,
    pub transfer_id: u64,
    pub source: BlobSource,
    pub media_type: MediaType,
    pub total_bytes: u64,
    /// SHA-256 of the bytes for an artifact (all zero for a Capture).
    pub digest: [u8; 32],
}
impl BlobStart {
    pub const LEN: usize = 8 + 8 + 1 + 1 + 6 + 8 + 32;
    /// `digest_hex` is the artifact reference's 64-digit digest.
    pub fn artifact_digest(digest_hex: &str) -> Result<[u8; 32], DataError> {
        digest_raw(digest_hex)
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        out.extend_from_slice(&self.core_epoch.to_le_bytes());
        out.extend_from_slice(&self.transfer_id.to_le_bytes());
        out.push(self.source as u8);
        out.push(self.media_type as u8);
        out.extend_from_slice(&[0; 6]);
        out.extend_from_slice(&self.total_bytes.to_le_bytes());
        out.extend_from_slice(&self.digest);
        encode_record(Kind::BlobStart, &out, &[])
    }
    pub fn decode(meta: &[u8]) -> Result<Self, DataError> {
        exact(meta, Self::LEN, "blob start")?;
        let mut r = Reader(meta);
        let core_epoch = r.u64();
        let transfer_id = r.u64();
        let source = match r.u8() {
            1 => BlobSource::Artifact,
            2 => BlobSource::Capture,
            _ => return Err(DataError::Malformed("blob source")),
        };
        let media_type = MediaType::from_wire(r.u8()).ok_or(DataError::Malformed("media type"))?;
        let padding: [u8; 6] = r.take();
        if padding != [0; 6] {
            return Err(DataError::Malformed("blob start"));
        }
        let total_bytes = r.u64();
        let digest = r.take();
        let start = Self {
            core_epoch,
            transfer_id,
            source,
            media_type,
            total_bytes,
            digest,
        };
        start.validate()?;
        Ok(start)
    }
    /// The size checks a receiver makes before reserving the blob. A Capture is
    /// always a PNG and has no digest; an artifact has one.
    pub fn validate(&self) -> Result<(), DataError> {
        let (max, digest_ok) = match self.source {
            BlobSource::Artifact => (MAX_IMAGE_BLOB_BYTES, self.digest != [0; 32]),
            BlobSource::Capture => (
                MAX_CAPTURE_BLOB_BYTES,
                self.digest == [0; 32] && self.media_type == MediaType::Png,
            ),
        };
        if self.total_bytes == 0 {
            return Err(DataError::Malformed("empty blob"));
        }
        if self.total_bytes > max {
            return Err(DataError::TooLarge {
                field: "blob",
                len: self.total_bytes,
                max,
            });
        }
        if !digest_ok {
            return Err(DataError::Malformed("blob digest"));
        }
        Ok(())
    }
}

/// A piece of the current frame or blob. `id` is the frame's sequence or the
/// blob's transfer id; `offset` is where the body starts in the whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkMeta {
    pub id: u64,
    pub offset: u64,
}
impl ChunkMeta {
    pub const LEN: usize = 16;
    pub fn encode(&self, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.offset.to_le_bytes());
        encode_record(Kind::Chunk, &out, body)
    }
    pub fn decode(meta: &[u8]) -> Result<Self, DataError> {
        exact(meta, Self::LEN, "chunk")?;
        let mut r = Reader(meta);
        Ok(Self {
            id: r.u64(),
            offset: r.u64(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndMeta {
    pub id: u64,
    pub total_bytes: u64,
}
impl EndMeta {
    pub const LEN: usize = 16;
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.total_bytes.to_le_bytes());
        encode_record(Kind::End, &out, &[])
    }
    pub fn decode(meta: &[u8]) -> Result<Self, DataError> {
        exact(meta, Self::LEN, "end")?;
        let mut r = Reader(meta);
        Ok(Self {
            id: r.u64(),
            total_bytes: r.u64(),
        })
    }
}

/// The client's receipt for one whole frame or blob. It is separate from the
/// helper's acknowledgement to the core: a slow data client never delays the
/// helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckMeta {
    pub id: u64,
}
impl AckMeta {
    pub const LEN: usize = 8;
    pub fn encode(&self) -> Vec<u8> {
        encode_record(Kind::Ack, &self.id.to_le_bytes(), &[])
    }
    pub fn decode(meta: &[u8]) -> Result<Self, DataError> {
        exact(meta, Self::LEN, "ack")?;
        Ok(Self {
            id: Reader(meta).u64(),
        })
    }
}

/// Why a data connection ends. A bounded code: no text, no path and nothing
/// that tells an unauthorized caller whether the target exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ErrorCode {
    /// Not bound, not allowed, or the target is not available to this caller.
    Denied = 1,
    /// The binding window passed.
    Expired = 2,
    /// A limit (connections, subscriptions, transfers) is reached.
    Busy = 3,
    /// The target ended: tab closed, helper stopped, observer revoked, drain.
    Closed = 4,
    /// A record or a size broke the protocol.
    Invalid = 5,
    /// No progress inside the no-progress window.
    TimedOut = 6,
}
impl ErrorCode {
    fn from_wire(value: u16) -> Option<Self> {
        Some(match value {
            1 => Self::Denied,
            2 => Self::Expired,
            3 => Self::Busy,
            4 => Self::Closed,
            5 => Self::Invalid,
            6 => Self::TimedOut,
            _ => return None,
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorMeta {
    pub code: ErrorCode,
}
impl ErrorMeta {
    pub const LEN: usize = 2;
    pub fn encode(&self) -> Vec<u8> {
        encode_record(Kind::Error, &(self.code as u16).to_le_bytes(), &[])
    }
    pub fn decode(meta: &[u8]) -> Result<Self, DataError> {
        exact(meta, Self::LEN, "error")?;
        let wire = Reader(meta).take::<2>();
        Ok(Self {
            code: ErrorCode::from_wire(u16::from_le_bytes(wire))
                .ok_or(DataError::Malformed("error code"))?,
        })
    }
}

/// Checks that the chunks of one frame or blob arrive as the `Start` announced:
/// one `id`, contiguous offsets from zero, at most [`CHUNK_BYTES`] each, no
/// byte past `total`, and an `End` that matches exactly. After any error the
/// connection is closed; a transfer is never resumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    id: u64,
    total: u64,
    received: u64,
}
impl Transfer {
    pub fn new(id: u64, total: u64) -> Self {
        Self {
            id,
            total,
            received: 0,
        }
    }
    pub fn received(&self) -> u64 {
        self.received
    }
    /// Accepts the next chunk of `len` bytes.
    pub fn chunk(&mut self, meta: ChunkMeta, len: usize) -> Result<(), DataError> {
        if meta.id != self.id {
            return Err(DataError::OutOfOrder("chunk of another transfer"));
        }
        if meta.offset != self.received {
            return Err(DataError::OutOfOrder("chunk offset"));
        }
        if len == 0 || len > CHUNK_BYTES {
            return Err(DataError::UnexpectedLength("chunk body"));
        }
        let end = self
            .received
            .checked_add(len as u64)
            .ok_or(DataError::Malformed("chunk end"))?;
        if end > self.total {
            return Err(DataError::TooLarge {
                field: "chunk",
                len: end,
                max: self.total,
            });
        }
        self.received = end;
        Ok(())
    }
    /// Accepts the `End`. Every byte must have arrived.
    pub fn end(&self, meta: EndMeta) -> Result<(), DataError> {
        if meta.id != self.id || meta.total_bytes != self.total {
            return Err(DataError::OutOfOrder("end of another transfer"));
        }
        if self.received != self.total {
            return Err(DataError::OutOfOrder("end before every byte"));
        }
        Ok(())
    }
}

/// Frame sequence numbers on one connection only move forward. A newer frame
/// replacing a skipped one is normal (the latest wins); a repeat or a step back
/// is not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SequenceGuard {
    last: Option<u64>,
}
impl SequenceGuard {
    /// Accepts `sequence` when it is greater than every earlier one.
    pub fn accept(&mut self, sequence: u64) -> Result<(), DataError> {
        if self.last.is_some_and(|last| sequence <= last) {
            return Err(DataError::OutOfOrder("frame sequence"));
        }
        self.last = Some(sequence);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
