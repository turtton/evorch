//! Access reviews that belong to a tool call: out-of-sandbox shell escalation
//! verdicts and network access decisions for web tools.

use event_bus::DiagnosticEvent;
use serde_json::Value;

use super::{ToolStatus, TranscriptEntry, TranscriptModel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolReview {
    /// Runtime verdict text such as `approved` or `denied: <reason>`.
    Sandbox { verdict: String },
    Network {
        allowed: bool,
        reviewer: String,
        reason: String,
    },
}

impl ToolReview {
    pub fn summary(&self) -> String {
        match self {
            Self::Sandbox { verdict } => {
                if verdict == "approved" {
                    "Sandbox escape reviewed: approved".into()
                } else if verdict == event_bus::event::ESCALATION_VERDICT_YOLO {
                    "Sandbox escape approved without review (yolo mode)".into()
                } else if let Some(reason) = verdict.strip_prefix("denied: ") {
                    format!("Sandbox escape reviewed: denied: {reason}")
                } else if let Some(reason) =
                    verdict.strip_prefix("automatic review declined; requesting user approval")
                {
                    let reason = reason.trim_start_matches(':').trim();
                    let mut text =
                        "Sandbox escape: automatic review declined; asking you".to_owned();
                    if !reason.is_empty() {
                        text.push_str(" — ");
                        text.push_str(reason);
                    }
                    text
                } else {
                    format!("Sandbox escape review: {verdict}")
                }
            }
            Self::Network {
                allowed: true,
                reviewer,
                ..
            } => match reviewer.as_str() {
                "tool_policy" => "Network access: allowed by policy".into(),
                "auto_review" => "Network access: approved by auto review".into(),
                "user_review" => "Network access: approved by you".into(),
                other => format!("Network access: allowed ({other})"),
            },
            Self::Network {
                allowed: false,
                reviewer,
                reason,
            } => {
                let (by, reason) = match reviewer.as_str() {
                    "tool_policy" => (" by policy", reason.as_str()),
                    "auto_review" => (" by auto review", reason.as_str()),
                    "user_review" => match reason.strip_prefix("approval denied: ") {
                        Some(reason) => (" by you", reason),
                        None => ("", reason.as_str()),
                    },
                    _ => ("", reason.as_str()),
                };
                if reason.is_empty() {
                    format!("Network access: denied{by}")
                } else {
                    format!("Network access: denied{by}: {reason}")
                }
            }
        }
    }

    pub fn needs_attention(&self) -> bool {
        match self {
            Self::Sandbox { verdict } => {
                verdict != "approved" && verdict != event_bus::event::ESCALATION_VERDICT_YOLO
            }
            Self::Network { allowed, .. } => !allowed,
        }
    }
}

struct NetworkCall {
    tool: String,
    input: Option<Value>,
    denied: Option<String>,
}

fn parse(event: &DiagnosticEvent) -> Option<(ToolReview, Option<NetworkCall>)> {
    match (event.source.as_str(), event.code.as_str()) {
        ("sandbox", event_bus::event::diagnostic_codes::ESCALATION_REVIEW) => {
            // Older diagnostic text included the IDs as well as structured fields.
            let verdict = event
                .detail
                .strip_prefix("run_id=")
                .and_then(|detail| detail.split_once(": ").map(|(_, verdict)| verdict))
                .unwrap_or(&event.detail);
            Some((
                ToolReview::Sandbox {
                    verdict: verdict.to_owned(),
                },
                None,
            ))
        }
        ("network_access", event_bus::event::diagnostic_codes::TOOL_CALL_ACCESS) => {
            let detail: Value = serde_json::from_str(&event.detail).ok()?;
            let text = |key: &str| {
                detail
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let allowed = match detail.get("decision")?.as_str()? {
                "allow" => true,
                "deny" => false,
                _ => return None,
            };
            let reason = text("reason");
            Some((
                ToolReview::Network {
                    allowed,
                    reviewer: text("reviewer"),
                    reason: reason.clone(),
                },
                Some(NetworkCall {
                    tool: text("tool"),
                    input: detail.get("input").cloned(),
                    denied: (!allowed).then_some(reason),
                }),
            ))
        }
        _ => None,
    }
}

impl TranscriptModel {
    /// Attaches a review to its tool card. Returns false for other diagnostics.
    pub(super) fn apply_review(&mut self, event: &DiagnosticEvent) -> bool {
        let Some((review, network)) = parse(event) else {
            return false;
        };
        let run_id = event.run_id.as_deref();
        if let Some(call_id) = event.call_id.as_deref() {
            if let Some(TranscriptEntry::Tool { reviews, .. }) =
                self.find_scoped_tool_mut(call_id, run_id)
            {
                reviews.push(review);
                return true;
            }
            match network {
                // Network decisions precede ToolStarted; a denied call never starts.
                Some(NetworkCall {
                    tool,
                    input,
                    denied: Some(reason),
                }) => {
                    self.push(TranscriptEntry::Tool {
                        run_id: run_id.map(str::to_owned),
                        tool_name: tool,
                        call_id: call_id.to_owned(),
                        input,
                        output: None,
                        detail: None,
                        is_error: false,
                        status: ToolStatus::Denied { reason },
                        reviews: vec![review],
                        parent_call_id: None,
                    });
                    return true;
                }
                Some(_) => {
                    self.pending_reviews
                        .entry((run_id.map(str::to_owned), call_id.to_owned()))
                        .or_default()
                        .push(review);
                    return true;
                }
                None => {}
            }
        }
        // Restored history may hold a review whose tool call is gone.
        self.push(TranscriptEntry::SandboxReview {
            text: review.summary(),
            run_id: event.run_id.clone(),
            call_id: event.call_id.clone(),
        });
        true
    }

    pub(super) fn take_pending_reviews(
        &mut self,
        run_id: Option<&str>,
        call_id: &str,
    ) -> Vec<ToolReview> {
        self.pending_reviews
            .remove(&(run_id.map(str::to_owned), call_id.to_owned()))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(verdict: &str) -> ToolReview {
        ToolReview::Sandbox {
            verdict: verdict.into(),
        }
    }

    fn network(allowed: bool, reviewer: &str, reason: &str) -> ToolReview {
        ToolReview::Network {
            allowed,
            reviewer: reviewer.into(),
            reason: reason.into(),
        }
    }

    #[test]
    fn summaries_name_the_decision_and_who_made_it() {
        for (review, expected, attention) in [
            (
                sandbox("approved"),
                "Sandbox escape reviewed: approved",
                false,
            ),
            (
                sandbox(event_bus::event::ESCALATION_VERDICT_YOLO),
                "Sandbox escape approved without review (yolo mode)",
                false,
            ),
            (
                sandbox("denied: prohibited command"),
                "Sandbox escape reviewed: denied: prohibited command",
                true,
            ),
            (
                sandbox("automatic review declined; requesting user approval: model unavailable"),
                "Sandbox escape: automatic review declined; asking you — model unavailable",
                true,
            ),
            (
                network(true, "tool_policy", ""),
                "Network access: allowed by policy",
                false,
            ),
            (
                network(true, "auto_review", "approved"),
                "Network access: approved by auto review",
                false,
            ),
            (
                network(true, "user_review", "approved"),
                "Network access: approved by you",
                false,
            ),
            (
                network(false, "user_review", "approval denied: needs network"),
                "Network access: denied by you: needs network",
                true,
            ),
            (
                network(false, "tool_policy", "web tools are disabled"),
                "Network access: denied by policy: web tools are disabled",
                true,
            ),
        ] {
            assert_eq!(review.summary(), expected);
            assert_eq!(review.needs_attention(), attention, "{expected}");
        }
    }

    fn diagnostic(source: &str, code: &str, detail: String, call: &str) -> event_bus::Event {
        event_bus::Event::new(DiagnosticEvent {
            source: source.into(),
            severity: event_bus::DiagnosticSeverity::Info,
            code: code.into(),
            detail,
            run_id: Some("run-1".into()),
            thread_id: None,
            call_id: Some(call.into()),
        })
    }

    fn access(decision: &str, reviewer: &str, reason: &str) -> event_bus::Event {
        let detail = serde_json::json!({
            "tool": "web_fetch", "input": {"url": "https://example.com"},
            "decision": decision, "reviewer": reviewer, "reason": reason,
        });
        diagnostic(
            "network_access",
            "tool_call_access",
            detail.to_string(),
            "call-1",
        )
    }

    fn started(tool: &str) -> event_bus::Event {
        event_bus::Event::new(event_bus::ToolEvent::ToolStarted {
            tool_name: tool.into(),
            call_id: "call-1".into(),
            input: None,
            run_id: Some("run-1".into()),
            parent_call_id: None,
        })
    }

    fn reviews(model: &TranscriptModel) -> Vec<(&str, &ToolStatus, Vec<String>)> {
        model
            .entries()
            .iter()
            .map(|entry| match entry {
                TranscriptEntry::Tool {
                    tool_name,
                    status,
                    reviews,
                    ..
                } => (
                    tool_name.as_str(),
                    status,
                    reviews.iter().map(ToolReview::summary).collect(),
                ),
                other => panic!("unexpected entry {other:?}"),
            })
            .collect()
    }

    #[test]
    fn sandbox_verdicts_attach_to_the_matching_shell_call() {
        // Given: a running shell call.
        let mut model = TranscriptModel::new();
        model.apply(&started("shell"));
        // When: automatic review declines and the user then denies.
        for verdict in [
            "automatic review declined; requesting user approval: unclear",
            "denied: shell escalation denied by user",
        ] {
            model.apply(&diagnostic(
                "sandbox",
                "escalation_review",
                verdict.into(),
                "call-1",
            ));
        }
        // Then: both verdicts belong to the one shell card.
        assert_eq!(
            reviews(&model),
            [(
                "shell",
                &ToolStatus::Running,
                vec![
                    "Sandbox escape: automatic review declined; asking you — unclear".to_owned(),
                    "Sandbox escape reviewed: denied: shell escalation denied by user".to_owned(),
                ]
            )]
        );
    }

    #[test]
    fn network_decisions_wait_for_their_call_or_stand_in_for_a_denied_one() {
        // Given: an allowed fetch is reviewed before it starts.
        let mut allowed = TranscriptModel::new();
        allowed.apply(&access("allow", "tool_policy", ""));
        assert!(allowed.entries().is_empty());
        // When: the call starts.
        allowed.apply(&started("web_fetch"));
        // Then: the decision is on its card rather than a raw JSON notice.
        assert_eq!(
            reviews(&allowed),
            [(
                "web_fetch",
                &ToolStatus::Running,
                vec!["Network access: allowed by policy".to_owned()]
            )]
        );

        // When: a fetch is denied and therefore never starts.
        let mut denied = TranscriptModel::new();
        denied.apply(&access("deny", "auto_review", "private host"));
        // Then: a compact denied card names the tool and the reason.
        assert_eq!(
            reviews(&denied),
            [(
                "web_fetch",
                &ToolStatus::Denied {
                    reason: "private host".into()
                },
                vec!["Network access: denied by auto review: private host".to_owned()]
            )]
        );
    }

    #[test]
    fn user_approved_network_call_keeps_one_card() {
        // Given: automatic review fell back to the user before the call started.
        let mut model = TranscriptModel::new();
        model.apply(&event_bus::Event::new(
            event_bus::ToolEvent::ApprovalRequested {
                tool_name: "web_fetch".into(),
                call_id: "run-1:call-1".into(),
                input: Some(serde_json::json!({"url": "https://example.com"})),
            },
        ));
        // When: the user approves and the call runs.
        model.apply(&event_bus::Event::new(
            event_bus::ToolEvent::ApprovalResolved {
                call_id: "run-1:call-1".into(),
                approved: true,
            },
        ));
        model.apply(&access("allow", "user_review", "approved"));
        model.apply(&started("web_fetch"));
        // Then: approval, decision and execution share a single card.
        assert_eq!(
            reviews(&model),
            [(
                "web_fetch",
                &ToolStatus::Running,
                vec!["Network access: approved by you".to_owned()]
            )]
        );
    }
}
