//! Positional provenance of raw history. Never compare message text.
use providers::{Message, Role};
use storage::thread_index::{
    BlockOrigin, BlockProvenance, CopySource, ExcludedReason, ForkBoundary, MessageProvenance,
    PROVENANCE_FORMAT_VERSION, ProvenanceEvidence, ProvenanceMap, ProvenanceStatus,
    ProvenanceUnknownReason, SnapshotBundle, SnapshotWrite,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConversationProvenance {
    status: ProvenanceStatus,
    messages: Vec<MessageProvenance>,
}

impl Default for ConversationProvenance {
    fn default() -> Self {
        Self {
            status: ProvenanceStatus::Verified {
                evidence: ProvenanceEvidence::CompletePositionMap,
            },
            messages: Vec::new(),
        }
    }
}

impl ConversationProvenance {
    pub(crate) fn unknown() -> Self {
        Self {
            status: ProvenanceStatus::Unknown {
                reason: ProvenanceUnknownReason::LegacyUnproven,
            },
            messages: Vec::new(),
        }
    }

    pub(crate) fn from_bundle(bundle: &SnapshotBundle) -> Self {
        // The shared extractor validates hash, revision and all positions even
        // when the snapshot has no indexable native text.
        if storage::thread_index::extract_snapshot_bundle(
            bundle,
            &secret_guard::SecretRedactor::from_env(),
        )
        .is_err()
        {
            return Self::unknown();
        }
        let Some(sidecar) = &bundle.provenance else {
            return Self::unknown();
        };
        Self {
            status: sidecar.status.clone(),
            messages: sidecar.map.messages.clone(),
        }
    }

    pub(crate) fn fork(&mut self, source: &SnapshotBundle, context_len: usize) {
        self.messages.truncate(context_len);
        for message in &mut self.messages {
            for block in &mut message.blocks {
                block.origin = BlockOrigin::ForkCopy {
                    source: CopySource::Message {
                        run_id: source.record.run_id.clone(),
                        snapshot_revision: source.snapshot_revision,
                        ordinal: message.ordinal,
                        block_index: block.block_index,
                    },
                    original_origin: Box::new(block.origin.clone()),
                    boundary: ForkBoundary {
                        run_id: source.record.run_id.clone(),
                        snapshot_revision: source.snapshot_revision,
                        context_len,
                    },
                };
            }
        }
    }

    pub(crate) fn continuity(
        &mut self,
        messages: &[Message],
        continuity: &crate::continuity::Continuity,
    ) {
        let mut mapped = self.map(messages, 0);
        if let Some(message) = mapped.messages.last_mut() {
            for block in &mut message.blocks {
                // The first block and source catalog are generated guidance,
                // not a native utterance nor a copy of frozen evidence text.
                block.excluded_reason = Some(ExcludedReason::GeneratedBoundary);
            }
            if let Some(note) = &continuity.inherited_note
                && let Some(block) = message.blocks.get_mut(1)
            {
                block.origin = BlockOrigin::ContinuityCopy {
                    source: CopySource::WorkingNote {
                        run_id: note.writer_run_id.clone(),
                        note_seq: note.revision,
                    },
                };
                block.excluded_reason = None;
            }
        }
        self.messages = mapped.messages;
        if let ProvenanceStatus::Verified { evidence } = &mut self.status {
            let mut record_ids = match evidence {
                ProvenanceEvidence::StructuredGenerationRecords { record_ids } => {
                    record_ids.clone()
                }
                ProvenanceEvidence::CompletePositionMap => Vec::new(),
            };
            record_ids.extend(
                continuity
                    .sources
                    .iter()
                    .map(|source| source.source.clone()),
            );
            if let Some(note) = &continuity.inherited_note {
                record_ids.push(format!(
                    "run_ledger:{}:{}",
                    note.writer_run_id, note.revision
                ));
            }
            *evidence = ProvenanceEvidence::StructuredGenerationRecords { record_ids };
        }
    }

    pub(crate) fn generated_since(&mut self, messages: &[Message], raw_start: usize) {
        let mut mapped = self.map(messages, 0);
        for message in mapped
            .messages
            .iter_mut()
            .filter(|message| message.raw_index >= raw_start)
        {
            for block in &mut message.blocks {
                block.excluded_reason = Some(ExcludedReason::GeneratedBoundary);
            }
        }
        self.messages = mapped.messages;
    }

    fn map(&self, messages: &[Message], checkpoint_count: usize) -> ProvenanceMap {
        let mapped = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role != Role::System)
            .enumerate()
            .map(|(ordinal, (raw_index, message))| MessageProvenance {
                raw_index,
                ordinal,
                blocks: (0..message.content.len())
                    .map(|block_index| {
                        self.messages
                            .get(ordinal)
                            .and_then(|message| message.blocks.get(block_index))
                            .cloned()
                            .unwrap_or(BlockProvenance {
                                block_index,
                                origin: BlockOrigin::Native,
                                excluded_reason: None,
                            })
                    })
                    .collect(),
            })
            .collect();
        ProvenanceMap {
            version: PROVENANCE_FORMAT_VERSION,
            raw_message_count: messages.len(),
            checkpoint_count,
            messages: mapped,
        }
    }

    pub(crate) fn write(
        &self,
        record: storage::RunContextRecord,
    ) -> Result<SnapshotWrite, serde_json::Error> {
        let messages: Vec<Message> = serde_json::from_str(&record.messages_json)?;
        let checkpoints: Vec<serde_json::Value> = serde_json::from_str(&record.checkpoints_json)?;
        Ok(SnapshotWrite {
            map: self.map(&messages, checkpoints.len()),
            status: self.status.clone(),
            record,
            expected_snapshot_revision: None,
            binding: None,
        })
    }
}

#[cfg(test)]
mod tests;

pub(crate) fn storage_error(
    error: storage::thread_index::SnapshotApiError,
) -> storage::StorageError {
    match error {
        storage::thread_index::SnapshotApiError::Storage(error) => error,
        other => storage::StorageError::Serialization(format!("snapshot bundle: {other:?}")),
    }
}
