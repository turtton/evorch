//! Passive, storage-backed improvement evidence and local drafts (phase A).
//!
//! Opt-in is installing [`ImprovementSettings`], not constructing a default policy.
//! Nothing here files issues, modifies prompts, or writes canonical packets.

mod crash;
mod drafts;
pub(crate) mod lifecycle;

pub use crash::{SpooledCrash, drain_crash_spool, install_crash_spool};
pub use drafts::{render_issue_draft, render_packet_draft};

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use event_bus::{DiagnosticEvent, DiagnosticSeverity, EventKind, EventReceiver, FaultEvent};
use serde_json::{Value, json};
use storage::improvement::{
    ImprovementCandidate, ImprovementRecordOutcome, ImprovementSeverity, ImprovementSource,
    ImprovementStatus, ImprovementWritePolicy, NewImprovementCandidate,
};

/// Resolved, config-agnostic runtime policy for phase-A self-improvement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImprovementPolicy {
    /// Composition must resolve None to <storage-dir>/self-improvement/drafts.
    /// An unresolved directory fails closed: storage intake works, file output does not.
    pub draft_dir: Option<PathBuf>,
    pub evidence_max_bytes: u32,
    pub daily_limit: u32,
    pub duplicate_cooldown_secs: u64,
    pub max_candidates: u32,
    pub collect_diagnostics: bool,
    pub collect_lessons: bool,
}

impl Default for ImprovementPolicy {
    fn default() -> Self {
        Self {
            draft_dir: None,
            evidence_max_bytes: 2048,
            daily_limit: 20,
            duplicate_cooldown_secs: 86_400,
            max_candidates: 200,
            collect_diagnostics: true,
            collect_lessons: true,
        }
    }
}

impl ImprovementPolicy {
    /// Resolves paths; the caller must honor `cfg.enabled` before installing settings.
    pub fn from_config(cfg: &config::SelfImprovementConfig, storage_dir: &Path) -> Self {
        Self {
            draft_dir: Some(
                cfg.draft_dir
                    .as_ref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| storage_dir.join("self-improvement").join("drafts")),
            ),
            evidence_max_bytes: cfg.evidence_max_bytes.min(65_536),
            daily_limit: cfg.daily_limit,
            duplicate_cooldown_secs: cfg.duplicate_cooldown_secs,
            max_candidates: cfg.max_candidates,
            collect_diagnostics: cfg.collect_diagnostics,
            collect_lessons: cfg.collect_lessons,
        }
    }

    fn evidence_limit(&self) -> usize {
        self.evidence_max_bytes.min(65_536) as usize
    }

    fn write_policy(&self) -> ImprovementWritePolicy {
        ImprovementWritePolicy {
            duplicate_cooldown: Duration::from_secs(self.duplicate_cooldown_secs),
            daily_limit: self.daily_limit,
            max_candidates: self.max_candidates,
        }
    }
}

#[derive(Clone)]
pub struct ImprovementSettings {
    pub writer: storage::StorageHandle,
    pub project: String,
    pub policy: ImprovementPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateClass {
    HarnessImprovement,
    TransientOrExternal,
    Ignored,
}

/// Conservative allowlist of actual DiagnosticEvent producers (plus reserved crash intake).
/// Severity alone never establishes a harness fault; new codes default to ignored.
pub fn classify_diagnostic(event: &DiagnosticEvent) -> CandidateClass {
    use CandidateClass::*;
    match event.code.as_str() {
        // identical_calls: repeated calls stopped by the harness's loop detector.
        "IdenticalToolCalls" => HarnessImprovement,
        // budget_tracker / escalation_detector: execution stopped making progress.
        "NoProgress" => HarnessImprovement,
        // memory_lifecycle: extraction/review failed; candidates remain unpromoted.
        "LearningPipelineFailed" => HarnessImprovement,
        // run_context: failed to persist restoration context; previous checkpoint retained.
        "ContextSnapshotFailed" => HarnessImprovement,
        // escalation/handoff: terminal direct -> orchestrator ownership/question transfer failed.
        "EscalationHandoffFailed" => HarnessImprovement,
        // Reserved for the durable panic spool, not an existing bus emitter.
        "CrashRecovered" => HarnessImprovement,
        // compose: no verified provider, which is provider/auth/config dependent.
        "ProviderUnavailable" => TransientOrExternal,
        // budget_tracker: user-configured execution limits, not harness defects.
        "BudgetWarning" | "BudgetExhausted" => TransientOrExternal,
        // providers/cache: unchanged wire prefix but reduced provider cache retention.
        "CacheRegression" => TransientOrExternal,
        // run_context: successful checkpoint is routine telemetry.
        "ContextCheckpointSaved" => Ignored,
        // sandbox/network/MCP: access decisions and scope policy, not internal faults.
        "escalation_review" | "tool_call_access" | "scope_denied" => Ignored,
        // LSP and MCP: diagnostics from external tools/user code, no harness attribution.
        "publish_diagnostics" | "tool_result" => Ignored,
        // Browser errors may be site/action/environment dependent; no harness attribution.
        "browser.error" | "browser.close_error" | "browser.action_error" => Ignored,
        // Browser session/action/DOM/screenshot telemetry is not a fault signal.
        "browser.session" | "browser.stopped" | "browser.action" | "browser.dom_diff"
        | "browser.screenshot" => Ignored,
        // Includes tool-result codes (unknown_run/run_output_denied), not bus diagnostics.
        _ => Ignored,
    }
}

#[derive(Debug, thiserror::Error)]
enum SelfImprovementError {
    #[error("draft directory was not resolved by composition")]
    UnresolvedDirectory,
    #[error("improvement identity unavailable")]
    Identity,
    #[error("draft I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("improvement storage operation failed: {0}")]
    Storage(#[from] storage::StorageError),
}

pub struct ImprovementCollector {
    settings: ImprovementSettings,
}

impl ImprovementCollector {
    pub fn new(settings: ImprovementSettings) -> Self {
        Self { settings }
    }

    /// Synchronous intake. All persistence/draft/identity errors are warn-only.
    pub fn handle_diagnostic(&self, event: &DiagnosticEvent) {
        if !self.settings.policy.collect_diagnostics
            || classify_diagnostic(event) != CandidateClass::HarnessImprovement
        {
            return;
        }
        let severity = match event.severity {
            DiagnosticSeverity::Info => ImprovementSeverity::Info,
            DiagnosticSeverity::Warning => ImprovementSeverity::Warning,
            DiagnosticSeverity::Error => ImprovementSeverity::Error,
        };
        self.intake(
            "diag",
            NewImprovementCandidate {
                id: String::new(),
                source: ImprovementSource::Diagnostic,
                code: event.code.clone(),
                severity,
                title: title(&format!("{}: {}", event.code, first_line(&event.detail))),
                evidence: self.json_evidence(json!({
                    "source": event.source, "code": event.code, "severity": severity.as_str(),
                    "detail": bound_text(&event.detail, self.settings.policy.evidence_limit()),
                    "run_id": event.run_id, "thread_id": event.thread_id, "call_id": event.call_id,
                })),
                dedup_key: format!("diag:{}", event.code),
                run_id: event.run_id.clone(),
            },
        );
    }

    /// Only already-promoted harness-scoped lessons enter this passive intake; project
    /// and user lessons are task memory, not harness feedback. Their evidence stays text.
    /// Failure never changes the learning pipeline's outcome.
    pub fn ingest_lessons(&self, lessons: &[storage::memory::Lesson]) {
        if !self.settings.policy.collect_lessons {
            return;
        }
        for lesson in lessons
            .iter()
            .filter(|lesson| lesson.scope == storage::memory::LessonScope::Harness)
        {
            self.intake(
                "lesson",
                NewImprovementCandidate {
                    id: String::new(),
                    source: ImprovementSource::Lesson,
                    code: "LessonPromoted".into(),
                    severity: ImprovementSeverity::Info,
                    title: title(&lesson.content),
                    evidence: bound_text(&lesson.evidence, self.settings.policy.evidence_limit()),
                    dedup_key: format!("lesson:{}", lesson.id),
                    run_id: None,
                },
            );
        }
    }

    pub fn ingest_crashes(&self, crashes: Vec<SpooledCrash>) {
        if !self.settings.policy.collect_diagnostics {
            return;
        }
        for crash in crashes {
            self.intake("crash", NewImprovementCandidate {
                id: String::new(),
                source: ImprovementSource::Diagnostic,
                code: "CrashRecovered".into(),
                severity: ImprovementSeverity::Error,
                title: title(&format!("Recovered crash: {}", first_line(&crash.message))),
                evidence: self.json_evidence(json!({
                    "file_name": crash.file_name,
                    "message": bound_text(&crash.message, self.settings.policy.evidence_limit()),
                    "location": crash.location, "thread": crash.thread,
                    "timestamp_unix": crash.timestamp_unix,
                })),
                dedup_key: format!("crash:{}", crash.file_name),
                run_id: None,
            });
        }
    }

    /// Broadcast lag and closure end this best-effort observer with a warning.
    pub async fn run(self, mut receiver: EventReceiver) {
        loop {
            match receiver.recv().await {
                Ok(event) => match event.kind {
                    EventKind::Diagnostic(diagnostic) => self.handle_diagnostic(&diagnostic),
                    EventKind::Fault(FaultEvent::SkillDiagnostic {
                        kind,
                        skill,
                        scope,
                        detail,
                    }) if self.settings.policy.collect_diagnostics => {
                        // SkillDiagnosticKind has Debug, not Display; stable variant spellings.
                        let kind = format!("{kind:?}");
                        self.intake("diag", NewImprovementCandidate {
                            id: String::new(),
                            source: ImprovementSource::Diagnostic,
                            code: format!("SkillDiagnostic:{kind}"),
                            severity: ImprovementSeverity::Warning,
                            title: title(&format!("Skill diagnostic: {skill}")),
                            evidence: self.json_evidence(json!({
                                "kind": kind, "skill": skill, "scope": scope,
                                "detail": bound_text(&detail, self.settings.policy.evidence_limit()),
                            })),
                            dedup_key: format!("skill-diag:{skill}:{kind}"),
                            run_id: None,
                        });
                    }
                    _ => {}
                },
                Err(error) => {
                    tracing::warn!(?error, "self-improvement observer stopped");
                    break;
                }
            }
        }
    }

    fn json_evidence(&self, mut value: Value) -> String {
        let limit = self.settings.policy.evidence_limit();
        // Bound the whole serialized object, including escaping and metadata, not just detail.
        // Keep valid JSON and all keys where possible; shrink the largest text field first.
        loop {
            let text = value.to_string();
            if text.len() <= limit {
                return text;
            }
            let longest = value.as_object_mut().and_then(|object| {
                object
                    .values_mut()
                    .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
                    .max_by_key(|v| v.as_str().map_or(0, str::len))
            });
            let Some(field) = longest else {
                // Manually supplied policies can be smaller than the JSON key overhead.
                return match limit {
                    0 => String::new(),
                    1 => "0".into(),
                    _ => "{}".into(),
                };
            };
            if let Some(string) = field.as_str() {
                *field = Value::String(bound_text(
                    string,
                    string.len().saturating_sub(text.len() - limit),
                ));
            }
        }
    }

    fn intake(&self, prefix: &str, mut candidate: NewImprovementCandidate) {
        let result = new_id(prefix).and_then(|id| {
            candidate.id = id;
            self.record(candidate)
        });
        if let Err(error) = result {
            // Do not log the evidence (including SecretGuard-rejected material).
            tracing::warn!(%error, "self-improvement intake failed; execution result retained");
        }
    }

    fn record(&self, candidate: NewImprovementCandidate) -> Result<(), SelfImprovementError> {
        let outcome = self.settings.writer.record_improvement_candidate(
            &self.settings.project,
            candidate.clone(),
            self.settings.policy.write_policy(),
        )?;
        if let ImprovementRecordOutcome::Stored { id } = outcome {
            // StorageHandle intentionally exposes no read connection. Render only the guarded
            // intake fields; the authoritative creation timestamp/status live in the database.
            let c = ImprovementCandidate {
                id: id.clone(),
                project: self.settings.project.clone(),
                created_at_ns: 0,
                source: candidate.source,
                code: candidate.code,
                severity: candidate.severity,
                title: candidate.title,
                evidence: candidate.evidence,
                dedup_key: candidate.dedup_key,
                status: ImprovementStatus::New,
                draft_path: None,
                run_id: candidate.run_id,
            };
            let (issue, _) =
                self.write_drafts(&id, &render_issue_draft(&c), &render_packet_draft(&c))?;
            if !self.settings.writer.attach_improvement_draft(&id, &issue)? {
                tracing::warn!("improvement candidate evicted before draft attachment");
            }
        }
        Ok(())
    }
}

fn new_id(prefix: &str) -> Result<String, SelfImprovementError> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| SelfImprovementError::Identity)?;
    Ok(format!("{prefix}-{:032x}", u128::from_le_bytes(bytes)))
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

fn title(text: &str) -> String {
    first_line(text).chars().take(120).collect()
}

/// UTF-8-safe byte bound, including the truncation marker. For caps smaller than
/// the marker itself, only its fitting UTF-8 prefix is returned (zero stays empty).
fn bound_text(text: &str, max_bytes: usize) -> String {
    const MARKER: &str = "…[truncated]";
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    fn prefix(text: &str, mut end: usize) -> &str {
        end = end.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }
    if max_bytes < MARKER.len() {
        return prefix(MARKER, max_bytes).to_owned();
    }
    format!("{}{MARKER}", prefix(text, max_bytes - MARKER.len()))
}

#[cfg(test)]
mod tests;
