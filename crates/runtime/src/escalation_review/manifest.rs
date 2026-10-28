//! The record of what one escalation review was given.
//!
//! A review sends the model one instruction and one JSON payload. The record
//! lets a later investigation check which inputs reached the reviewer without
//! keeping their text: user words are named by the ID of their immutable
//! evidence row, and everything that changes on disk or is reshaped for the
//! reviewer (skill bodies, AGENTS.md, the payload itself, the instruction) is
//! named by SHA-256 and size.
//!
//! The record is emitted as a sandbox [`diagnostic_codes::ESCALATION_REVIEW_MANIFEST`]
//! event, which is persisted unconditionally and exempt from diagnostic
//! retention, like the decisions it explains. Recording is best effort: a
//! failure leaves a warning with the same code and never changes the review.
//!
//! New inputs belong in [`ReviewedContext`] (or [`ReviewManifest`] for things
//! outside the context) as a digest, bumping [`MANIFEST_VERSION`] only when an
//! existing field changes meaning.

use std::sync::Arc;

use agents::Role;
use config::agent_categories::CategoryId;
use event_bus::event::diagnostic_codes;
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, EventBus};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{
    AuthorizationLevel, ProcedureStep, ReviewContext, ReviewError, ReviewVerdict, RiskLevel,
    WireVerdict,
};

pub const MANIFEST_VERSION: u32 = 1;

/// Most user-request references one record lists, newest last. The count and
/// the omitted number are always recorded.
const MAX_USER_REQUEST_REFS: usize = 256;
/// Largest record emitted, well below the storage limit for one event.
const MAX_DETAIL_BYTES: usize = 128 * 1024;

/// Lower-case hex SHA-256, the digest every stored hash uses.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Hash and size of bytes the reviewer received.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest {
    pub sha256: String,
    pub bytes: u64,
}

impl Digest {
    fn of(text: &str) -> Self {
        Self {
            sha256: sha256_hex(text.as_bytes()),
            bytes: text.len() as u64,
        }
    }
}

/// A [`Digest`] of text that may have been cut before it was sent. The digest
/// covers the text as sent, so `truncated` explains a size at the limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextDigest {
    #[serde(flatten)]
    pub digest: Digest,
    pub truncated: bool,
}

impl TextDigest {
    pub(super) fn of(text: &str, truncated: bool) -> Self {
        Self {
            digest: Digest::of(text),
            truncated,
        }
    }
}

/// One loaded skill as the reviewer saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillEntry {
    pub name: String,
    pub scope: String,
    pub trust: String,
    pub loaded_by: String,
    #[serde(flatten)]
    pub text: TextDigest,
    /// For a skill the user attached with `@skill`: the evidence ID of the
    /// message it was cut out of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_to_evidence_id: Option<String>,
}

/// The project's AGENTS.md as the reviewer saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesEntry {
    pub path: String,
    pub trust: String,
    #[serde(flatten)]
    pub text: TextDigest,
}

/// A real user input shown to the reviewer, by its stored evidence. The text is
/// the stored row's; it is not repeated here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRequestRef {
    /// ID of the `review_evidence` row. Absent for an input kept in memory only.
    pub evidence_id: Option<String>,
    /// Row kind, such as `host_chat`, `initial_prompt` or `user_answer`.
    pub kind: Option<String>,
    pub target_run_id: String,
    /// For an answer: the question it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers_question_id: Option<String>,
}

/// Everything the payload's `context` carried, except the user's text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewedContext {
    pub root_run_id: String,
    pub lineage_run_ids: Vec<String>,
    pub delegation_chain: Vec<String>,
    /// The evidence stamp's revision the review was prepared against.
    pub evidence_revision: u64,
    pub user_request_count: usize,
    /// Up to the newest [`MAX_USER_REQUEST_REFS`], in payload order.
    pub user_requests: Vec<UserRequestRef>,
    pub user_requests_omitted: usize,
    pub project_rules: Option<RulesEntry>,
    pub loaded_skills: Vec<SkillEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewKind {
    Shell,
    FileAccess,
    Web,
    Upload,
}

/// The model call, as requested. The route that served it is in the usage
/// ledger: `usage_requests` rows of this run with purpose `escalation_review`
/// finishing between these two times.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub category: String,
    pub role: String,
    pub requested_at_ns: i64,
    pub finished_at_ns: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    /// The reviewer answered with a valid verdict.
    Decided,
    ModelError,
    Timeout,
    InvalidVerdict,
    /// The review was dropped before it finished; the request may have been sent.
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    Deny,
    RequiresUser,
}

/// The reviewer's verdict and what the thresholds made of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerdictRecord {
    pub approve: bool,
    pub risk_level: Option<RiskLevel>,
    pub authorization_level: Option<AuthorizationLevel>,
    pub procedure_step: ProcedureStep,
    pub decision: Decision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub status: OutcomeStatus,
    pub verdict: Option<VerdictRecord>,
}

/// What one review was given and what it answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewManifest {
    pub version: u32,
    pub kind: ReviewKind,
    /// Access the request asked for: `host_unsandboxed` or `network_only` for
    /// shell, `file_read` or `file_write`, `web`, `upload`.
    pub scope: String,
    /// The tool that made the request.
    pub tool: Option<String>,
    pub run_id: String,
    /// Absent when the review ended before anything was sent.
    pub model_request: Option<ModelRequest>,
    /// The system message as sent.
    pub instruction: Option<Digest>,
    /// The structured-output schema as sent.
    pub schema: Option<Digest>,
    /// The user message as sent: the whole payload JSON.
    pub payload: Option<Digest>,
    /// Absent when the review had no run context.
    pub context: Option<ReviewedContext>,
    pub outcome: Outcome,
}

/// Where a record is sent: the bus, tagged with the call it belongs to so it
/// sits beside that call's decision diagnostic.
#[derive(Clone)]
pub(crate) struct ManifestSink {
    bus: Arc<EventBus>,
    run_id: String,
    thread_id: Option<String>,
    call_id: Option<String>,
    encode: fn(&ReviewManifest) -> Result<String, String>,
}

impl std::fmt::Debug for ManifestSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManifestSink")
            .field("run_id", &self.run_id)
            .field("call_id", &self.call_id)
            .finish_non_exhaustive()
    }
}

fn encode(manifest: &ReviewManifest) -> Result<String, String> {
    let detail = serde_json::to_string(manifest).map_err(|error| error.to_string())?;
    if detail.len() > MAX_DETAIL_BYTES {
        return Err(format!(
            "record is {} bytes, over the {MAX_DETAIL_BYTES} byte limit",
            detail.len()
        ));
    }
    Ok(detail)
}

impl ManifestSink {
    pub(crate) fn new(
        bus: Arc<EventBus>,
        run_id: &str,
        thread_id: Option<String>,
        call_id: Option<String>,
    ) -> Self {
        Self {
            bus,
            run_id: run_id.to_owned(),
            thread_id,
            call_id,
            encode,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_encoder(
        mut self,
        encode: fn(&ReviewManifest) -> Result<String, String>,
    ) -> Self {
        self.encode = encode;
        self
    }

    fn emit(&self, manifest: &ReviewManifest) {
        let (severity, detail) = match (self.encode)(manifest) {
            Ok(detail) => (DiagnosticSeverity::Info, detail),
            // The review is already decided and goes on; what is left is the
            // fact that no record exists, and the hash that would identify it.
            Err(error) => (
                DiagnosticSeverity::Warning,
                format!(
                    "manifest could not be recorded: {error}; payload_sha256={}",
                    manifest
                        .payload
                        .as_ref()
                        .map_or("none", |payload| payload.sha256.as_str())
                ),
            ),
        };
        self.bus.emit(Event::new(DiagnosticEvent {
            source: "sandbox".into(),
            severity,
            code: diagnostic_codes::ESCALATION_REVIEW_MANIFEST.into(),
            detail,
            run_id: Some(self.run_id.clone()),
            thread_id: self.thread_id.clone(),
            call_id: self.call_id.clone(),
        }));
    }
}

/// What the reviewer's view of the run contributes to a record. Held by the
/// [`ReviewContext`] but never sent to the reviewer.
#[derive(Debug, Clone, Default)]
pub(crate) struct ContextAudit {
    pub(super) lineage_run_ids: Vec<String>,
    pub(super) evidence_revision: u64,
    pub(super) sink: Option<ManifestSink>,
}

impl ReviewContext {
    /// Sends this review's record to `sink`.
    #[must_use]
    pub(crate) fn recorded_to(mut self, sink: ManifestSink) -> Self {
        self.audit.sink = Some(sink);
        self
    }

    fn reviewed(&self) -> ReviewedContext {
        let all = &self.real_user_requests;
        let omitted = all.len().saturating_sub(MAX_USER_REQUEST_REFS);
        ReviewedContext {
            root_run_id: self.root_run_id.clone(),
            lineage_run_ids: self.audit.lineage_run_ids.clone(),
            delegation_chain: self.delegation_chain.clone(),
            evidence_revision: self.audit.evidence_revision,
            user_request_count: all.len(),
            user_requests: all[omitted..]
                .iter()
                .map(|request| UserRequestRef {
                    evidence_id: request.evidence.id.clone(),
                    kind: request.evidence.kind.clone(),
                    target_run_id: request.target_run_id.clone(),
                    answers_question_id: request
                        .in_reply_to
                        .as_ref()
                        .map(|question| question.id.clone()),
                })
                .collect(),
            user_requests_omitted: omitted,
            project_rules: self.project_rules.as_ref().map(|rules| RulesEntry {
                path: rules.source.clone(),
                trust: rules.trust.to_owned(),
                text: TextDigest::of(&rules.text, rules.truncated),
            }),
            loaded_skills: self
                .loaded_skills
                .iter()
                .map(super::LoadedSkillView::manifest_entry)
                .collect(),
        }
    }
}

/// A record being built across one review. It is emitted exactly once, by
/// [`Audit::conclude`] or, when the review is dropped after sending (an
/// interrupted tool call), as `cancelled`.
pub(super) struct Audit {
    sink: Option<ManifestSink>,
    manifest: ReviewManifest,
    sent: bool,
    done: bool,
}

impl Audit {
    pub(super) fn begin(
        kind: ReviewKind,
        scope: &str,
        tool: Option<&str>,
        run_id: &str,
        context: Option<&ReviewContext>,
    ) -> Self {
        Self {
            sink: context.and_then(|context| context.audit.sink.clone()),
            manifest: ReviewManifest {
                version: MANIFEST_VERSION,
                kind,
                scope: scope.to_owned(),
                tool: tool.map(str::to_owned),
                run_id: run_id.to_owned(),
                model_request: None,
                instruction: None,
                schema: None,
                payload: None,
                context: context.map(ReviewContext::reviewed),
                outcome: Outcome {
                    status: OutcomeStatus::Cancelled,
                    verdict: None,
                },
            },
            sent: false,
            done: false,
        }
    }

    /// Notes the exact text about to go to the model.
    pub(super) fn sent(
        &mut self,
        category: CategoryId,
        role: Role,
        instruction: &str,
        schema: &serde_json::Value,
        payload: &str,
    ) {
        self.manifest.model_request = Some(ModelRequest {
            category: category.to_string(),
            role: format!("{role:?}"),
            requested_at_ns: crate::run_store::review_time_ns(),
            finished_at_ns: None,
        });
        self.manifest.instruction = Some(Digest::of(instruction));
        self.manifest.schema = Some(Digest::of(&schema.to_string()));
        self.manifest.payload = Some(Digest::of(payload));
        self.sent = true;
    }

    /// Ends the record with the review's result. `wire` is the reviewer's parsed
    /// answer when there was one.
    pub(super) fn conclude(
        mut self,
        wire: Option<&WireVerdict>,
        result: &Result<ReviewVerdict, ReviewError>,
    ) {
        self.manifest.outcome = match (result, wire) {
            (Ok(verdict), Some(wire)) => Outcome {
                status: OutcomeStatus::Decided,
                verdict: Some(VerdictRecord {
                    approve: wire.approve,
                    risk_level: wire.risk_level,
                    authorization_level: wire.authorization_level,
                    procedure_step: wire.procedure_step,
                    decision: match verdict {
                        ReviewVerdict::Approve => Decision::Approve,
                        ReviewVerdict::Deny { .. } => Decision::Deny,
                        ReviewVerdict::RequiresUser { .. } => Decision::RequiresUser,
                    },
                }),
            },
            (Ok(_), None) => Outcome {
                status: OutcomeStatus::Decided,
                verdict: None,
            },
            (Err(error), _) => Outcome {
                status: match error {
                    ReviewError::Model | ReviewError::Unsynced => OutcomeStatus::ModelError,
                    ReviewError::Timeout => OutcomeStatus::Timeout,
                    ReviewError::InvalidVerdict => OutcomeStatus::InvalidVerdict,
                },
                verdict: None,
            },
        };
        self.finish();
    }

    fn finish(&mut self) {
        self.done = true;
        if !self.sent {
            return;
        }
        if let Some(request) = &mut self.manifest.model_request {
            request.finished_at_ns = Some(crate::run_store::review_time_ns());
        }
        if let Some(sink) = &self.sink {
            sink.emit(&self.manifest);
        }
    }
}

impl Drop for Audit {
    fn drop(&mut self) {
        if !self.done {
            self.finish();
        }
    }
}
