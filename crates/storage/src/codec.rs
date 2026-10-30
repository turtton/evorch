//! Transparent zstd compression of large JSON text columns.
//!
//! `run_contexts.messages_json` / `checkpoints_json` and the payloads of the
//! bulky event kinds dominate the database size. A value of at least
//! [`MIN_COMPRESSED_BYTES`] is stored as a zstd frame in a BLOB; anything
//! smaller, or anything zstd cannot shrink, stays plain TEXT. Readers accept
//! both forms in the same column, so a database written before this codec
//! existed (all TEXT) reads unchanged and needs no bulk conversion.
//!
//! SQL cannot look inside a compressed value (`json_extract`, `instr`, ...).
//! Queries over these columns must either be restricted to rows that are never
//! compressed or fall back to decoding in Rust; see the callers.

use std::borrow::Cow;

use rusqlite::types::{ToSql, ToSqlOutput, ValueRef};

/// Values shorter than this stay plain TEXT: the frame overhead and the lost
/// SQL searchability are not worth the few bytes saved.
pub(crate) const MIN_COMPRESSED_BYTES: usize = 2048;

const LEVEL: i32 = 3;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// A column value ready to bind: plain TEXT or a zstd BLOB.
#[derive(Debug)]
pub(crate) enum Encoded<'a> {
    Plain(&'a str),
    Compressed(Vec<u8>),
}

impl Encoded<'_> {
    /// Bytes the value occupies in the database, which is what storage
    /// accounting charges.
    pub(crate) fn stored_len(&self) -> usize {
        match self {
            Self::Plain(text) => text.len(),
            Self::Compressed(bytes) => bytes.len(),
        }
    }
}

impl ToSql for Encoded<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Self::Plain(text) => ToSqlOutput::Borrowed(ValueRef::Text(text.as_bytes())),
            Self::Compressed(bytes) => ToSqlOutput::Borrowed(ValueRef::Blob(bytes)),
        })
    }
}

/// Compresses `text` when it is large enough and zstd actually shrinks it.
pub(crate) fn encode(text: &str) -> Encoded<'_> {
    if text.len() < MIN_COMPRESSED_BYTES {
        return Encoded::Plain(text);
    }
    match zstd::bulk::compress(text.as_bytes(), LEVEL) {
        Ok(bytes) if bytes.len() < text.len() => Encoded::Compressed(bytes),
        _ => Encoded::Plain(text),
    }
}

/// Why a stored value could not be read back as text.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DecodeError {
    /// The column held neither TEXT nor BLOB.
    NotText,
    /// A BLOB that is not a zstd frame.
    NotZstd,
    /// A zstd frame that does not decompress.
    Corrupt(String),
    /// Decompressed bytes that are not UTF-8.
    NotUtf8,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotText => write!(formatter, "stored value is neither TEXT nor a BLOB"),
            Self::NotZstd => write!(formatter, "stored BLOB is not a zstd frame"),
            Self::Corrupt(reason) => write!(formatter, "stored zstd frame is corrupt: {reason}"),
            Self::NotUtf8 => write!(formatter, "stored value is not valid UTF-8"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<DecodeError> for crate::StorageError {
    fn from(error: DecodeError) -> Self {
        Self::Serialization(error.to_string())
    }
}

/// The text of a stored value. TEXT is borrowed as is; a BLOB must be a zstd
/// frame (magic `28 B5 2F FD`), which is decompressed.
pub(crate) fn decode(value: ValueRef<'_>) -> Result<Cow<'_, str>, DecodeError> {
    #[cfg(test)]
    if matches!(value, ValueRef::Blob(_)) {
        tests::DECODED_BLOBS.with(|count| count.set(count.get() + 1));
    }
    match value {
        ValueRef::Text(bytes) => std::str::from_utf8(bytes)
            .map(Cow::Borrowed)
            .map_err(|_| DecodeError::NotUtf8),
        ValueRef::Blob(bytes) => {
            if !bytes.starts_with(&ZSTD_MAGIC) {
                return Err(DecodeError::NotZstd);
            }
            let plain = zstd::stream::decode_all(bytes)
                .map_err(|error| DecodeError::Corrupt(error.to_string()))?;
            String::from_utf8(plain)
                .map(Cow::Owned)
                .map_err(|_| DecodeError::NotUtf8)
        }
        _ => Err(DecodeError::NotText),
    }
}

/// Reads column `index` of `row` as decoded text, for `query_map` closures.
pub(crate) fn text_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<String> {
    let value = row.get_ref(index)?;
    let data_type = value.data_type();
    decode(value).map(Cow::into_owned).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, data_type, Box::new(error))
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::Cell;

    use rusqlite::types::Value;

    thread_local! {
        /// BLOBs decompressed (or rejected) by [`decode`] on this thread, for tests
        /// that bound how many compressed rows a lookup may read.
        pub(crate) static DECODED_BLOBS: Cell<usize> = const { Cell::new(0) };
    }

    use super::*;

    fn stored(encoded: &Encoded<'_>) -> Value {
        match encoded.to_sql().unwrap() {
            ToSqlOutput::Borrowed(ValueRef::Text(bytes)) => {
                Value::Text(String::from_utf8(bytes.to_vec()).unwrap())
            }
            ToSqlOutput::Borrowed(ValueRef::Blob(bytes)) => Value::Blob(bytes.to_vec()),
            _ => unreachable!(),
        }
    }

    #[test]
    fn values_under_the_threshold_stay_plain_text() {
        // Given: a value one byte under the threshold and one at it.
        let small = "x".repeat(MIN_COMPRESSED_BYTES - 1);
        let at_threshold = "x".repeat(MIN_COMPRESSED_BYTES);

        // When/Then: the first stays TEXT and reads back unchanged; the second is compressed.
        let encoded = encode(&small);
        assert_eq!(encoded.stored_len(), small.len());
        let value = stored(&encoded);
        assert!(matches!(value, Value::Text(_)));
        assert_eq!(decode(ValueRef::from(&value)).unwrap(), small);
        assert!(matches!(encode(&at_threshold), Encoded::Compressed(_)));
    }

    #[test]
    fn large_values_roundtrip_byte_for_byte_as_blob() {
        // Given: a 9 MiB JSON-like text with multibyte characters.
        let text = "{\"role\":\"assistant\",\"text\":\"こんにちは 🙂 line\"}\n".repeat(200_000);
        assert!(text.len() > 9 * 1024 * 1024);

        // When: encoded and decoded.
        let encoded = encode(&text);
        let value = stored(&encoded);

        // Then: stored as a smaller zstd BLOB that decodes to the identical text.
        let Value::Blob(bytes) = &value else {
            panic!("large value must be a BLOB");
        };
        assert_eq!(bytes[..4], ZSTD_MAGIC);
        assert_eq!(encoded.stored_len(), bytes.len());
        assert!(bytes.len() < text.len() / 4);
        let decoded = decode(ValueRef::from(&value)).unwrap();
        assert_eq!(decoded.as_bytes(), text.as_bytes());
    }

    #[test]
    fn plain_text_decodes_without_copying_and_foreign_values_are_rejected() {
        // Given/When/Then: TEXT is borrowed; other BLOBs, corrupt frames and
        // non-text types fail instead of being guessed at.
        assert!(matches!(
            decode(ValueRef::Text(b"[1]")).unwrap(),
            Cow::Borrowed("[1]")
        ));
        assert_eq!(decode(ValueRef::Blob(&[0xFF])), Err(DecodeError::NotZstd));
        assert_eq!(
            decode(ValueRef::Blob(b"[1,2,3]")),
            Err(DecodeError::NotZstd)
        );
        let mut frame = zstd::bulk::compress(&[b'a'; 4096], LEVEL).unwrap();
        frame.truncate(frame.len() - 3);
        assert!(matches!(
            decode(ValueRef::Blob(&frame)),
            Err(DecodeError::Corrupt(_))
        ));
        assert_eq!(decode(ValueRef::Integer(1)), Err(DecodeError::NotText));
        assert_eq!(decode(ValueRef::Null), Err(DecodeError::NotText));
        let invalid = zstd::bulk::compress(&[0xFF; 4096], LEVEL).unwrap();
        assert_eq!(decode(ValueRef::Blob(&invalid)), Err(DecodeError::NotUtf8));
    }
}
