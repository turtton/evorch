//! Conservative proofs for pre-sidecar snapshots, shared by discovery and W6.
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::*;
use crate::{RunContextRecord, StorageError, threads::ThreadLineageKind};

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyProvenanceReason {
    ContinuityMissing,
    ContinuityBoundaryUnverifiable,
    ForkBoundaryUnverifiable,
    LineageUnverifiable,
    InvalidSnapshot,
}

/// Derive positions only from persisted generation metadata. Never searches
/// message text, modifies the connection, or promotes a non-legacy sidecar.
///
/// # Errors
/// SQLite failures are returned separately from a structurally unprovable run.
pub fn derive_legacy_provenance(
    conn: &Connection,
    record: &RunContextRecord,
) -> Result<Result<SnapshotProvenance, LegacyProvenanceReason>, StorageError> {
    derive(conn, record, &mut Vec::new())
}

fn derive(
    conn: &Connection,
    record: &RunContextRecord,
    visiting: &mut Vec<String>,
) -> Result<Result<SnapshotProvenance, LegacyProvenanceReason>, StorageError> {
    use LegacyProvenanceReason::*;
    if visiting.len() >= crate::threads::MAX_RUN_LINKS as usize || visiting.contains(&record.run_id)
    {
        return Ok(Err(ForkBoundaryUnverifiable));
    }
    visiting.push(record.run_id.clone());
    let result = derive_inner(conn, record, visiting);
    visiting.pop();
    result
}

fn derive_inner(
    conn: &Connection,
    record: &RunContextRecord,
    visiting: &mut Vec<String>,
) -> Result<Result<SnapshotProvenance, LegacyProvenanceReason>, StorageError> {
    use LegacyProvenanceReason::*;
    let Ok(config) = serde_json::from_str::<Value>(&record.config_json) else {
        return Ok(Err(InvalidSnapshot));
    };
    let Some(continuity) = config.get("continuity") else {
        return Ok(Err(ContinuityMissing));
    };
    if !continuity.is_object()
        || continuity.get("inherited_note") != Some(&Value::Null)
        || continuity
            .get("sources")
            .and_then(Value::as_array)
            .is_none_or(|sources| !sources.is_empty())
    {
        return Ok(Err(ContinuityBoundaryUnverifiable));
    }
    let (Ok(messages), Ok(checkpoints)) = (
        serde_json::from_str::<Vec<Value>>(&record.messages_json),
        serde_json::from_str::<Vec<Value>>(&record.checkpoints_json),
    ) else {
        return Ok(Err(InvalidSnapshot));
    };
    if messages.iter().any(|message| {
        !matches!(
            message["role"].as_str(),
            Some("system" | "user" | "assistant")
        ) || !message["content"].is_array()
    }) || checkpoints.iter().any(|checkpoint| {
        let range = checkpoint["range"].as_array();
        !range.is_some_and(|range| {
            range.len() == 2
                && range[0]
                    .as_u64()
                    .zip(range[1].as_u64())
                    .is_some_and(|(start, end)| start < end && end <= messages.len() as u64)
        })
    }) {
        return Ok(Err(InvalidSnapshot));
    }
    let binding: Option<(String, String, u32)> = conn
        .query_row(
            "SELECT thread_id, root_run_id, depth FROM thread_runs WHERE run_id = ?1",
            [&record.run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((thread_id, root_run_id, depth)) = binding else {
        return Ok(Err(LineageUnverifiable));
    };
    let Some(thread) = crate::repo::threads::get(conn, &thread_id)? else {
        return Ok(Err(LineageUnverifiable));
    };
    if thread.root_run_id.as_deref() != Some(root_run_id.as_str())
        || config
            .get("thread_id")
            .and_then(Value::as_str)
            .is_some_and(|id| id != thread_id)
        || config.get("parent_run_id")
            != Some(
                &record
                    .parent_run_id
                    .clone()
                    .map_or(Value::Null, Value::String),
            )
        || (depth == 0) != (record.run_id == root_run_id)
        || (depth == 0) != record.parent_run_id.is_none()
    {
        return Ok(Err(LineageUnverifiable));
    }
    // Validate ancestry, rather than trusting a single backfilled binding.
    let mut parent = record.parent_run_id.clone();
    let mut expected_depth = depth;
    while let Some(id) = parent {
        if expected_depth == 0 || expected_depth > crate::threads::MAX_RUN_LINKS {
            return Ok(Err(LineageUnverifiable));
        }
        expected_depth -= 1;
        let row: Option<(Option<String>, String, String, u32)> = conn
            .query_row(
                "SELECT c.parent_run_id, t.thread_id, t.root_run_id, t.depth \
             FROM run_contexts c JOIN thread_runs t ON t.run_id=c.run_id WHERE c.run_id=?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((next, ancestor_thread, ancestor_root, ancestor_depth)) = row else {
            return Ok(Err(LineageUnverifiable));
        };
        if ancestor_thread != thread_id
            || ancestor_root != root_run_id
            || ancestor_depth != expected_depth
            || (expected_depth == 0 && id != root_run_id)
        {
            return Ok(Err(LineageUnverifiable));
        }
        parent = next;
    }
    if expected_depth != 0 {
        return Ok(Err(LineageUnverifiable));
    }
    let mut map = ProvenanceMap {
        version: PROVENANCE_FORMAT_VERSION,
        raw_message_count: messages.len(),
        checkpoint_count: checkpoints.len(),
        messages: messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message["role"] != "system")
            .enumerate()
            .map(|(ordinal, (raw_index, message))| MessageProvenance {
                raw_index,
                ordinal,
                blocks: (0..message["content"].as_array().map_or(0, Vec::len))
                    .map(|block_index| BlockProvenance {
                        block_index,
                        origin: BlockOrigin::Native,
                        excluded_reason: None,
                    })
                    .collect(),
            })
            .collect(),
    };
    let fork = thread.fork_point_run_id.is_some()
        || thread.fork_point_context_len.is_some()
        || matches!(
            thread.lineage_kind,
            Some(ThreadLineageKind::Fork | ThreadLineageKind::Rewind)
        );
    if depth == 0 && !fork {
        let inherited: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM review_lineage_edges WHERE child_run_id=?1)",
            [&record.run_id],
            |row| row.get(0),
        )?;
        // Same-thread followups can start a fresh run by copying full history
        // without a fork boundary. Empty continuity does not prove that prefix.
        if inherited {
            return Ok(Err(LineageUnverifiable));
        }
    }
    if depth == 0 && fork {
        let (Some(source_id), Some(context_len)) =
            (&thread.fork_point_run_id, thread.fork_point_context_len)
        else {
            return Ok(Err(ForkBoundaryUnverifiable));
        };
        let Ok(context_len) = usize::try_from(context_len) else {
            return Ok(Err(ForkBoundaryUnverifiable));
        };
        let Some(last) = context_len
            .checked_sub(1)
            .and_then(|index| map.messages.get(index))
        else {
            return Ok(Err(ForkBoundaryUnverifiable));
        };
        let last = &messages[last.raw_index];
        if last["role"] != "assistant"
            || last["content"]
                .as_array()
                .is_none_or(|blocks| blocks.iter().any(|block| block["type"] == "tool_use"))
        {
            return Ok(Err(ForkBoundaryUnverifiable));
        }
        let Some(source_record) = crate::repo::run_context::get(conn, source_id)? else {
            return Ok(Err(ForkBoundaryUnverifiable));
        };
        let Ok(source) = derive(conn, &source_record, visiting)? else {
            return Ok(Err(ForkBoundaryUnverifiable));
        };
        let Ok(source_messages) = serde_json::from_str::<Vec<Value>>(&source_record.messages_json)
        else {
            return Ok(Err(ForkBoundaryUnverifiable));
        };
        for message in map.messages.iter_mut().take(context_len) {
            let Some(original) = source.map.messages.get(message.ordinal) else {
                return Ok(Err(ForkBoundaryUnverifiable));
            };
            if original.blocks.len() != message.blocks.len() {
                return Ok(Err(ForkBoundaryUnverifiable));
            }
            if source_messages[original.raw_index]["role"] != messages[message.raw_index]["role"]
                || original.blocks.iter().any(|block| {
                    source_messages[original.raw_index]["content"][block.block_index]["type"]
                        != messages[message.raw_index]["content"][block.block_index]["type"]
                })
            {
                return Ok(Err(ForkBoundaryUnverifiable));
            }
            for (block, original) in message.blocks.iter_mut().zip(&original.blocks) {
                block.origin = BlockOrigin::ForkCopy {
                    source: CopySource::Message {
                        run_id: source_id.clone(),
                        snapshot_revision: 0,
                        ordinal: message.ordinal,
                        block_index: block.block_index,
                    },
                    original_origin: Box::new(original.origin.clone()),
                    boundary: ForkBoundary {
                        run_id: source_id.clone(),
                        snapshot_revision: 0,
                        context_len,
                    },
                };
                block.excluded_reason = original.excluded_reason;
            }
        }
    } else if depth == 0
        && (thread.parent_thread_id.is_some()
            || thread.escalation_source_run_id.is_some()
            || thread.lineage_kind.is_some())
    {
        // Escalation must carry nonempty continuity. An empty descriptor does
        // not prove its inherited reference boundary.
        return Ok(Err(LineageUnverifiable));
    }
    Ok(Ok(SnapshotProvenance {
        run_id: record.run_id.clone(),
        snapshot_revision: 0,
        snapshot_sha256: snapshot_sha256(&record.messages_json, &record.checkpoints_json),
        format_version: PROVENANCE_FORMAT_VERSION,
        status: ProvenanceStatus::Verified {
            evidence: ProvenanceEvidence::StructuredGenerationRecords {
                record_ids: vec![
                    format!("run_contexts:{}:continuity", record.run_id),
                    format!("thread_runs:{}", record.run_id),
                    format!("threads:{thread_id}"),
                ],
            },
        },
        map,
    }))
}
