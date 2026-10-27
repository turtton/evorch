//! Delivery receipts for host chat input (one per GUI submission).

use crate::StorageError;

/// How far one host submission got towards its agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostInputStatus {
    /// The runtime took it; the run may still hold it in its inbox.
    Accepted,
    /// A committed run context snapshot contains it.
    Delivered,
    /// Its run ended before reading it.
    Dropped,
    /// The runtime could not tell whether the agent received it.
    Unknown,
}

impl HostInputStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Delivered => "delivered",
            Self::Dropped => "dropped",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "accepted" => Self::Accepted,
            "delivered" => Self::Delivered,
            "dropped" => Self::Dropped,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }

    /// Every state but `Accepted` is final.
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Accepted)
    }
}

/// One submission's receipt. `text` is the host's own input, kept verbatim so
/// an undelivered message can be shown and edited again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInputReceipt {
    pub prompt_id: String,
    pub run_id: String,
    pub root_run_id: String,
    pub text: String,
    pub image_count: u32,
    pub status: HostInputStatus,
    pub reason: Option<String>,
    pub accepted_at_ns: i64,
    pub status_at_ns: i64,
}

impl HostInputReceipt {
    /// Applies the same text limits as review evidence: no truncation or redaction.
    ///
    /// # Errors
    /// Rejects an empty ID, oversized or secret-bearing text.
    pub fn validate(&self) -> Result<(), StorageError> {
        if self.prompt_id.is_empty() || self.run_id.is_empty() || self.root_run_id.is_empty() {
            return Err(StorageError::Serialization(
                "host input receipt requires prompt, run and root IDs".into(),
            ));
        }
        for (field, text) in [
            ("text", Some(self.text.as_str())),
            ("reason", self.reason.as_deref()),
        ] {
            if let Some(text) = text {
                if text.len() > 256 * 1024 {
                    return Err(StorageError::Serialization(format!(
                        "host input receipt {field} exceeds 256 KiB"
                    )));
                }
                crate::entity::SecretGuard::from_env().check_text(
                    "host_input_receipt",
                    field,
                    text,
                )?;
            }
        }
        Ok(())
    }
}

/// A requested move of one receipt out of `Accepted`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInputTransition {
    pub prompt_id: String,
    pub status: HostInputStatus,
    pub reason: Option<String>,
}

impl HostInputTransition {
    pub fn new(
        prompt_id: impl Into<String>,
        status: HostInputStatus,
        reason: Option<&str>,
    ) -> Self {
        Self {
            prompt_id: prompt_id.into(),
            status,
            reason: reason.map(String::from),
        }
    }
}

/// Receipt changes committed in the same transaction as a run context snapshot.
///
/// Only `Accepted` receipts move; a receipt already in a final state keeps it.
/// Unknown prompt IDs are ignored, so a snapshot never fails over a receipt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostInputWrite {
    /// New receipts, inserted as given. A duplicate prompt ID fails the write.
    pub inserts: Vec<HostInputReceipt>,
    pub transitions: Vec<HostInputTransition>,
    /// Before the inserts, mark this snapshot run's receipts that cannot be
    /// delivered by any earlier incarnation: `Accepted` ones, and `Delivered`
    /// ones missing from the snapshot's host inputs. Used when a run is restored.
    pub reconcile_reason: Option<String>,
}

impl HostInputWrite {
    pub fn is_empty(&self) -> bool {
        self.inserts.is_empty() && self.transitions.is_empty() && self.reconcile_reason.is_none()
    }
}
