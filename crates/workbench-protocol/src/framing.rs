//! Length-prefixed JSON frames.
//!
//! A frame is a 4-byte big-endian payload length followed by that many bytes of
//! UTF-8 JSON. The length is checked against the limit before any payload
//! buffer is allocated, so a hostile peer cannot make the receiver reserve
//! memory by lying about size.

use crate::limits::HARD_MAX_FRAME_BYTES;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Bytes in the length prefix.
pub const HEADER_BYTES: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    #[error("frame of {len} bytes exceeds the limit of {max}")]
    FrameTooLarge { len: u64, max: u32 },
    #[error("empty frame")]
    EmptyFrame,
    #[error("frame is not valid JSON for the expected type: {0}")]
    Json(#[from] serde_json::Error),
}

/// Serialize `value` into one frame, refusing payloads over `max_frame_bytes`.
pub fn encode_frame<T: Serialize>(
    value: &T,
    max_frame_bytes: u32,
) -> Result<Vec<u8>, FramingError> {
    let payload = serde_json::to_vec(value)?;
    let max = max_frame_bytes.min(HARD_MAX_FRAME_BYTES);
    if payload.len() as u64 > u64::from(max) {
        return Err(FramingError::FrameTooLarge {
            len: payload.len() as u64,
            max,
        });
    }
    let mut frame = Vec::with_capacity(HEADER_BYTES + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Parse one frame payload (without its header).
pub fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FramingError> {
    Ok(serde_json::from_slice(payload)?)
}

/// Incremental frame reader: feed it whatever the transport yields, in
/// pieces of any size, and take complete payloads out.
///
/// After an error the stream position is unknown, so the decoder refuses
/// further input; the connection must be dropped.
#[derive(Debug)]
pub struct FrameDecoder {
    max_frame_bytes: u32,
    buffer: Vec<u8>,
    failed: Option<(u64, u32)>,
}

impl FrameDecoder {
    pub fn new(max_frame_bytes: u32) -> Self {
        Self {
            max_frame_bytes: max_frame_bytes.min(HARD_MAX_FRAME_BYTES),
            buffer: Vec::new(),
            failed: None,
        }
    }

    pub fn max_frame_bytes(&self) -> u32 {
        self.max_frame_bytes
    }

    /// Change the limit, for example after the handshake negotiated one.
    pub fn set_max_frame_bytes(&mut self, max_frame_bytes: u32) {
        self.max_frame_bytes = max_frame_bytes.min(HARD_MAX_FRAME_BYTES);
    }

    /// Bytes buffered but not yet returned.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// Add received bytes. Buffers at most one frame plus the bytes of this call.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.failed.is_none() {
            self.buffer.extend_from_slice(bytes);
        }
    }

    /// Take the next complete payload, or `None` if more bytes are needed.
    pub fn next_payload(&mut self) -> Result<Option<Vec<u8>>, FramingError> {
        if let Some((len, max)) = self.failed {
            return Err(FramingError::FrameTooLarge { len, max });
        }
        let Some(header) = self.buffer.first_chunk::<HEADER_BYTES>() else {
            return Ok(None);
        };
        let len = u32::from_be_bytes(*header);
        if len == 0 {
            self.buffer.clear();
            self.failed = Some((0, self.max_frame_bytes));
            return Err(FramingError::EmptyFrame);
        }
        if len > self.max_frame_bytes {
            self.buffer.clear();
            self.buffer.shrink_to_fit();
            self.failed = Some((u64::from(len), self.max_frame_bytes));
            return Err(FramingError::FrameTooLarge {
                len: u64::from(len),
                max: self.max_frame_bytes,
            });
        }
        let total = HEADER_BYTES + len as usize;
        if self.buffer.len() < total {
            return Ok(None);
        }
        let payload = self.buffer[HEADER_BYTES..total].to_vec();
        self.buffer.drain(..total);
        Ok(Some(payload))
    }
}
