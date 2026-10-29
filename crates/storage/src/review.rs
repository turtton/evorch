//! Durable user evidence and explicit review lineage.

/// An immutable assistant turn reference whose approval can be consumed once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantReferenceRow {
    pub reference_id: String,
    pub source_run_id: String,
    pub turn_end: usize,
    pub text: String,
    pub consumed: bool,
    pub created_at_ns: i64,
}

impl AssistantReferenceRow {
    /// Validate the complete assistant text without redaction or truncation.
    ///
    /// # Errors
    /// Rejects oversized or secret-bearing text and indices outside SQLite's range.
    pub fn validate(&self) -> Result<(), crate::StorageError> {
        i64::try_from(self.turn_end)
            .map_err(|_| crate::StorageError::OutOfRange("assistant reference turn end"))?;
        if self.text.len() > 256 * 1024 {
            return Err(crate::StorageError::Serialization(
                "assistant reference text exceeds 256 KiB".into(),
            ));
        }
        crate::entity::SecretGuard::from_env().check_text("assistant_reference", "text", &self.text)
    }
}

/// A skill body the host expanded from the user's `@name` mention. It is kept
/// apart from the user's own words in [`ReviewEvidenceRow::text`], so a reader
/// can tell what the user typed from what the runtime read on the user's behalf.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EvidenceAttachment {
    pub name: String,
    /// Where the skill came from (`user`, `repo`, `builtin`, ...).
    pub scope: String,
    pub body: String,
}

/// Immutable evidence supplied by the host or user for a review target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewEvidenceRow {
    /// Caller-generated key covering identity, text, question and assistant reference.
    pub id: String,
    /// Query cache only; lineage edges are the source of truth.
    pub root_run_id: String,
    pub target_run_id: String,
    /// `initial_prompt`, `user_answer`, or `host_chat`.
    pub kind: String,
    pub text: String,
    pub in_reply_to_question_id: Option<String>,
    pub in_reply_to_title: Option<String>,
    pub reference_kind: Option<String>,
    pub reference_context: Option<String>,
    /// Skill bodies expanded from the user's `@name` mentions in `text`.
    pub attachments: Vec<EvidenceAttachment>,
    pub created_at_ns: i64,
}

/// An explicit child-to-parent relationship retained independently of snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineageEdge {
    pub child_run_id: String,
    pub parent_run_id: String,
    pub created_at_ns: i64,
}

impl ReviewEvidenceRow {
    /// Validate evidence before persistence or RAM-only reflection using the same policy.
    ///
    /// # Errors
    /// Rejects secret-bearing or oversized evidence without truncation or redaction.
    pub fn validate(&self) -> Result<(), crate::StorageError> {
        let guard = crate::entity::SecretGuard::from_env();
        let attachments = self.attachments.iter().flat_map(|attachment| {
            [
                ("attachment_name", Some(attachment.name.as_str())),
                ("attachment_scope", Some(attachment.scope.as_str())),
                ("attachment_body", Some(attachment.body.as_str())),
            ]
        });
        for (field, text) in [
            ("text", Some(self.text.as_str())),
            ("in_reply_to_title", self.in_reply_to_title.as_deref()),
            ("reference_context", self.reference_context.as_deref()),
        ]
        .into_iter()
        .chain(attachments)
        {
            if let Some(text) = text {
                if text.len() > 256 * 1024 {
                    return Err(crate::StorageError::Serialization(format!(
                        "review evidence {field} exceeds 256 KiB"
                    )));
                }
                guard.check_text("review_evidence", field, text)?;
            }
        }
        Ok(())
    }
}
