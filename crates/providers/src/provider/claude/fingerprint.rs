use std::{fmt, marker::PhantomData, ops::Range};

use serde::{Deserialize, Deserializer, de};
use sha2::{Digest, Sha256};
use xxhash_rust::xxh64::Xxh64;

use crate::ProviderError;

#[cfg(test)]
mod tests;

const FINGERPRINT_SALT: &str = "59cf53e54c78";
const BILLING_PREFIX: &str = "x-anthropic-billing-header: cc_version=";
const CHECKSUM_PLACEHOLDER: &str = " cch=00000;";
const CHECKSUM_SEED: u64 = 0x4d65_9218_e32a_3268;

/// Match JavaScript string indexing, including its UTF-8 replacement of lone surrogates.
pub(crate) fn version_suffix(first_user_text: &str, version: &str) -> String {
    let mut selected = [u16::from(b'0'); 3];
    for (index, unit) in first_user_text.encode_utf16().take(21).enumerate() {
        if let Some(slot) = [4, 7, 20].iter().position(|candidate| *candidate == index) {
            selected[slot] = unit;
        }
    }
    // Join the selected UTF-16 units before decoding: adjacent selections can form a pair.
    let sample = String::from_utf16_lossy(&selected);
    let mut hash = Sha256::new();
    hash.update(FINGERPRINT_SALT.as_bytes());
    hash.update(sample.as_bytes());
    hash.update(version.as_bytes());
    let digest = hash.finalize();
    format!("{:02x}{:x}", digest[0], digest[1] >> 4)
}

pub(crate) fn billing_header(version: &str, first_user_text: &str) -> String {
    let suffix = version_suffix(first_user_text, version);
    format!("{BILLING_PREFIX}{version}.{suffix}; cc_entrypoint=cli;{CHECKSUM_PLACEHOLDER}")
}

/// Patch the final serialized request in place; callers must send these exact bytes.
pub(crate) fn finalize_billing_checksum(body: &mut [u8]) -> Result<(), ProviderError> {
    let range = checksum_range(body)?;
    let checksum = format!("{:05x}", native_checksum(body) & 0x000f_ffff);
    body[range].copy_from_slice(checksum.as_bytes());
    Ok(())
}

// Claude Code 2.1.295 hashes raw bytes while omitting routing fields. These literal
// matches intentionally apply inside schemas too; parsing and reserializing changes cch.
fn native_checksum(body: &[u8]) -> u64 {
    let mut hash = Xxh64::new(CHECKSUM_SEED);
    let mut cursor = 0;
    let mut field = next_routing_field(body, cursor);
    loop {
        let model = next_model_value(body, cursor);
        let use_field = field
            .as_ref()
            .is_some_and(|field| model.as_ref().is_none_or(|model| field.start < model.start));
        let range = if use_field { field.take() } else { model };
        let Some(range) = range else {
            hash.update(&body[cursor..]);
            return hash.digest();
        };
        hash.update(&body[cursor..range.start]);
        cursor = range.end;
        if use_field {
            field = next_routing_field(body, cursor);
        }
    }
}

fn next_model_value(body: &[u8], cursor: usize) -> Option<Range<usize>> {
    let marker = b"\"model\":\"";
    let start = find_bytes(body, cursor, marker)? + marker.len();
    let end = start + body[start..].iter().position(|byte| *byte == b'"')?;
    Some(start..end)
}

fn next_routing_field(body: &[u8], cursor: usize) -> Option<Range<usize>> {
    let fallbacks = find_bytes(body, cursor, b"\"fallbacks\":[").and_then(|start| {
        array_end(body, start + b"\"fallbacks\":".len())
            .map(|end| field_with_comma(body, cursor, start..end))
    });
    let credit = find_bytes(body, cursor, b"\"fallback_credit_token\":\"").and_then(|start| {
        let value = start + b"\"fallback_credit_token\":\"".len();
        let end = value + body[value..].iter().position(|byte| *byte == b'"')? + 1;
        Some(field_with_comma(body, cursor, start..end))
    });
    [fallbacks, credit, next_max_tokens(body, cursor)]
        .into_iter()
        .flatten()
        .min_by_key(|range| range.start)
}

fn next_max_tokens(body: &[u8], cursor: usize) -> Option<Range<usize>> {
    let marker = b"\"max_tokens\":";
    let mut search = cursor;
    while let Some(start) = find_bytes(body, search, marker) {
        let value = start + marker.len();
        let end = value
            + body[value..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
        if end > value {
            return Some(field_with_comma(body, cursor, start..end));
        }
        search = value;
    }
    None
}

fn field_with_comma(body: &[u8], cursor: usize, mut range: Range<usize>) -> Range<usize> {
    if body.get(range.end) == Some(&b',') {
        range.end += 1;
    } else if range.start > cursor && body[range.start - 1] == b',' {
        range.start -= 1;
    }
    range
}

fn array_end(body: &[u8], start: usize) -> Option<usize> {
    let mut depth = 1;
    let mut quoted = false;
    let mut cursor = start + 1;
    while cursor < body.len() {
        match body[cursor] {
            b'\\' if quoted => cursor += 1,
            b'"' => quoted = !quoted,
            b'[' if !quoted => depth += 1,
            b']' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return Some(cursor + 1);
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

fn find_bytes(body: &[u8], cursor: usize, marker: &[u8]) -> Option<usize> {
    body[cursor..]
        .windows(marker.len())
        .position(|window| window == marker)
        .map(|offset| cursor + offset)
}

fn checksum_range(body: &[u8]) -> Result<Range<usize>, ProviderError> {
    let request: BillingEnvelope<'_> =
        serde_json::from_slice(body).map_err(|_| invalid_billing())?;
    let first = request.system.0;
    if first.kind != "text" || !first.text.starts_with(BILLING_PREFIX) {
        return Err(invalid_billing());
    }
    let mut matches = first.text.match_indices(CHECKSUM_PLACEHOLDER);
    let Some((placeholder_start, _)) = matches.next() else {
        return Err(invalid_billing());
    };
    if matches.next().is_some() {
        return Err(invalid_billing());
    }
    // The deserializer borrows this unescaped string directly from the original body.
    let text_start = first.text.as_ptr() as usize - body.as_ptr() as usize;
    let checksum_start = text_start + placeholder_start + " cch=".len();
    Ok(checksum_start..checksum_start + 5)
}

fn invalid_billing() -> ProviderError {
    ProviderError::Request("Claude request has an invalid billing checksum placeholder".into())
}

#[derive(Deserialize)]
struct BillingEnvelope<'a> {
    #[serde(borrow)]
    system: FirstSystemBlock<'a>,
}

#[derive(Deserialize)]
struct BillingBlock<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    text: &'a str,
}

struct FirstSystemBlock<'a>(BillingBlock<'a>);

impl<'de: 'a, 'a> Deserialize<'de> for FirstSystemBlock<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FirstBlockVisitor<'a>(PhantomData<&'a str>);

        impl<'de: 'a, 'a> de::Visitor<'de> for FirstBlockVisitor<'a> {
            type Value = FirstSystemBlock<'a>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a system array starting with a billing text block")
            }

            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let first = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::custom("missing billing block"))?;
                while seq.next_element::<de::IgnoredAny>()?.is_some() {}
                Ok(FirstSystemBlock(first))
            }
        }

        deserializer.deserialize_seq(FirstBlockVisitor(PhantomData))
    }
}
