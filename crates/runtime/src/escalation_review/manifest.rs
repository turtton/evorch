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

use providers::{AttemptLog, AttemptRecord};

use super::{
    AuthorizationLevel, ProcedureStep, ReviewContext, ReviewError, ReviewVerdict, RiskLevel,
    WireVerdict,
};

pub const MANIFEST_VERSION: u32 = 1;

/// User-request references kept in the record itself; any further ones follow in
/// continuation events of this many each, so none is dropped.
const REFS_PER_EVENT: usize = 256;
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
    /// The agent-written delegation prompts, by hash and size only.
    pub delegation_chain: Vec<Digest>,
    /// The evidence stamp's revision the review was prepared against.
    pub evidence_revision: u64,
    /// Every real user input the payload carried.
    pub user_request_count: usize,
    /// The first part of those inputs, in payload order. The rest are in
    /// `user_request_parts` continuation events ([`ManifestPart`]) of the same
    /// `review_id`; `evorch inspect` joins them.
    pub user_requests: Vec<UserRequestRef>,
    pub user_request_parts: usize,
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

/// The model call, as requested. The provider attempts that served it are in
/// [`ReviewManifest::attempts`].
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
    /// Names this review in its continuation events. Absent when nothing was sent.
    pub review_id: Option<String>,
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
    /// The structured-output schema requested. A provider may adapt it for its
    /// API, so this names what the runtime asked for, not the provider's wire form.
    pub schema: Option<Digest>,
    /// The user message handed to the model boundary: the whole payload JSON.
    /// It matches the input that was prepared; it does not show what a provider
    /// received.
    pub payload: Option<Digest>,
    /// The provider attempts the observer saw for the call, with the model and
    /// route each used and how it ended. An attempt is noted when it starts,
    /// before its HTTP request, so a `pending` or `failed` one may never have
    /// reached the provider: this shows what was attempted and how it ended, not
    /// delivery. `request_id` is the key of the `usage_requests` row, which the
    /// ledger's retention may remove; this copy is kept.
    pub attempts: Vec<AttemptRecord>,
    /// Absent when the review had no run context.
    pub context: Option<ReviewedContext>,
    pub outcome: Outcome,
}

/// Continuation of a record's user-request list; the detail of its own event is
/// `{"manifest_part": ..}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPart {
    pub review_id: String,
    pub index: usize,
    pub count: usize,
    pub user_requests: Vec<UserRequestRef>,
}

#[derive(Serialize, Deserialize)]
struct PartDetail {
    manifest_part: ManifestPart,
}

/// Where a record is sent: the bus, tagged with the call it belongs to so it
/// sits beside that call's decision diagnostic.
#[derive(Clone)]
pub(crate) struct ManifestSink {
    bus: Arc<EventBus>,
    run_id: String,
    thread_id: Option<String>,
    call_id: Option<String>,
    seal: fn(&str) -> Result<String, String>,
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

/// Checks and cleans the serialized record before it is emitted: an oversized
/// record is refused, and anything shaped like a credential (a skill name or a
/// path can be anything) is redacted, so the storage secret guard never rejects
/// the whole event.
fn seal(json: &str) -> Result<String, String> {
    if json.len() > MAX_DETAIL_BYTES {
        return Err(format!(
            "record is {} bytes, over the {MAX_DETAIL_BYTES} byte limit",
            json.len()
        ));
    }
    Ok(secret_guard::SecretRedactor::from_env().redact(json).text)
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
            seal,
        }
    }

    fn call_id(&self) -> &str {
        self.call_id.as_deref().unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn with_seal(mut self, seal: fn(&str) -> Result<String, String>) -> Self {
        self.seal = seal;
        self
    }

    fn emit(&self, manifest: &ReviewManifest) {
        let payload_sha = manifest
            .payload
            .as_ref()
            .map_or("none", |payload| payload.sha256.as_str());
        let mut main = manifest.clone();
        let mut parts = Vec::new();
        if let (Some(review_id), Some(context)) = (&manifest.review_id, &mut main.context)
            && context.user_requests.len() > REFS_PER_EVENT
        {
            let rest = context.user_requests.split_off(REFS_PER_EVENT);
            let chunks: Vec<_> = rest.chunks(REFS_PER_EVENT).map(<[_]>::to_vec).collect();
            context.user_request_parts = chunks.len();
            let count = chunks.len();
            parts.extend(
                chunks
                    .into_iter()
                    .enumerate()
                    .map(|(index, user_requests)| PartDetail {
                        manifest_part: ManifestPart {
                            review_id: review_id.clone(),
                            index,
                            count,
                            user_requests,
                        },
                    }),
            );
        }
        self.send(&main, payload_sha);
        for part in &parts {
            self.send(part, payload_sha);
        }
    }

    fn send(&self, record: &impl Serialize, payload_sha: &str) {
        let (severity, detail) = match serde_json::to_string(record)
            .map_err(|error| error.to_string())
            .and_then(|json| (self.seal)(&json))
        {
            Ok(detail) => (DiagnosticSeverity::Info, detail),
            // The review is already decided and goes on; what is left is the
            // fact that no record exists, and the hash that would identify it.
            Err(error) => (
                DiagnosticSeverity::Warning,
                format!("manifest could not be recorded: {error}; payload_sha256={payload_sha}"),
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
        ReviewedContext {
            root_run_id: self.root_run_id.clone(),
            lineage_run_ids: self.audit.lineage_run_ids.clone(),
            delegation_chain: self
                .delegation_chain
                .iter()
                .map(|text| Digest::of(text))
                .collect(),
            evidence_revision: self.audit.evidence_revision,
            user_request_count: all.len(),
            user_requests: all
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
            user_request_parts: 0,
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
    attempts: AttemptLog,
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
            attempts: AttemptLog::default(),
            sink: context.and_then(|context| context.audit.sink.clone()),
            manifest: ReviewManifest {
                version: MANIFEST_VERSION,
                review_id: None,
                kind,
                scope: scope.to_owned(),
                tool: tool.map(str::to_owned),
                run_id: run_id.to_owned(),
                model_request: None,
                instruction: None,
                schema: None,
                payload: None,
                attempts: Vec::new(),
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

    /// Where the provider attempts for this review are noted.
    pub(super) fn attempt_log(&self) -> AttemptLog {
        self.attempts.clone()
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
        let requested_at_ns = crate::run_store::review_time_ns();
        self.manifest.model_request = Some(ModelRequest {
            category: category.to_string(),
            role: format!("{role:?}"),
            requested_at_ns,
            finished_at_ns: None,
        });
        self.manifest.instruction = Some(Digest::of(instruction));
        self.manifest.schema = Some(Digest::of(&schema.to_string()));
        let payload = Digest::of(payload);
        self.manifest.review_id = Some(
            sha256_hex(
                format!(
                    "{}|{}|{requested_at_ns}|{}",
                    self.manifest.run_id,
                    self.sink.as_ref().map_or("", |sink| sink.call_id()),
                    payload.sha256
                )
                .as_bytes(),
            )[..32]
                .to_owned(),
        );
        self.manifest.payload = Some(payload);
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
        self.manifest.attempts = self.attempts.snapshot();
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
