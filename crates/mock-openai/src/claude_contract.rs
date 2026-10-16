//! Independent validation of Claude subscription control data at the HTTP boundary.
//!
//! This validates the captured bytes before removing the first billing control
//! block. It does not assert how Anthropic accounts for usage or cache hits.

use std::ops::Range;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use xxhash_rust::xxh64::xxh64;

const BILLING_START: &str = "x-anthropic-billing-header: cc_version=2.1.295.";
const BILLING_MIDDLE: &str = "; cc_entrypoint=cli; cch=";

/// Validate subscription billing bytes and return the remaining model input.
///
/// Only the verified first billing block is removed. Direct message cache
/// breakpoints with a one-hour TTL become ordinary ephemeral breakpoints so
/// [`crate::cache_contract`] can compare their placement. Instructions, schemas,
/// tool arguments, and all other message content remain intact.
///
/// # Errors
/// Returns an explanation for malformed JSON, unexpected billing data, an
/// incorrect fingerprint/checksum.
pub fn claude_model_input(raw: &[u8]) -> Result<Value, String> {
    let mut request: Value = serde_json::from_slice(raw).map_err(|error| error.to_string())?;
    request.as_object().ok_or("request must be an object")?;
    let first = request["system"]
        .as_array()
        .and_then(|blocks| blocks.first())
        .and_then(Value::as_object)
        .ok_or("system must begin with a billing text block")?;
    if first.len() != 2 || first.get("type") != Some(&json!("text")) {
        return Err("billing block must contain only type and text".into());
    }
    let billing = first["text"].as_str().ok_or("billing text is missing")?;
    let rest = billing
        .strip_prefix(BILLING_START)
        .ok_or("unexpected billing version")?;
    let (suffix, checksum) = rest
        .split_once(BILLING_MIDDLE)
        .ok_or("invalid billing format")?;
    let checksum = checksum.strip_suffix(';').ok_or("invalid billing ending")?;
    if !lower_hex(suffix, 3) || !lower_hex(checksum, 5) {
        return Err("billing fingerprint/checksum must be lowercase hexadecimal".into());
    }
    if suffix != version_suffix(first_user_text(&request)) {
        return Err("billing version fingerprint does not match the first user text".into());
    }

    let system_range = member_range(raw, "system")?;
    let system = &raw[system_range.clone()];
    let first_start = skip_space(system, 1);
    let first_end = value_end(system, first_start)?;
    let block_start = system_range.start + first_start;
    let text_range = member_range(&system[first_start..first_end], "text")?;
    let text_start = block_start + text_range.start + 1;
    let text_end = block_start + text_range.end - 1;
    if raw.get(text_start..text_end) != Some(billing.as_bytes()) {
        return Err("billing text must be an unescaped wire string".into());
    }
    let checksum_start = text_end - 6;
    let mut placeholder = raw.to_vec();
    placeholder[checksum_start..checksum_start + 5].copy_from_slice(b"00000");
    let expected = format!(
        "{:05x}",
        xxh64(&checksum_input(&placeholder)?, 0x4d65_9218_e32a_3268) & 0xfffff
    );
    if checksum != expected {
        return Err(format!("billing checksum mismatch: expected {expected}"));
    }

    request["system"].as_array_mut().unwrap().remove(0);
    if let Some(messages) = request["messages"].as_array_mut() {
        for message in messages {
            if let Some(blocks) = message["content"].as_array_mut() {
                for block in blocks {
                    if block.get("cache_control")
                        == Some(&json!({"type": "ephemeral", "ttl": "1h"}))
                    {
                        block["cache_control"] = json!({"type": "ephemeral"});
                    }
                }
            }
        }
    }
    Ok(request)
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn first_user_text(request: &Value) -> &str {
    let Some(user) = request["messages"]
        .as_array()
        .and_then(|messages| messages.iter().find(|message| message["role"] == "user"))
    else {
        return "";
    };
    if let Some(text) = user["content"].as_str() {
        return text;
    }
    user["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find(|block| block["type"] == "text"))
        .and_then(|block| block["text"].as_str())
        .unwrap_or("")
}

fn version_suffix(text: &str) -> String {
    let units: Vec<_> = text.encode_utf16().collect();
    let selected: Vec<_> = [4, 7, 20]
        .into_iter()
        .map(|index| units.get(index).copied().unwrap_or(u16::from(b'0')))
        .collect();
    let sample = String::from_utf16_lossy(&selected);
    let digest = Sha256::digest(format!("59cf53e54c78{sample}2.1.295"));
    format!("{digest:x}")[..3].to_owned()
}

// Match native byte exclusions, including its literal (not JSON-unescaped)
// model scanning. Do not alter the captured request itself.
fn checksum_input(raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut output = Vec::with_capacity(raw.len());
    let mut cursor = 0;
    while cursor < raw.len() {
        let remaining = &raw[cursor..];
        if remaining.starts_with(b"\"model\":\"") {
            let value_start = cursor + 9;
            let value_end = raw[value_start..]
                .iter()
                .position(|byte| *byte == b'"')
                .ok_or("unterminated model")?
                + value_start;
            output.extend_from_slice(&raw[cursor..value_start]);
            cursor = value_end;
            continue;
        }
        let removed_end = if remaining.starts_with(b"\"max_tokens\":") {
            let number_start = cursor + 13;
            let digits = raw[number_start..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            (digits > 0).then_some(number_start + digits)
        } else if remaining.starts_with(b"\"fallbacks\":[") {
            Some(value_end(raw, cursor + 12)?)
        } else if remaining.starts_with(b"\"fallback_credit_token\":\"") {
            let value_start = cursor + 25;
            Some(
                raw[value_start..]
                    .iter()
                    .position(|byte| *byte == b'"')
                    .ok_or("unterminated fallback credit token")?
                    + value_start
                    + 1,
            )
        } else {
            None
        };
        if let Some(end) = removed_end {
            if raw.get(end) == Some(&b',') {
                cursor = end + 1;
            } else {
                if output.last() == Some(&b',') {
                    output.pop();
                }
                cursor = end;
            }
        } else {
            output.push(raw[cursor]);
            cursor += 1;
        }
    }
    Ok(output)
}

fn skip_space(raw: &[u8], mut index: usize) -> usize {
    while raw.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

fn string_end(raw: &[u8], start: usize) -> Result<usize, String> {
    let mut index = start + 1;
    while let Some(byte) = raw.get(index) {
        match byte {
            b'\\' => index += 2,
            b'"' => return Ok(index + 1),
            _ => index += 1,
        }
    }
    Err("unterminated JSON string".into())
}

fn value_end(raw: &[u8], start: usize) -> Result<usize, String> {
    if raw.get(start) == Some(&b'"') {
        return string_end(raw, start);
    }
    let mut depth = 0usize;
    let mut index = start;
    while let Some(byte) = raw.get(index) {
        match byte {
            b'"' => index = string_end(raw, index)?,
            b'[' | b'{' => {
                depth += 1;
                index += 1;
            }
            b']' | b'}' if depth > 0 => {
                depth -= 1;
                index += 1;
                if depth == 0 {
                    return Ok(index);
                }
            }
            b',' | b']' | b'}' if depth == 0 => return Ok(index),
            _ => index += 1,
        }
    }
    Err("unterminated JSON value".into())
}

fn member_range(raw: &[u8], name: &str) -> Result<Range<usize>, String> {
    let mut index = skip_space(raw, 0) + 1;
    let mut found = None;
    while raw.get(skip_space(raw, index)) != Some(&b'}') {
        index = skip_space(raw, index);
        let key_end = string_end(raw, index)?;
        let key: String =
            serde_json::from_slice(&raw[index..key_end]).map_err(|error| error.to_string())?;
        index = skip_space(raw, key_end) + 1;
        let start = skip_space(raw, index);
        let end = value_end(raw, start)?;
        if key == name && found.replace(start..end).is_some() {
            return Err(format!("duplicate {name} member"));
        }
        index = skip_space(raw, end);
        if raw.get(index) == Some(&b',') {
            index += 1;
        }
    }
    found.ok_or_else(|| format!("missing {name} member"))
}

#[cfg(test)]
mod tests;
