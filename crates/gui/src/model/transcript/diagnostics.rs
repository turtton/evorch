use event_bus::{EventKind, FaultEvent, ProviderEvent, ProviderFailureKind, SkillDiagnosticKind};

use super::TranscriptEntry;

pub(super) fn entry(kind: &EventKind) -> Option<TranscriptEntry> {
    match kind {
        EventKind::Diagnostic(event) if event.code == "ContextCheckpointSaved" => None,
        EventKind::Diagnostic(event)
            if event.source == "sandbox" && event.code == "escalation_review" =>
        {
            // Older diagnostic text included the IDs as well as structured fields.
            let detail = event
                .detail
                .strip_prefix("run_id=")
                .and_then(|detail| detail.split_once(": ").map(|(_, verdict)| verdict))
                .unwrap_or(&event.detail);
            Some(TranscriptEntry::SandboxReview {
                text: format!(
                    "[{}] sandbox (escalation_review): {detail}",
                    event.severity.as_str()
                ),
                run_id: event.run_id.clone(),
                call_id: event.call_id.clone(),
            })
        }
        EventKind::Diagnostic(event) => {
            let text = format!(
                "[{}] {} ({}): {}",
                event.severity.as_str(),
                event.source,
                event.code,
                event.detail
            );
            Some(match event.severity {
                event_bus::DiagnosticSeverity::Error => TranscriptEntry::Error { text },
                event_bus::DiagnosticSeverity::Info | event_bus::DiagnosticSeverity::Warning => {
                    TranscriptEntry::Notice { text }
                }
            })
        }
        EventKind::Provider(ProviderEvent::FallbackTriggered {
            from_provider,
            from_model,
            to_provider,
            to_model,
            ..
        }) => Some(TranscriptEntry::Notice {
            text: format!(
                "Fallback: {from_provider}/{} → {to_provider}/{to_model} (previous provider failed)",
                from_model.as_deref().unwrap_or("unknown")
            ),
        }),
        EventKind::Provider(ProviderEvent::RequestFailed {
            provider,
            model,
            duration_ms,
            failure,
            detail,
            ..
        }) => {
            // RequestFailed is per-attempt and carries no retry decision. This is
            // a transient-kind heuristic, not a guarantee that another attempt follows.
            let transient = match failure {
                ProviderFailureKind::RateLimited
                | ProviderFailureKind::Server
                | ProviderFailureKind::Transport => true,
                ProviderFailureKind::Http { status } => (500..600).contains(status),
                ProviderFailureKind::Auth
                | ProviderFailureKind::Timeout
                | ProviderFailureKind::InvalidResponse
                | ProviderFailureKind::Quota
                | ProviderFailureKind::Other => false,
            };
            let suffix = if transient { ", retrying" } else { "" };
            let mut text = format!(
                "Provider request failed: {provider}/{model} — {} ({duration_ms} ms{suffix})",
                failure_label(failure)
            );
            if let Some(detail) = detail {
                text.push_str(": ");
                text.push_str(detail);
            }
            Some(if transient {
                TranscriptEntry::Notice { text }
            } else {
                TranscriptEntry::Error { text }
            })
        }
        EventKind::Provider(ProviderEvent::CooldownStarted {
            profile,
            model,
            failure,
            cooldown_ms,
            ..
        }) => {
            let target = match model {
                Some(model) => format!("{profile}/{model}"),
                None => profile.clone(),
            };
            let period = match cooldown_ms {
                Some(ms) => format!("for {}s", ms.div_ceil(1000)),
                None => "until settings are reloaded".into(),
            };
            Some(TranscriptEntry::Notice {
                text: format!(
                    "Cooldown: {target} skipped {period} ({})",
                    failure_label(failure)
                ),
            })
        }
        EventKind::Fault(FaultEvent::SkillDiagnostic {
            kind,
            skill,
            scope,
            detail,
        }) => {
            let label = match kind {
                SkillDiagnosticKind::DiscoveryError => "discovery error",
                SkillDiagnosticKind::ValidationError => "validation error",
                SkillDiagnosticKind::Shadowed => "shadowed",
            };
            Some(TranscriptEntry::Error {
                text: format!("Skill {skill} ({scope}): {label} — {detail}"),
            })
        }
        EventKind::Fault(FaultEvent::SubscriberLagged { .. }) => None,
        EventKind::Lifecycle(_)
        | EventKind::Ledger(_)
        | EventKind::Message(_)
        | EventKind::Tool(_)
        | EventKind::Usage(_)
        | EventKind::Provider(_)
        | EventKind::AgentMessage(_)
        | EventKind::Compaction(_)
        | EventKind::Ownership(_)
        | EventKind::Orchestrator(_)
        | EventKind::Snapshot(_) => None,
    }
}

fn failure_label(failure: &ProviderFailureKind) -> String {
    match failure {
        ProviderFailureKind::RateLimited => "rate limited".into(),
        ProviderFailureKind::Server => "server error".into(),
        ProviderFailureKind::Transport => "transport error".into(),
        ProviderFailureKind::Http { status } => format!("HTTP {status}"),
        ProviderFailureKind::Auth => "authentication failed".into(),
        ProviderFailureKind::Timeout => "timed out".into(),
        ProviderFailureKind::InvalidResponse => "invalid response".into(),
        ProviderFailureKind::Quota => "quota exceeded".into(),
        ProviderFailureKind::Other => "unknown error".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 期間付きのプロファイル全体のクールダウンと、期間なしのモデル単位のクールダウン
    // When: transcript の項目にする
    // Then: 対象・期間・原因を示す Notice になる
    #[test]
    fn cooldown_renders_target_period_and_cause() {
        let quota = EventKind::Provider(ProviderEvent::CooldownStarted {
            profile: "kimi".into(),
            model: None,
            failure: ProviderFailureKind::Quota,
            cooldown_ms: Some(1_800_000),
            run_id: Some("run-1".into()),
        });
        let auth = EventKind::Provider(ProviderEvent::CooldownStarted {
            profile: "codex-work".into(),
            model: Some("gpt-6.1-sol".into()),
            failure: ProviderFailureKind::Auth,
            cooldown_ms: None,
            run_id: None,
        });

        assert_eq!(
            entry(&quota),
            Some(TranscriptEntry::Notice {
                text: "Cooldown: kimi skipped for 1800s (quota exceeded)".into(),
            })
        );
        assert_eq!(
            entry(&auth),
            Some(TranscriptEntry::Notice {
                text: "Cooldown: codex-work/gpt-6.1-sol skipped until settings are reloaded (authentication failed)".into(),
            })
        );
    }

    // Given: 詳細付きの失敗 / When: transcript の項目にする / Then: 末尾に詳細が付く
    #[test]
    fn request_failure_appends_detail() {
        let event = EventKind::Provider(ProviderEvent::RequestFailed {
            request_id: "req-1".into(),
            provider: "kimi-subscription".into(),
            profile: Some("kimi".into()),
            protocol: "openai-chat-completions".into(),
            model: "k3".into(),
            streaming: true,
            duration_ms: 12,
            failure: ProviderFailureKind::Http { status: 403 },
            run_id: None,
            purpose: None,
            detail: Some("http error (status 403): usage limit reached".into()),
        });

        assert_eq!(
            entry(&event),
            Some(TranscriptEntry::Error {
                text: "Provider request failed: kimi-subscription/k3 — HTTP 403 (12 ms): http error (status 403): usage limit reached".into(),
            })
        );
    }

    #[test]
    fn fallback_renders_selected_provider_as_notice() {
        // Given
        let event = EventKind::Provider(ProviderEvent::FallbackTriggered {
            from_provider: "kimi".into(),
            from_model: Some("k3".into()),
            to_provider: "neuralwatt".into(),
            to_model: "kimi-k3".into(),
            logical_model: "worker".into(),
            session_id: "session".into(),
            failure: ProviderFailureKind::Timeout,
            request_id: None,
        });
        // When
        let mut registry = crate::model::transcript_registry::TranscriptRegistry::default();
        registry.bind_thread_root("owner", "session");
        registry.select_thread(Some("other".into()));
        registry.apply(&event_bus::Event::new(event));
        // Then
        assert_eq!(
            registry.run("session").unwrap().entries(),
            &[TranscriptEntry::Notice {
                text: "Fallback: kimi/k3 → neuralwatt/kimi-k3 (previous provider failed)".into(),
            }]
        );
        assert!(registry.thread().entries().is_empty());
        registry.select_thread(Some("owner".into()));
        assert_eq!(registry.thread().entries().len(), 1);
    }
}
