//! response size 三面チェック。

use std::io::Read;

use flate2::read::{DeflateDecoder, MultiGzDecoder, ZlibDecoder};
use reqwest::header::{CONTENT_ENCODING, CONTENT_LENGTH, HeaderMap};

use super::{MAX_RESPONSE_BYTES, NetworkGuardError};

pub(crate) fn check_content_length(headers: &HeaderMap) -> Result<(), NetworkGuardError> {
    let Some(value) = headers.get(CONTENT_LENGTH) else {
        return Ok(());
    };
    let Ok(value) = value.to_str() else {
        return Ok(());
    };
    let Ok(length) = value.parse::<u64>() else {
        return Ok(());
    };
    if length > MAX_RESPONSE_BYTES as u64 {
        return Err(too_large("Content-Length"));
    }
    Ok(())
}

pub(crate) fn append_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), NetworkGuardError> {
    if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
        return Err(too_large("streaming"));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

pub(crate) fn decode(headers: &HeaderMap, raw: &[u8]) -> Result<Vec<u8>, NetworkGuardError> {
    let encoding = headers
        .get(CONTENT_ENCODING)
        .map(|value| value.to_str())
        .transpose()
        .map_err(|error| NetworkGuardError::DecompressionFailed(error.to_string()))?
        .map(str::trim)
        .map(str::to_ascii_lowercase);

    match encoding.as_deref() {
        None | Some("") | Some("identity") => Ok(raw.to_vec()),
        Some("gzip") => read_bounded(MultiGzDecoder::new(raw)),
        Some("deflate") => match read_bounded(ZlibDecoder::new(raw)) {
            Ok(body) => Ok(body),
            Err(NetworkGuardError::DecompressionFailed(_)) => {
                read_bounded(DeflateDecoder::new(raw))
            }
            Err(other) => Err(other),
        },
        Some(other) => Err(NetworkGuardError::DecompressionFailed(format!(
            "未対応の Content-Encoding です: {other}"
        ))),
    }
}

fn read_bounded(mut decoder: impl Read) -> Result<Vec<u8>, NetworkGuardError> {
    let mut decoded = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = decoder
            .read(&mut chunk)
            .map_err(|error| NetworkGuardError::DecompressionFailed(error.to_string()))?;
        if read == 0 {
            return Ok(decoded);
        }
        if decoded.len().saturating_add(read) > MAX_RESPONSE_BYTES {
            return Err(too_large("decompressed"));
        }
        decoded.extend_from_slice(&chunk[..read]);
    }
}

const fn too_large(check: &'static str) -> NetworkGuardError {
    NetworkGuardError::ResponseTooLarge {
        check,
        limit: MAX_RESPONSE_BYTES,
    }
}
