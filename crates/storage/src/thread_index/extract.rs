use std::collections::HashMap;

use secret_guard::SecretRedactor;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::*;

/// Hash the exact redacted saved bytes, each preceded by its u64 big-endian byte
/// length, messages first then checkpoints. Whitespace changes the identity.
pub fn snapshot_sha256(messages_json: &str, checkpoints_json: &str) -> String {
    let mut hash = Sha256::new();
    for json in [messages_json, checkpoints_json] {
        hash.update((json.len() as u64).to_be_bytes());
        hash.update(json.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

/// Parse errors are run-level holds, without serde diagnostics/source leakage.
pub fn parse_index_snapshot(json: &str) -> Result<Value, ExtractionError> {
    serde_json::from_str(json).map_err(|_| ExtractionError::InvalidJson)
}

/// Convenience for jobs/measurement reading an atomic bundle. The Value-based
/// entry point remains provider-independent and accepts typed separate metadata.
pub fn extract_snapshot_bundle(
    bundle: &SnapshotBundle,
    redactor: &SecretRedactor,
) -> Result<ExtractedIndexInput, ExtractionError> {
    let messages = parse_index_snapshot(&bundle.record.messages_json)?;
    let checkpoints = parse_index_snapshot(&bundle.record.checkpoints_json)?;
    let checkpoint_count = checkpoints
        .as_array()
        .ok_or(ExtractionError::InvalidJson)?
        .len();
    let hash = snapshot_sha256(
        &bundle.record.messages_json,
        &bundle.record.checkpoints_json,
    );
    extract_index_input(
        IndexInput {
            run_id: &bundle.record.run_id,
            snapshot_revision: bundle.snapshot_revision,
            source_revision: bundle.source_revision,
            snapshot_sha256: &hash,
            messages: &messages,
            checkpoint_count,
            provenance: bundle.provenance.as_ref(),
            latest_note: bundle.latest_note.as_ref(),
        },
        redactor,
    )
}

/// Never manufacture native provenance. Validate the entire map, then filter
/// copies/hidden content without renumbering. Any malformed known content aborts
/// the whole run; partial blocks from that run are not returned.
pub fn extract_index_input(
    input: IndexInput<'_>,
    redactor: &SecretRedactor,
) -> Result<ExtractedIndexInput, ExtractionError> {
    let messages = input
        .messages
        .as_array()
        .ok_or(ExtractionError::InvalidJson)?;
    validate_messages(messages)?;
    let provenance = validate_provenance(&input, messages)?;
    let tools = tool_names(messages);
    let mut blocks = Vec::new();
    for mapped in &provenance.map.messages {
        let message = &messages[mapped.raw_index];
        let role = match message["role"].as_str() {
            Some("user") => IndexRole::User,
            Some("assistant") => IndexRole::Assistant,
            _ => return Err(ExtractionError::InvalidJson),
        };
        let content = message["content"]
            .as_array()
            .ok_or(ExtractionError::InvalidJson)?;
        for source in &mapped.blocks {
            if source.origin != BlockOrigin::Native || source.excluded_reason.is_some() {
                continue;
            }
            if let Some(block) = extract_block(
                &content[source.block_index],
                BlockLocator::Message {
                    ordinal: mapped.ordinal,
                    block_index: source.block_index,
                },
                role,
                &tools,
                redactor,
            ) {
                blocks.push(block);
            }
        }
    }
    if let Some(note) = input.latest_note {
        if note.run_id != input.run_id {
            return Err(ExtractionError::NoteRunMismatch);
        }
        if !note.body.is_empty() {
            let mut body = Value::String(note.body.clone());
            redactor.redact_json(&mut body);
            blocks.push(finish_block(
                BlockLocator::WorkingNote { note_seq: note.seq },
                IndexRole::WorkingNote,
                IndexBlockKind::WorkingNote,
                None,
                body.as_str().unwrap_or_default().to_owned(),
                redactor,
            ));
        }
    }
    Ok(ExtractedIndexInput {
        run_id: input.run_id.to_owned(),
        snapshot_revision: input.snapshot_revision,
        source_revision: input.source_revision,
        extractor_version: EXTRACTOR_VERSION,
        text_bytes: blocks.iter().map(|block| block.text.len() as u64).sum(),
        blocks,
    })
}

/// Save-time structural check of runtime-supplied positions against the exact
/// redacted snapshot; the same rules extraction applies later.
pub(crate) fn validate_saved_provenance(
    provenance: &SnapshotProvenance,
    messages: &Value,
    checkpoint_count: usize,
) -> Result<(), ExtractionError> {
    let array = messages.as_array().ok_or(ExtractionError::InvalidJson)?;
    let input = IndexInput {
        run_id: &provenance.run_id,
        snapshot_revision: provenance.snapshot_revision,
        source_revision: 0,
        snapshot_sha256: &provenance.snapshot_sha256,
        messages,
        checkpoint_count,
        provenance: Some(provenance),
        latest_note: None,
    };
    validate_provenance(&input, array).map(drop)
}

fn unknown(reason: ProvenanceUnknownReason) -> ExtractionError {
    ExtractionError::ProvenanceUnknown(reason)
}

fn validate_provenance<'a>(
    input: &IndexInput<'a>,
    messages: &[Value],
) -> Result<&'a SnapshotProvenance, ExtractionError> {
    use ProvenanceUnknownReason::*;
    let provenance = input.provenance.ok_or(unknown(MissingSidecar))?;
    if provenance.format_version != PROVENANCE_FORMAT_VERSION
        || provenance.map.version != PROVENANCE_FORMAT_VERSION
    {
        return Err(unknown(UnsupportedVersion));
    }
    if provenance.run_id != input.run_id
        || provenance.snapshot_revision != input.snapshot_revision
        || provenance.snapshot_sha256 != input.snapshot_sha256
    {
        return Err(unknown(SnapshotMismatch));
    }
    if let ProvenanceStatus::Unknown { reason } = provenance.status {
        return Err(unknown(reason));
    }
    if provenance.map.raw_message_count != messages.len()
        || provenance.map.checkpoint_count != input.checkpoint_count
    {
        return Err(unknown(IncompleteMap));
    }
    let mut ordinal = 0;
    for (raw_index, message) in messages.iter().enumerate() {
        if message["role"] == "system" {
            continue;
        }
        let mapped = provenance
            .map
            .messages
            .get(ordinal)
            .ok_or(unknown(IncompleteMap))?;
        let content = message["content"]
            .as_array()
            .ok_or(ExtractionError::InvalidJson)?;
        if mapped.raw_index != raw_index
            || mapped.ordinal != ordinal
            || mapped.blocks.len() != content.len()
            || mapped
                .blocks
                .iter()
                .enumerate()
                .any(|(index, block)| block.block_index != index)
        {
            return Err(unknown(IncompleteMap));
        }
        ordinal += 1;
    }
    if provenance.map.messages.len() != ordinal {
        return Err(unknown(IncompleteMap));
    }
    Ok(provenance)
}

fn validate_messages(messages: &[Value]) -> Result<(), ExtractionError> {
    let invalid = ExtractionError::InvalidJson;
    for message in messages {
        if !matches!(
            message["role"].as_str(),
            Some("system" | "user" | "assistant")
        ) {
            return Err(invalid);
        }
        for block in message["content"].as_array().ok_or(invalid)? {
            match block["type"].as_str().ok_or(invalid)? {
                "text" if !block["text"].is_string() => return Err(invalid),
                "tool_use" => {
                    if !block["id"].is_string()
                        || !block["name"].is_string()
                        || block.get("input").is_none()
                    {
                        return Err(invalid);
                    }
                }
                "tool_result" => {
                    if !block["tool_call_id"].is_string() || !block["is_error"].is_boolean() {
                        return Err(invalid);
                    }
                    for part in block["content"].as_array().ok_or(invalid)? {
                        let kind = part["type"].as_str().ok_or(invalid)?;
                        if kind == "text" && !part["text"].is_string() {
                            return Err(invalid);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn tool_names(messages: &[Value]) -> HashMap<&str, Option<&str>> {
    let mut tools = HashMap::new();
    for message in messages.iter().filter(|m| m["role"] != "system") {
        for block in message["content"].as_array().into_iter().flatten() {
            if block["type"] == "tool_use"
                && let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str())
            {
                tools
                    .entry(id)
                    .and_modify(|old| {
                        if *old != Some(name) {
                            *old = None;
                        }
                    })
                    .or_insert(Some(name));
            }
        }
    }
    tools
}

fn extract_block(
    raw: &Value,
    locator: BlockLocator,
    role: IndexRole,
    tools: &HashMap<&str, Option<&str>>,
    redactor: &SecretRedactor,
) -> Option<IndexBlock> {
    // Do not copy/redact opaque image or replay data, even temporarily.
    let (kind, mut structured, tool_name) = match raw["type"].as_str()? {
        "text" => (IndexBlockKind::Text, raw["text"].clone(), None),
        "tool_use" => (
            IndexBlockKind::ToolUse,
            raw["input"].clone(),
            raw["name"].as_str(),
        ),
        "tool_result" => {
            let texts: Vec<_> = raw["content"]
                .as_array()?
                .iter()
                .filter(|part| part["type"] == "text")
                .map(|part| part["text"].clone())
                .collect();
            if texts.is_empty() {
                return None;
            }
            (
                if raw["is_error"] == true {
                    IndexBlockKind::ToolError
                } else {
                    IndexBlockKind::ToolResult
                },
                Value::Array(texts),
                raw["tool_call_id"]
                    .as_str()
                    .and_then(|id| tools.get(id).copied().flatten()),
            )
        }
        _ => return None,
    };
    redactor.redact_json(&mut structured);
    let text = match kind {
        IndexBlockKind::ToolUse => structured.to_string(),
        IndexBlockKind::ToolResult | IndexBlockKind::ToolError => structured
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => structured.as_str()?.to_owned(),
    };
    Some(finish_block(locator, role, kind, tool_name, text, redactor))
}

fn finish_block(
    locator: BlockLocator,
    role: IndexRole,
    kind: IndexBlockKind,
    tool_name: Option<&str>,
    text: String,
    redactor: &SecretRedactor,
) -> IndexBlock {
    let mut text = redactor.redact(&text).text;
    let limit = match kind {
        IndexBlockKind::ToolUse => TOOL_USE_MAX_BYTES,
        IndexBlockKind::ToolResult => TOOL_RESULT_MAX_BYTES,
        _ => usize::MAX,
    };
    let truncated = text.len() > limit;
    if truncated {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
    IndexBlock {
        locator,
        role,
        kind,
        tool_name: tool_name.map(|name| redactor.redact(name).text),
        text,
        truncated,
        hash,
    }
}

#[cfg(test)]
mod tests;
