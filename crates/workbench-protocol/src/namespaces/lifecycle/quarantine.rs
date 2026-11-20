//! Quarantine leaf contract. Display never implies resume.
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
pub enum QuarantineCommand {
    Discard(ConfirmedGraph),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum QuarantineQuery {
    List,
    Detail { graph_id: AdminId },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantineItem {
    pub graph_id: AdminId,
    pub revision: Revision,
    pub cold: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum QuarantineReply {
    List {
        items: Vec<QuarantineItem>,
        revision: Revision,
        truncated: bool,
    },
    Detail(QuarantineItem),
}
impl QuarantineReply {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Self::List { items: rows, .. } = self {
            items(rows)?;
        }
        bounded(self)
    }
}
