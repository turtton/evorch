use event_bus::{EventKind, FaultEvent, ProviderEvent, ProviderFailureKind, SkillDiagnosticKind};

use super::TranscriptEntry;

pub(super) fn entry(kind: &EventKind) -> Option<TranscriptEntry> {
    match kind {
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
            ..
        }) => {
            // RequestFailed is per-attempt and carries no retry decision. This is
            // a transient-kind heuristic, not a guarantee that another attempt follows.
            let (label, transient) = match failure {
                ProviderFailureKind::RateLimited => ("rate limited".into(), true),
                ProviderFailureKind::Server => ("server error".into(), true),
                ProviderFailureKind::Transport => ("transport error".into(), true),
                ProviderFailureKind::Http { status } => {
                    (format!("HTTP {status}"), (500..600).contains(status))
                }
                ProviderFailureKind::Auth => ("authentication failed".into(), false),
                ProviderFailureKind::Timeout => ("timed out".into(), false),
                ProviderFailureKind::InvalidResponse => ("invalid response".into(), false),
                ProviderFailureKind::Quota => ("quota exceeded".into(), false),
                ProviderFailureKind::Other => ("unknown error".into(), false),
            };
            let suffix = if transient { ", retrying" } else { "" };
            let text = format!(
                "Provider request failed: {provider}/{model} — {label} ({duration_ms} ms{suffix})"
            );
            Some(if transient {
                TranscriptEntry::Notice { text }
            } else {
                TranscriptEntry::Error { text }
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
        EventKind::Fault(FaultEvent::SubscriberLagged {
            subscriber_id,
            skipped,
        }) => Some(TranscriptEntry::Notice {
            text: format!("Subscriber {subscriber_id} lagged: skipped {skipped} events"),
        }),
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

#[cfg(test)]
mod tests {
    use super::*;

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
        registry.bind_run("session", "owner");
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
