//! Shared bounded management contracts. No credentials, paths or tool bodies.
use crate::{
    validation::{ValidationError, check_count, check_line},
    wire::{InstanceEpoch, Revision},
};
use serde::{Deserialize, Serialize};
pub const MAX_ADMIN_ITEMS: usize = 64;
pub const MAX_ADMIN_REPLY_BYTES: usize = 64 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminAction {
    Targets,
    Request,
    Cancel,
    Status,
    Interrupt,
    Quarantine,
    Discard,
    Release,
    Resume,
}
impl AdminAction {
    pub const ALL: [Self; 9] = [
        Self::Targets,
        Self::Request,
        Self::Cancel,
        Self::Status,
        Self::Interrupt,
        Self::Quarantine,
        Self::Discard,
        Self::Release,
        Self::Resume,
    ];
    pub fn irreversible(self) -> bool {
        matches!(
            self,
            Self::Request | Self::Interrupt | Self::Discard | Self::Release | Self::Resume
        )
    }
}
/// An opaque ID, not an executable path, argv, environment or free-form text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AdminId(String);
impl AdminId {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        check_line("admin.id", &value, 128, false)?;
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            || value == "."
            || value == ".."
        {
            return Err(ValidationError::InvalidChars { field: "admin.id" });
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for AdminId {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<AdminId> for String {
    fn from(id: AdminId) -> String {
        id.0
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphCas {
    pub graph_id: AdminId,
    pub restart_tx_id: AdminId,
    pub capture_id: AdminId,
    pub expected_revision: Revision,
    pub expected_core_epoch: InstanceEpoch,
    pub owner_identity: Option<AdminId>,
    pub owner_generation: Option<Revision>,
    pub operation_id: AdminId,
}
impl GraphCas {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.owner_identity.is_some() != self.owner_generation.is_some() {
            return Err(ValidationError::Inconsistent {
                field: "graph.owner",
                reason: "identity and generation must both be present or absent",
            });
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmedGraph {
    pub binding: GraphCas,
    pub confirmed: bool,
}
impl ConfirmedGraph {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.binding.validate()?;
        confirmation(self.confirmed)
    }
}
pub fn confirmation(confirmed: bool) -> Result<(), ValidationError> {
    if confirmed {
        Ok(())
    } else {
        Err(ValidationError::Inconsistent {
            field: "admin.confirmed",
            reason: "explicit confirmation is required",
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminOutcome {
    Pending,
    Succeeded,
    Failed,
    Cancelled,
    Conflict,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminReceipt {
    pub operation_id: AdminId,
    pub state: AdminOperationState,
    pub outcome: AdminOutcome,
    pub audit_warning: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminOperationState {
    Pending,
    Started,
    Completed,
    Unknown,
}
pub(crate) fn bounded<T: Serialize>(value: &T) -> Result<(), ValidationError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ValidationError::InvalidChars {
        field: "admin.reply",
    })?;
    if bytes.len() > MAX_ADMIN_REPLY_BYTES {
        return Err(ValidationError::TooLong {
            field: "admin.reply",
            max: MAX_ADMIN_REPLY_BYTES,
            actual: bytes.len(),
        });
    }
    Ok(())
}
pub(crate) fn items<T>(items: &[T]) -> Result<(), ValidationError> {
    check_count("admin.items", items.len(), MAX_ADMIN_ITEMS)
}

impl crate::envelope::RequestBody {
    pub fn admin_action(&self) -> Option<AdminAction> {
        match self {
            Self::Command(crate::commands::Command::Lifecycle(c)) => Some(c.action()),
            Self::Query(crate::queries::Query::Lifecycle(q)) => q.action(),
            _ => None,
        }
    }
}
