//! ToolsWaiting is not history-only recovery and never re-dispatches a tool.
#[cfg(test)]
mod tests;
use super::{RestoredState, RunRestoreDescriptor};
use crate::checkpoint::CheckpointBoundary;
use crate::quiesce::{DurableWait, DurableWaitMode, WaitKind};
use providers::{ContentBlock, Message};
use std::collections::{HashMap, HashSet};

pub(crate) use crate::meta::wait_contract as wait_targets;

/// Validate the entire unfinished batch, not merely a matching call ID.
pub(crate) fn validate_wait_messages(
    run: crate::RunId,
    messages: &[Message],
    waits: &[DurableWait],
) -> Result<(), String> {
    let mut calls = Vec::new();
    let mut all_ids = HashSet::new();
    let mut results = HashMap::new();
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, name, input } => {
                    if !all_ids.insert(id.as_str()) {
                        return Err("duplicate tool call ID".into());
                    }
                    calls.push((id.as_str(), name.as_str(), input));
                }
                ContentBlock::ToolResult { tool_call_id, .. }
                    if calls
                        .get(results.len())
                        .is_none_or(|call| call.0 != tool_call_id)
                        || results.insert(tool_call_id.as_str(), ()).is_some() =>
                {
                    return Err("orphan/duplicate result".into());
                }
                _ => {}
            }
        }
    }
    let pending: Vec<_> = calls
        .iter()
        .enumerate()
        .filter(|(_, (id, _, _))| !results.contains_key(id))
        .collect();
    if pending.len() != waits.len() {
        return Err("unfinished tool-use set differs from waits".into());
    }
    let mut seen = HashSet::new();
    for (wait, (position, (id, name, input))) in waits.iter().zip(pending) {
        let payload = wait
            .continuation
            .payload
            .as_ref()
            .ok_or("missing wait continuation payload")?;
        if wait.requester != run
            || wait.tool_call_id != *id
            || wait.continuation.tool_name != *name
            || payload.position != position
            || &payload.input != *input
            || !seen.insert(id)
        {
            return Err("wait call identity/input/order mismatch".into());
        }
        for generations in [&wait.completions, &wait.questions, &wait.answers] {
            let unique: HashSet<_> = generations.iter().map(|g| &g.id).collect();
            if unique.len() != generations.len() {
                return Err("duplicate observation".into());
            }
        }
        match &payload.kind {
            WaitKind::Runs => {
                let (targets, mode, legacy) = wait_targets(input)?;
                if *name != "wait"
                    || wait.targets != targets
                    || wait.mode != mode
                    || wait.continuation.legacy_result != legacy
                    || !wait.continuation.consume_mailbox
                {
                    return Err("run wait contract mismatch".into());
                }
            }
            WaitKind::Delegates { calls: wave } => {
                let targets: Vec<_> = wave.iter().map(|call| call.child).collect();
                let ids: HashSet<_> = wave.iter().map(|call| &call.tool_call_id).collect();
                if *name != "delegate"
                    || input.get("background").is_some_and(|v| v != false)
                    || wave.is_empty()
                    || ids.len() != wave.len()
                    || targets.iter().collect::<HashSet<_>>().len() != targets.len()
                    || !ids.contains(&wait.tool_call_id)
                    || wait.targets != targets
                    || wait.mode != DurableWaitMode::All
                    || wait.continuation.legacy_result
                    || wait.continuation.consume_mailbox
                {
                    return Err("delegate projection mismatch".into());
                }
                let wave_waits: Vec<_> = waits
                    .iter()
                    .filter(|w| wave.iter().any(|c| c.tool_call_id == w.tool_call_id))
                    .collect();
                if wave_waits.len() != wave.len()
                    || wave_waits.iter().zip(wave).any(|(w, c)| {
                        w.tool_call_id != c.tool_call_id
                            || w.continuation
                                .payload
                                .as_ref()
                                .is_none_or(|p| p.kind != payload.kind)
                    })
                {
                    return Err("incomplete delegate wave".into());
                }
            }
            WaitKind::Reply {
                message_id,
                deadline_unix_ms,
                delivered_answer_ids,
            } => {
                if *name != "wait_reply"
                    || input.get("message_id").and_then(|v| v.as_str()) != Some(message_id)
                    || input.get("timeout_ms").and_then(|v| v.as_u64()).is_none()
                    || *deadline_unix_ms == 0
                    || wait.targets.len() != 1
                    || wait.mode != DurableWaitMode::Any
                    || wait.continuation.legacy_result
                    || !wait.continuation.consume_mailbox
                    || delivered_answer_ids.iter().collect::<HashSet<_>>().len()
                        != delivered_answer_ids.len()
                    || delivered_answer_ids
                        .iter()
                        .map(String::as_str)
                        .collect::<HashSet<_>>()
                        != wait
                            .answers
                            .iter()
                            .map(|g| g.id.as_str())
                            .collect::<HashSet<_>>()
                {
                    return Err("reply contract mismatch".into());
                }
            }
        }
        if wait
            .completions
            .iter()
            .any(|g| !wait.targets.iter().any(|target| target.to_string() == g.id))
        {
            return Err("foreign completion observation".into());
        }
    }
    Ok(())
}

pub(crate) fn validate_recovery_waits(
    record: &storage::RunContextRecord,
    waits: &[DurableWait],
) -> Result<(), crate::RuntimeError> {
    let check = || -> Result<(), String> {
        let d: RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).map_err(|e| e.to_string())?;
        let messages: Vec<Message> =
            serde_json::from_str(&record.messages_json).map_err(|e| e.to_string())?;
        if (d.recovery_boundary == Some(CheckpointBoundary::ToolsWaiting)) != !waits.is_empty()
            || d.recovery_waits != waits
            || !d.interrupted_tool_calls.is_empty()
        {
            return Err("ToolsWaiting boundary/evidence mismatch or unconfirmed effects".into());
        }
        let run = record
            .run_id
            .parse::<crate::RunId>()
            .map_err(|e| e.to_string())?;
        validate_wait_messages(run, &messages, waits)
    };
    check().map_err(|reason| crate::RuntimeError::RunRestoreFailed {
        run_id: record.run_id.clone(),
        reason: crate::RunRestoreFailure::CorruptContext(reason),
    })
}

impl RestoredState {
    pub(crate) fn from_recovery_bundle(
        bundle: &storage::thread_index::SnapshotBundle,
        waits: &[DurableWait],
    ) -> Result<Self, crate::RuntimeError> {
        if !waits.is_empty() {
            super::validate_recovery_waits(&bundle.record, waits)?;
        }
        let mut restored = Self::from_record_inner(&bundle.record, !waits.is_empty())?;
        restored.provenance = super::provenance::ConversationProvenance::from_bundle(bundle);
        restored.snapshot_revision =
            super::revision::SnapshotRevision::known(bundle.snapshot_revision);
        Ok(restored)
    }
}
