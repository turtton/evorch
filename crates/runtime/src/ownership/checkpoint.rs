//! Ownership checkpoints contain references, never conversation bodies (ADR 0037).
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use crate::checkpoint::{
    CheckpointBoundary, RunCheckpoint, SnapshotNoneReason, SnapshotReference,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerCheckpoint {
    pub v: u32,
    pub generation: u64,
    pub boundary: CheckpointBoundary,
    pub runs: BTreeMap<String, RunCheckpoint>,
}

impl OwnerCheckpoint {
    /// Arrays (including legacy transcripts), corrupt objects and unknown versions
    /// carry no usable reference. They must not prevent claim/handoff/release.
    pub fn decode(payload: &str) -> Option<Self> {
        if !payload.trim_start().starts_with('{') {
            return None;
        }
        serde_json::from_str::<Self>(payload)
            .ok()
            .filter(|row| row.v == 1)
    }

    pub(super) fn advance(
        previous: Option<&str>,
        owner: &super::ThreadOwner,
        permit: &super::OwnerPermit,
        boundary: CheckpointBoundary,
        snapshot: SnapshotReference,
    ) -> Self {
        let mut row = previous
            .and_then(Self::decode)
            .filter(|row| row.generation == owner.lease.generation)
            .unwrap_or_else(|| Self {
                v: 1,
                generation: owner.lease.generation,
                boundary,
                runs: BTreeMap::new(),
            });
        row.boundary = boundary;
        if let Some(run) = &permit.run_id {
            row.runs
                .retain(|id, _| owner.active_runs.contains(id) || id == run);
            row.runs.insert(
                run.clone(),
                RunCheckpoint {
                    snapshot,
                    boundary,
                    checkpointed_at_ms: super::permit::now_ms(),
                },
            );
        } else {
            // Idle is valid only after all runs have drained. Keep at most the
            // most recently completed run as an advisory reference.
            let latest = row
                .runs
                .iter()
                .max_by_key(|(_, run)| run.checkpointed_at_ms)
                .map(|(id, _)| id.clone());
            row.runs.retain(|id, _| Some(id) == latest.as_ref());
        }
        row
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct CheckpointInput {
    pub boundary: CheckpointBoundary,
    pub snapshot: SnapshotReference,
}

impl Default for CheckpointInput {
    fn default() -> Self {
        Self {
            boundary: CheckpointBoundary::Idle,
            snapshot: SnapshotReference::none(SnapshotNoneReason::Skipped),
        }
    }
}
