use event_bus::event::diagnostic_codes as code;
use event_bus::{
    DiagnosticEvent, EventKind, FaultEvent, ProviderEvent, ProviderFailureKind, SkillDiagnosticKind,
};

use super::TranscriptEntry;
use crate::model::failure_text;

pub(super) fn entry(kind: &EventKind) -> Option<TranscriptEntry> {
    match kind {
        EventKind::Diagnostic(event) if event.code == "ContextCheckpointSaved" => None,
        EventKind::Diagnostic(event) => Some(TranscriptEntry::Diagnostic {
            severity: event.severity,
            text: summary(event),
            hover: hover(event),
        }),
        EventKind::Provider(ProviderEvent::FallbackTriggered {
            from_provider,
            from_model,
            to_provider,
            to_model,
            failure,
            ..
        }) => Some(TranscriptEntry::Notice {
            text: format!(
                "Fallback: {from_provider}/{} → {to_provider}/{to_model} ({})",
                from_model.as_deref().unwrap_or("unknown"),
                failure_label(failure)
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
            let mut raw_changed = None;
            if let Some(detail) = detail {
                let shown = failure_text::english(detail);
                if shown != *detail {
                    raw_changed = Some(detail.clone());
                }
                text.push_str(": ");
                text.push_str(&shown);
            }
            Some(match (transient, raw_changed) {
                (true, None) => TranscriptEntry::Notice { text },
                (false, None) => TranscriptEntry::Error { text },
                (transient, Some(hover)) => TranscriptEntry::Diagnostic {
                    severity: if transient {
                        event_bus::DiagnosticSeverity::Warning
                    } else {
                        event_bus::DiagnosticSeverity::Error
                    },
                    text,
                    hover,
                },
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
            let shown = failure_text::english(detail);
            let text = format!("Skill {skill} ({scope}): {label} — {shown}");
            Some(if shown == *detail {
                TranscriptEntry::Error { text }
            } else {
                TranscriptEntry::Diagnostic {
                    severity: event_bus::DiagnosticSeverity::Error,
                    text,
                    hover: detail.clone(),
                }
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
        | EventKind::Orchestrator(_) => None,
    }
}

/// How much of a diagnostic's raw detail the hover keeps.
const HOVER_DETAIL_CHARS: usize = 1500;

/// How a diagnostic code reads in the transcript. The raw detail stays in the hover.
enum Wording {
    /// A sentence that stands on its own.
    Fixed(&'static str),
    /// A heading followed by the first line of the detail, which names the cause.
    Lead(&'static str),
    /// The first line of the detail, which the producer already writes for people.
    Detail,
}

fn wording(code: &str) -> Option<Wording> {
    use Wording::{Detail, Fixed, Lead};
    Some(match code {
        code::BUDGET_WARNING => Fixed("Approaching the budget limit"),
        code::BUDGET_EXHAUSTED => Fixed("Stopped: budget exhausted"),
        code::NO_PROGRESS => Fixed("No progress in recent tool calls"),
        code::IDENTICAL_TOOL_CALLS => Fixed("Stopped: the same tool call kept repeating"),
        code::DELEGATION_RETRACTED => {
            Fixed("Background subagents were cancelled right after delegation")
        }
        code::PROVIDER_UNAVAILABLE => Lead("Provider unavailable"),
        code::PROVIDER_MODEL_ROUTE => Fixed("Model route adjusted for the provider"),
        code::CACHE_REGRESSION => Fixed("Prompt cache reuse dropped"),
        code::TOOL_ARGUMENTS_MALFORMED => Fixed("A tool call had malformed arguments"),
        code::OWNERSHIP_REGISTRY_WAIT => Detail,
        code::COMPACTION_FAILED => Lead("Compaction failed"),
        code::LEARNING_PIPELINE_FAILED => Fixed("Lesson extraction or review failed"),
        code::CONTEXT_SNAPSHOT_FAILED => Fixed("Could not save the conversation checkpoint"),
        code::HOST_INPUT_RECEIPT_FAILED => Fixed("Input delivery receipts could not be settled"),
        code::ESCALATION_HANDOFF_FAILED => Fixed("Escalation could not be prepared"),
        code::ESCALATION_ADMISSION_FAILED => Fixed("The escalated Orchestrator did not start"),
        code::CRASH_RECOVERED => Fixed("Recovered from a crash in a previous session"),
        code::AGENT_RUN_PANICKED => Fixed("The run crashed and was stopped"),
        code::STORAGE_WRITER_HALTED => Fixed("History is no longer being saved"),
        code::SCOPE_DENIED => Lead("Tool call denied"),
        code::ESCALATION_REVIEW => Detail,
        code::TOOL_CALL_ACCESS => Fixed("Network access decision recorded"),
        code::PUBLISH_DIAGNOSTICS => Fixed("Language server reported diagnostics"),
        code::TOOL_RESULT => Fixed("MCP tool call reported a result"),
        code::EVALUATION_FINISHED => Detail,
        code::BROWSER_SESSION => Fixed("Browser session updated"),
        code::BROWSER_STOPPED => Fixed("Browser stopped"),
        code::BROWSER_ERROR => Fixed("Browser error"),
        code::BROWSER_CLOSE_ERROR => Fixed("Browser could not be closed cleanly"),
        code::BROWSER_ACTION => Fixed("Browser action performed"),
        code::BROWSER_ACTION_ERROR => Fixed("Browser action failed"),
        code::BROWSER_DOM_DIFF => Fixed("Page content changed"),
        code::BROWSER_SCREENSHOT => Fixed("Screenshot captured"),
        _ => return None,
    })
}

/// The one-line, user-facing text of a diagnostic. Unknown codes show their
/// detail when it reads as text, and a generic line when it is empty or
/// structured data.
fn summary(event: &DiagnosticEvent) -> String {
    if let Some(text) = structured(event) {
        return text;
    }
    let detail = event.detail.trim();
    let first_line = failure_text::english(detail.lines().next().unwrap_or_default().trim());
    match wording(&event.code) {
        Some(Wording::Fixed(text)) => text.to_owned(),
        Some(Wording::Lead(lead)) if !first_line.is_empty() => format!("{lead}: {first_line}"),
        Some(Wording::Lead(lead)) => lead.to_owned(),
        Some(Wording::Detail) if !first_line.is_empty() => first_line,
        Some(Wording::Detail) | None if is_plain_text(detail) => failure_text::english(detail),
        Some(Wording::Detail) | None => generic(event.severity).to_owned(),
    }
}

fn generic(severity: event_bus::DiagnosticSeverity) -> &'static str {
    match severity {
        event_bus::DiagnosticSeverity::Info => "A diagnostic was reported",
        event_bus::DiagnosticSeverity::Warning => "A warning was reported",
        event_bus::DiagnosticSeverity::Error => "An error was reported",
    }
}

/// Whether a detail is prose for people rather than empty or JSON.
fn is_plain_text(detail: &str) -> bool {
    !detail.is_empty() && !detail.starts_with('{') && !detail.starts_with('[')
}

/// Lines for diagnostics whose detail is a JSON object built by the producer
/// (language server and MCP results); the object itself stays in the hover.
fn structured(event: &DiagnosticEvent) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(event.detail.trim()).ok()?;
    let text = |key: &str| value.get(key).and_then(serde_json::Value::as_str);
    match event.code.as_str() {
        code::PUBLISH_DIAGNOSTICS => {
            let count = value.get("count")?.as_u64()?;
            let file = text("file")
                .map(|file| file.rsplit('/').next().unwrap_or(file))
                .filter(|file| !file.is_empty())
                .map_or_else(String::new, |file| format!(" in {file}"));
            Some(match count {
                0 => format!("Language server found no issues{file}"),
                1 => format!("Language server reported 1 issue{file}"),
                n => format!("Language server reported {n} issues{file}"),
            })
        }
        code::TOOL_RESULT => {
            let server = text("server")
                .filter(|server| !server.is_empty())
                .map_or_else(String::new, |server| format!(" to {server}"));
            Some(match text("class") {
                Some("success") => format!("MCP tool call{server} succeeded"),
                Some(class) => format!("MCP tool call{server} failed ({class})"),
                None => format!("MCP tool call{server} reported a result"),
            })
        }
        _ => None,
    }
}

/// Severity, source and code, then the raw detail, for the hover.
fn hover(event: &DiagnosticEvent) -> String {
    let mut text = format!(
        "[{}] {} ({})",
        event.severity.as_str(),
        event.source,
        event.code
    );
    let detail = event.detail.trim();
    if !detail.is_empty() {
        text.push('\n');
        match detail.char_indices().nth(HOVER_DETAIL_CHARS) {
            Some((end, _)) => {
                text.push_str(&detail[..end]);
                text.push('…');
            }
            None => text.push_str(detail),
        }
    }
    text
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

    fn diagnostic(severity: event_bus::DiagnosticSeverity, code: &str, detail: &str) -> EventKind {
        EventKind::Diagnostic(DiagnosticEvent {
            source: "src".into(),
            severity,
            code: code.into(),
            detail: detail.into(),
            run_id: None,
            thread_id: None,
            call_id: None,
        })
    }

    fn text_and_hover(kind: &EventKind) -> (String, String) {
        match entry(kind) {
            Some(TranscriptEntry::Diagnostic { text, hover, .. }) => (text, hover),
            other => panic!("expected a diagnostic entry, got {other:?}"),
        }
    }

    // Given: a code with a fixed sentence, a heading plus cause, and a producer-worded detail
    // When: shown / Then: the line is the short English text and the hover carries the metadata
    #[test]
    fn known_codes_map_to_short_text_with_metadata_in_the_hover() {
        use event_bus::DiagnosticSeverity::{Error, Warning};
        let (text, hover) = text_and_hover(&diagnostic(
            Warning,
            code::BUDGET_WARNING,
            "task t remaining_tool_calls=3",
        ));
        assert_eq!(text, "Approaching the budget limit");
        assert_eq!(
            hover,
            "[warning] src (BudgetWarning)\ntask t remaining_tool_calls=3"
        );

        let (text, _) = text_and_hover(&diagnostic(
            Error,
            code::PROVIDER_UNAVAILABLE,
            "kimi: HTTP 403\nsecond line",
        ));
        assert_eq!(text, "Provider unavailable: kimi: HTTP 403");

        let (text, _) = text_and_hover(&diagnostic(
            Warning,
            code::OWNERSHIP_REGISTRY_WAIT,
            "Waiting for the owner registry (locked for over 3 s)",
        ));
        assert_eq!(text, "Waiting for the owner registry (locked for over 3 s)");
    }

    // Given: a code the GUI has no wording for / When: shown / Then: its detail is the line
    #[test]
    fn unknown_codes_fall_back_to_the_detail() {
        let (text, hover) = text_and_hover(&diagnostic(
            event_bus::DiagnosticSeverity::Info,
            "claim_conflict",
            "owner changed",
        ));
        assert_eq!(text, "owner changed");
        assert_eq!(hover, "[info] src (claim_conflict)\nowner changed");

        let (text, hover) = text_and_hover(&diagnostic(
            event_bus::DiagnosticSeverity::Info,
            "odd_code",
            "",
        ));
        assert_eq!(text, "A diagnostic was reported");
        assert_eq!(hover, "[info] src (odd_code)");
    }

    // Given: the JSON details the LSP and MCP producers emit / When: shown
    // Then: a short English line, with the JSON and the code only in the hover
    #[test]
    fn structured_details_become_short_lines_with_the_json_in_the_hover() {
        use event_bus::DiagnosticSeverity::{Error, Info};
        let lsp = r#"{"file":"/repo/src/main.rs","count":2,"codes":[1,"[redacted]"]}"#;
        let (text, hover) = text_and_hover(&diagnostic(Error, code::PUBLISH_DIAGNOSTICS, lsp));
        assert_eq!(text, "Language server reported 2 issues in main.rs");
        assert_eq!(hover, format!("[error] src (publish_diagnostics)\n{lsp}"));
        let (text, _) = text_and_hover(&diagnostic(
            Info,
            code::PUBLISH_DIAGNOSTICS,
            r#"{"file":"/repo/a.rs","count":0,"codes":[]}"#,
        ));
        assert_eq!(text, "Language server found no issues in a.rs");

        let ok = r#"{"server":"docs","method":"tools/call","request_id":3,"class":"success","status":null}"#;
        let (text, hover) = text_and_hover(&diagnostic(Info, code::TOOL_RESULT, ok));
        assert_eq!(text, "MCP tool call to docs succeeded");
        assert!(hover.ends_with(ok));
        let failed = r#"{"server":"docs","method":"tools/call","request_id":4,"class":"timeout","status":null}"#;
        let (text, _) = text_and_hover(&diagnostic(Error, code::TOOL_RESULT, failed));
        assert_eq!(text, "MCP tool call to docs failed (timeout)");

        for code in [code::PUBLISH_DIAGNOSTICS, code::TOOL_RESULT] {
            let (text, _) = text_and_hover(&diagnostic(Info, code, "{not json"));
            assert!(!text.contains('{') && !text.is_empty(), "{code}: {text}");
        }
    }

    // Given: unknown codes with an empty or JSON detail / When: shown
    // Then: a generic English line, never the code or the JSON
    #[test]
    fn empty_or_structured_unknown_details_use_a_generic_line() {
        use event_bus::DiagnosticSeverity::{Error, Info, Warning};
        for (severity, expected) in [
            (Info, "A diagnostic was reported"),
            (Warning, "A warning was reported"),
            (Error, "An error was reported"),
        ] {
            let (text, hover) = text_and_hover(&diagnostic(severity, "odd_code", "  "));
            assert_eq!(text, expected);
            assert!(hover.contains("odd_code"));
            let (text, _) = text_and_hover(&diagnostic(severity, "odd_code", r#"{"a":1}"#));
            assert_eq!(text, expected);
        }
    }

    // Given: a Japanese runtime detail / When: shown / Then: English in the line, raw in the hover
    #[test]
    fn japanese_runtime_details_are_translated_and_kept_in_the_hover() {
        let raw = "モデル呼び出しに失敗しました: HTTP 500";
        let (text, hover) = text_and_hover(&diagnostic(
            event_bus::DiagnosticSeverity::Error,
            code::PROVIDER_UNAVAILABLE,
            raw,
        ));
        assert_eq!(text, "Provider unavailable: Model call failed: HTTP 500");
        assert!(hover.ends_with(raw));
    }

    // Given: a very long raw detail / When: shown / Then: the hover is capped
    #[test]
    fn hover_detail_is_capped() {
        let (_, hover) = text_and_hover(&diagnostic(
            event_bus::DiagnosticSeverity::Warning,
            code::AGENT_RUN_PANICKED,
            &"あ".repeat(HOVER_DETAIL_CHARS + 50),
        ));
        assert!(hover.ends_with('…'));
        assert_eq!(
            hover.chars().filter(|c| *c == 'あ').count(),
            HOVER_DETAIL_CHARS
        );
    }

    // Given: every code the event bus defines / When: shown / Then: the checkpoint marker
    // stays hidden and every other code yields a non-empty line.
    #[test]
    fn every_diagnostic_code_renders_without_panicking() {
        for code in code::ALL {
            if *code == code::CONTEXT_CHECKPOINT_SAVED {
                assert_eq!(
                    entry(&diagnostic(event_bus::DiagnosticSeverity::Info, code, "x")),
                    None
                );
                continue;
            }
            let (text, _) =
                text_and_hover(&diagnostic(event_bus::DiagnosticSeverity::Info, code, "x"));
            assert!(!text.is_empty());
        }
    }

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
                text: "Fallback: kimi/k3 → neuralwatt/kimi-k3 (timed out)".into(),
            }]
        );
        assert!(registry.thread().entries().is_empty());
        registry.select_thread(Some("owner".into()));
        assert_eq!(registry.thread().entries().len(), 1);
    }
}
