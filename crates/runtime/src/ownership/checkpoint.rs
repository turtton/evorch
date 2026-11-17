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
            row.runs.insert(
                run.clone(),
                RunCheckpoint {
                    snapshot,
                    boundary,
                    checkpointed_at_ms: super::permit::now_ms(),
                },
            );
        }
        row.prune_terminal(permit.run_id.as_deref());
        row
    }

    /// A non-terminal checkpoint of this generation proves its run is parked.
    /// Only that run's own terminal checkpoint replaces it, and a new
    /// generation starts from an empty row. Terminal records prove nothing, so
    /// one stays as an advisory reference (the written run, else the latest).
    fn prune_terminal(&mut self, written: Option<&str>) {
        let is_terminal = |saved: &RunCheckpoint| saved.boundary == CheckpointBoundary::Terminal;
        let keep = written
            .filter(|run| self.runs.get(*run).is_some_and(is_terminal))
            .map(str::to_owned)
            .or_else(|| {
                self.runs
                    .iter()
                    .filter(|(_, saved)| is_terminal(saved))
                    .max_by_key(|(id, saved)| (saved.checkpointed_at_ms, id.as_str()))
                    .map(|(id, _)| id.clone())
            });
        self.runs
            .retain(|id, saved| !is_terminal(saved) || Some(id) == keep.as_ref());
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
