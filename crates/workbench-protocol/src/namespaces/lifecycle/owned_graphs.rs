//! Owned-graph leaf contract. Release and resume are distinct operations.
use super::contracts::*;
use crate::{validation::ValidationError, wire::Revision};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum OwnedGraphsCommand {
    Release(ConfirmedGraph),
    Resume(ConfirmedGraph),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum OwnedGraphsQuery {
    Undisposed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphReference {
    pub graph_id: AdminId,
    pub restart_tx_id: AdminId,
    pub capture_id: AdminId,
    pub revision: Revision,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UndisposedGraphs {
    pub count: Revision,
    pub references: Vec<GraphReference>,
    pub snapshot_revision: Revision,
    pub truncated: bool,
}
impl UndisposedGraphs {
    pub fn validate(&self) -> Result<(), ValidationError> {
        items(&self.references)?;
        if self.count.get() < self.references.len() as u64
            || self.truncated != (self.count.get() > self.references.len() as u64)
        {
            return Err(ValidationError::Inconsistent {
                field: "owned_graphs.count",
                reason: "count must match references and truncation",
            });
        }
        bounded(self)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum OwnedGraphsReply {
    Undisposed(UndisposedGraphs),
}
impl OwnedGraphsReply {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Undisposed(g) => g.validate(),
        }
    }
}
