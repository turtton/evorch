//! Lifecycle failure intake uses an exclusion list, not the diagnostic allowlist.
//! Like SkillDiagnostic, these synthetic codes are not producers in diagnostic_codes::ALL.

use std::time::SystemTime;

use super::{
    ImprovementCollector, ImprovementSeverity, ImprovementSource, NewImprovementCandidate,
    bound_text, diagnostic_codes, first_line, json, normalize_variable_parts, title, unix_ns,
};

/// All non-excluded failures are harness improvement evidence, not validated defects.
/// Keep wrapper-specific checks after exclusions to avoid double-counting diagnostics.
pub(super) fn classify_run_error(reason: &str) -> Option<&'static str> {
    let reason = reason.trim();
    let lower = reason.to_ascii_lowercase();
    if ["cancelled", "stopped"].iter().any(|prefix| {
        lower == *prefix
            || lower
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with([':', ' ']))
    }) || diagnostic_codes::ALL.iter().any(|code| {
        reason
            .strip_prefix(code)
            .is_some_and(|rest| rest.starts_with(": "))
    }) || lower.starts_with("model stopped:")
        || lower.contains("length limit")
        || lower.contains("content filter")
        || lower.starts_with("goal cumulative budget exhausted")
        || reason.starts_with("モデル呼び出しに失敗しました:")
        || lower.starts_with("provider error:")
        // run_panic already reported this failure as an AgentRunPanicked diagnostic.
        || reason.starts_with("panicked:")
    {
        return None;
    }
    Some(if lower.contains("database is locked") {
        "RunError:storage_busy"
    } else if [
        // OwnershipError display strings (ownership/state.rs), plus RuntimeError::StaleOwnership.
        "owner or generation no longer matches",
        "thread is not accepting new turns",
        "thread has an active turn or needs a checkpoint",
        "owner is not claimable",
        "generation exhausted",
        "previous owner still responds or liveness could not be established",
        "run ownership generation is stale",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
    {
        "RunError:ownership"
    } else if lower.starts_with("cannot persist") || lower.starts_with("cannot record tool intent")
    {
        "RunError:persist"
    } else if reason.starts_with("ContextWindowExhausted:") {
        "RunError:context_window"
    } else if lower.contains("scratch setup failed") {
        "RunError:scratch"
    } else if lower.contains("runtime unavailable")
        || lower.contains("runtime is unavailable")
        || lower.contains("inbox closed")
    {
        "RunError:runtime"
    } else {
        "RunError:unclassified"
    })
}

impl ImprovementCollector {
    pub(super) fn run_error_observed_at(&self, run_id: &str, reason: &str, observed: SystemTime) {
        if !self.settings.policy.collect_diagnostics {
            return;
        }
        let Some(code) = classify_run_error(reason) else {
            return;
        };
        let normalized = normalize_variable_parts(first_line(reason));
        self.intake(
            "run-error",
            NewImprovementCandidate {
                id: String::new(),
                source: ImprovementSource::Diagnostic,
                code: code.into(),
                severity: ImprovementSeverity::Warning,
                title: title(&format!("{code}: {normalized}")),
                evidence: self.json_evidence(json!({
                    "reason": bound_text(reason, self.settings.policy.evidence_limit()),
                    "normalized": normalized,
                    "run_id": run_id,
                    "observed_at_ns": unix_ns(observed),
                })),
                // The prefix is also the storage daily-cap discriminator; bound the entire
                // key in bytes without ever truncating the prefix or splitting UTF-8.
                dedup_key: bound_text(&format!("run-error:{code}:{normalized}"), 120),
                run_id: Some(run_id.into()),
            },
        );
    }
}
