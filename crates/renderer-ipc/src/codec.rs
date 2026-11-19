//! Message framing shared by both channels.
//!
//! ```text
//! offset  size  field
//!  0      4     magic "ERIP"
//!  4      2     protocol version (little endian)
//!  6      1     kind
//!  7      1     reserved, zero
//!  8      4     meta length  (little endian)
//! 12      4     body length  (little endian)
//! 16      ..    meta: JSON, then body: raw bytes
//! ```
//!
//! The meta part is a small JSON document; the body carries bulk bytes (raw
//! BGRA pixels, a PNG, artifact files) without any text encoding. Both
//! lengths are checked against the channel's [`Limits`] before anything is
//! allocated, so a corrupt or hostile length cannot make a peer reserve
//! memory.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::protocol::{MAX_FRAME_BYTES, PROTOCOL_VERSION};

const MAGIC: [u8; 4] = *b"ERIP";
pub const HEADER_LEN: usize = 16;

/// Largest JSON part of a control message.
pub const MAX_CONTROL_META: usize = 1024 * 1024;
/// Largest bulk part of a control message (artifact files, a PNG).
pub const MAX_CONTROL_BODY: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Request = 1,
    Response = 2,
    Event = 3,
    Frame = 4,
    Ack = 5,
}

impl Kind {
    fn from_wire(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Request,
            2 => Self::Response,
            3 => Self::Event,
            4 => Self::Frame,
            5 => Self::Ack,
            _ => return None,
        })
    }
}

/// Upper bounds one channel accepts for a single message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_meta: usize,
    pub max_body: usize,
}

/// Requests, responses and events.
pub const CONTROL_LIMITS: Limits = Limits {
    max_meta: MAX_CONTROL_META,
    max_body: MAX_CONTROL_BODY,
};

/// Frames from the helper to the core: a short meta and the pixels.
pub const FRAME_LIMITS: Limits = Limits {
    max_meta: 1024,
    max_body: MAX_FRAME_BYTES,
};

/// Acknowledgements from the core on the frame channel: meta only.
pub const ACK_LIMITS: Limits = Limits {
    max_meta: 256,
    max_body: 0,
};

#[derive(Debug)]
pub struct Message {
    pub kind: Kind,
    pub meta: Vec<u8>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// The peer closed the stream between two messages.
    Closed,
    /// The peer closed the stream inside a message.
    Truncated,
    /// A message began, but its remainder did not arrive in time.
    Stalled,
    /// No message began within the stream's read timeout, or a write did not
    /// complete within its write timeout. Nothing of a message was consumed
    /// on a read.
    Timeout,
    BadMagic,
    UnsupportedVersion(u16),
    UnknownKind(u8),
    TooLarge {
        field: &'static str,
        len: u64,
        max: u64,
    },
    /// The meta part is not a valid document of the expected type.
    Malformed(String),
    Io(io::ErrorKind),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "the peer closed the channel"),
            Self::Truncated => write!(f, "the peer closed the channel inside a message"),
            Self::Stalled => write!(f, "a message stalled before it completed"),
            Self::Timeout => write!(f, "the channel timed out"),
            Self::BadMagic => write!(f, "the message does not start with the protocol magic"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported protocol version {version}")
            }
            Self::UnknownKind(kind) => write!(f, "unknown message kind {kind}"),
            Self::TooLarge { field, len, max } => {
                write!(f, "{field} of {len} bytes exceeds the limit of {max}")
            }
            Self::Malformed(detail) => write!(f, "malformed message: {detail}"),
            Self::Io(kind) => write!(f, "channel I/O error: {kind}"),
        }
    }
}

impl std::error::Error for CodecError {}

fn classify(error: &io::Error, partial: bool) -> CodecError {
    match error.kind() {
        io::ErrorKind::UnexpectedEof => CodecError::Truncated,
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut if partial => CodecError::Stalled,
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => CodecError::Timeout,
        // The peer is gone; the stream will not carry another message.
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset => CodecError::Closed,
        kind => CodecError::Io(kind),
    }
}

/// Writes one message. Refuses to send what `limits` would make the peer
/// reject, so an oversized message fails here instead of killing the channel.
pub fn write_message(
    mut stream: impl Write,
    kind: Kind,
    meta: &[u8],
    body: &[u8],
    limits: Limits,
) -> Result<(), CodecError> {
    if meta.len() > limits.max_meta {
        return Err(CodecError::TooLarge {
            field: "meta",
            len: meta.len() as u64,
            max: limits.max_meta as u64,
        });
    }
    if body.len() > limits.max_body {
        return Err(CodecError::TooLarge {
            field: "body",
            len: body.len() as u64,
            max: limits.max_body as u64,
        });
    }
    let mut head = Vec::with_capacity(HEADER_LEN + meta.len());
    head.extend_from_slice(&MAGIC);
    head.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    head.push(kind as u8);
    head.push(0);
    head.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    head.extend_from_slice(&(body.len() as u32).to_le_bytes());
    head.extend_from_slice(meta);
    stream.write_all(&head).map_err(|e| classify(&e, false))?;
    stream.write_all(body).map_err(|e| classify(&e, false))?;
    stream.flush().map_err(|e| classify(&e, false))
}

/// What to do with a body whose header and meta were already read.
pub enum Admit {
    /// Read the body into this buffer (cleared first, grown to the body's
    /// length). The caller reserved whatever the buffer costs.
    Read(Vec<u8>),
    /// Read the body and drop it without buffering, to stay in step with the
    /// stream (for example a stale frame or one over a memory budget).
    Discard,
}

/// Reads one message. The first byte waits for the stream's current read
/// timeout (none for an idle control reader); once a message has begun, the
/// rest must arrive within `message_timeout`. `body_buf` is reused for the
/// body so a steady frame stream does not allocate per frame.
pub fn read_message(
    stream: &UnixStream,
    limits: Limits,
    body_buf: Vec<u8>,
    message_timeout: Duration,
) -> Result<Message, CodecError> {
    let mut buffer = Some(body_buf);
    read_message_with(stream, limits, message_timeout, |_, _, _| {
        Ok(Admit::Read(buffer.take().unwrap_or_default()))
    })
}

/// Like [`read_message`], but `admit` sees the kind, the meta and the announced
/// body length **before any body byte is read or buffered**, and decides
/// whether to buffer, discard or reject it. A discarded body comes back empty.
/// The meta is already bounded by `limits`, so `admit` may decode it freely.
pub fn read_message_with(
    stream: &UnixStream,
    limits: Limits,
    message_timeout: Duration,
    admit: impl FnOnce(Kind, &[u8], usize) -> Result<Admit, CodecError>,
) -> Result<Message, CodecError> {
    let mut reader = stream;
    let idle = stream.read_timeout().map_err(|e| classify(&e, false))?;
    let mut head = [0u8; HEADER_LEN];
    let first = loop {
        match reader.read(&mut head[..1]) {
            Ok(count) => break count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(classify(&error, false)),
        }
    };
    if first == 0 {
        return Err(CodecError::Closed);
    }
    // The first byte arrived, so a timeout from here on means a stalled
    // message and the channel can no longer be framed.
    stream
        .set_read_timeout(Some(message_timeout.max(Duration::from_millis(1))))
        .map_err(|e| classify(&e, true))?;
    let result = read_rest(&mut reader, &mut head, limits, admit);
    let restore = stream.set_read_timeout(idle);
    let (kind, meta, body) = result?;
    restore.map_err(|e| classify(&e, true))?;
    Ok(Message { kind, meta, body })
}

fn read_rest(
    reader: &mut &UnixStream,
    head: &mut [u8; HEADER_LEN],
    limits: Limits,
    admit: impl FnOnce(Kind, &[u8], usize) -> Result<Admit, CodecError>,
) -> Result<(Kind, Vec<u8>, Vec<u8>), CodecError> {
    reader
        .read_exact(&mut head[1..])
        .map_err(|e| classify(&e, true))?;
    if head[..4] != MAGIC {
        return Err(CodecError::BadMagic);
    }
    let version = u16::from_le_bytes([head[4], head[5]]);
    if version != PROTOCOL_VERSION {
        return Err(CodecError::UnsupportedVersion(version));
    }
    let kind = Kind::from_wire(head[6]).ok_or(CodecError::UnknownKind(head[6]))?;
    let meta_len = u32::from_le_bytes([head[8], head[9], head[10], head[11]]) as usize;
    let body_len = u32::from_le_bytes([head[12], head[13], head[14], head[15]]) as usize;
    if meta_len > limits.max_meta {
        return Err(CodecError::TooLarge {
            field: "meta",
            len: meta_len as u64,
            max: limits.max_meta as u64,
        });
    }
    if body_len > limits.max_body {
        return Err(CodecError::TooLarge {
            field: "body",
            len: body_len as u64,
            max: limits.max_body as u64,
        });
    }
    let mut meta = vec![0u8; meta_len];
    reader
        .read_exact(&mut meta)
        .map_err(|e| classify(&e, true))?;
    match admit(kind, &meta, body_len)? {
        Admit::Read(mut body_buf) => {
            body_buf.clear();
            // `take` + `read_to_end` fills the buffer without first zeroing it.
            body_buf.reserve(body_len);
            let read = reader
                .take(body_len as u64)
                .read_to_end(&mut body_buf)
                .map_err(|e| classify(&e, true))?;
            if read != body_len {
                return Err(CodecError::Truncated);
            }
            Ok((kind, meta, body_buf))
        }
        Admit::Discard => {
            let copied = io::copy(&mut reader.take(body_len as u64), &mut io::sink())
                .map_err(|e| classify(&e, true))?;
            if copied != body_len as u64 {
                return Err(CodecError::Truncated);
            }
            Ok((kind, meta, Vec::new()))
        }
    }
}

/// Serializes a meta document.
pub fn encode_meta<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    serde_json::to_vec(value).map_err(|e| CodecError::Malformed(e.to_string()))
}

/// Parses a meta document.
pub fn decode_meta<T: serde::de::DeserializeOwned>(meta: &[u8]) -> Result<T, CodecError> {
    serde_json::from_slice(meta).map_err(|e| CodecError::Malformed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Duration = Duration::from_secs(30);

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    #[test]
    fn a_message_round_trips_with_meta_and_body() {
        let (a, b) = pair();
        write_message(&a, Kind::Frame, b"{\"x\":1}", &[1, 2, 3], FRAME_LIMITS).unwrap();
        let message = read_message(&b, FRAME_LIMITS, Vec::new(), SHORT).unwrap();
        assert_eq!(message.kind, Kind::Frame);
        assert_eq!(message.meta, b"{\"x\":1}");
        assert_eq!(message.body, [1, 2, 3]);
    }

    #[test]
    fn the_body_buffer_is_reused_across_messages() {
        let (a, b) = pair();
        write_message(&a, Kind::Frame, b"{}", &[7; 4096], FRAME_LIMITS).unwrap();
        let first = read_message(&b, FRAME_LIMITS, Vec::new(), SHORT).unwrap();
        let capacity = first.body.capacity();
        write_message(&a, Kind::Frame, b"{}", &[9; 16], FRAME_LIMITS).unwrap();
        let second = read_message(&b, FRAME_LIMITS, first.body, SHORT).unwrap();
        assert_eq!(second.body, [9; 16]);
        assert_eq!(second.body.capacity(), capacity);
    }

    #[test]
    fn a_discarded_body_is_consumed_and_the_next_message_stays_in_step() {
        let (a, b) = pair();
        write_message(&a, Kind::Frame, b"{}", &[7; 4096], FRAME_LIMITS).unwrap();
        write_message(&a, Kind::Frame, b"{\"n\":2}", &[1, 2], FRAME_LIMITS).unwrap();
        let first = read_message_with(&b, FRAME_LIMITS, SHORT, |kind, meta, len| {
            assert_eq!((kind, meta, len), (Kind::Frame, b"{}".as_slice(), 4096));
            Ok(Admit::Discard)
        })
        .unwrap();
        assert!(first.body.is_empty() && first.body.capacity() == 0);
        let second = read_message(&b, FRAME_LIMITS, Vec::new(), SHORT).unwrap();
        assert_eq!(second.body, [1, 2]);
    }

    #[test]
    fn a_rejected_message_is_refused_before_any_body_byte_is_read() {
        let (a, b) = pair();
        // The body never arrives: only a refusal made before reading it can
        // return without waiting for the message timeout.
        let mut header = b"ERIP".to_vec();
        header.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        header.extend_from_slice(&[Kind::Frame as u8, 0]);
        header.extend_from_slice(&2u32.to_le_bytes());
        header.extend_from_slice(&1_000_000u32.to_le_bytes());
        (&a).write_all(&header).unwrap();
        (&a).write_all(b"{}").unwrap();
        let error = read_message_with(&b, FRAME_LIMITS, SHORT, |_, _, len| {
            Err(CodecError::Malformed(format!("{len} is not acceptable")))
        })
        .unwrap_err();
        assert_eq!(
            error,
            CodecError::Malformed("1000000 is not acceptable".to_owned())
        );
    }

    #[test]
    fn a_clean_close_between_messages_is_reported_as_closed() {
        let (a, b) = pair();
        drop(a);
        assert_eq!(
            read_message(&b, CONTROL_LIMITS, Vec::new(), SHORT).unwrap_err(),
            CodecError::Closed
        );
    }

    fn raw_header(magic: &[u8; 4], version: u16, kind: u8, meta: u32, body: u32) -> Vec<u8> {
        let mut head = magic.to_vec();
        head.extend_from_slice(&version.to_le_bytes());
        head.push(kind);
        head.push(0);
        head.extend_from_slice(&meta.to_le_bytes());
        head.extend_from_slice(&body.to_le_bytes());
        head
    }

    fn read_raw(bytes: &[u8], limits: Limits) -> Result<Message, CodecError> {
        let (mut a, b) = pair();
        a.write_all(bytes).unwrap();
        // Closing the writer ends the stream, so a short message is truncated
        // rather than waiting for more bytes.
        drop(a);
        read_message(&b, limits, Vec::new(), SHORT)
    }

    #[test]
    fn damaged_headers_are_rejected_without_allocating_what_they_announce() {
        let good_version = PROTOCOL_VERSION;
        assert_eq!(
            read_raw(&raw_header(b"NOPE", good_version, 1, 0, 0), CONTROL_LIMITS).unwrap_err(),
            CodecError::BadMagic
        );
        assert_eq!(
            read_raw(
                &raw_header(&MAGIC, good_version + 1, 1, 0, 0),
                CONTROL_LIMITS
            )
            .unwrap_err(),
            CodecError::UnsupportedVersion(good_version + 1)
        );
        assert_eq!(
            read_raw(&raw_header(&MAGIC, good_version, 99, 0, 0), CONTROL_LIMITS).unwrap_err(),
            CodecError::UnknownKind(99)
        );
        // The announced body is 4 GiB; the peer sends none of it. The limit
        // check must fire before any read or allocation.
        assert_eq!(
            read_raw(
                &raw_header(&MAGIC, good_version, 4, 2, u32::MAX),
                FRAME_LIMITS
            )
            .unwrap_err(),
            CodecError::TooLarge {
                field: "body",
                len: u64::from(u32::MAX),
                max: FRAME_LIMITS.max_body as u64,
            }
        );
        assert_eq!(
            read_raw(
                &raw_header(&MAGIC, good_version, 1, u32::MAX, 0),
                CONTROL_LIMITS
            )
            .unwrap_err(),
            CodecError::TooLarge {
                field: "meta",
                len: u64::from(u32::MAX),
                max: CONTROL_LIMITS.max_meta as u64,
            }
        );
    }

    #[test]
    fn a_message_cut_short_is_truncated() {
        let mut bytes = raw_header(&MAGIC, PROTOCOL_VERSION, 4, 2, 8);
        bytes.extend_from_slice(b"{}");
        bytes.extend_from_slice(&[0; 3]);
        assert_eq!(
            read_raw(&bytes, FRAME_LIMITS).unwrap_err(),
            CodecError::Truncated
        );
        assert_eq!(
            read_raw(&bytes[..9], FRAME_LIMITS).unwrap_err(),
            CodecError::Truncated
        );
    }

    #[test]
    fn writing_more_than_the_limit_fails_before_anything_is_sent() {
        let (a, b) = pair();
        let error = write_message(&a, Kind::Ack, b"{}", &[0], ACK_LIMITS).unwrap_err();
        assert!(matches!(error, CodecError::TooLarge { field: "body", .. }));
        drop(a);
        assert_eq!(
            read_message(&b, ACK_LIMITS, Vec::new(), SHORT).unwrap_err(),
            CodecError::Closed
        );
    }
}
