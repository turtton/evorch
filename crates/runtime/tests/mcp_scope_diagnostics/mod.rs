use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Failure {
    None,
    Server,
    Http(usize),
}

fn diagnostic(events: &[event_bus::Event], run: &str) -> serde_json::Value {
    let diagnostics = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Diagnostic(d) if d.source == "mcp" => Some(d),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(diagnostics.len(), 1);
    let d = diagnostics[0];
    assert_eq!(d.run_id.as_deref(), Some(run));
    assert_eq!(d.call_id.as_deref(), Some("mcp-call"));
    assert!(!d.detail.contains("secret-body-token"));
    let detail: serde_json::Value = serde_json::from_str(&d.detail).unwrap();
    assert_eq!(
        detail
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["class", "method", "request_id", "server", "status"]
    );
    assert_eq!(detail["server"], "fixture");
    detail
}

#[tokio::test]
async fn mcp_scope_emits_diagnostic_when_call_succeeds() {
    // Given/When: the real runtime executes an allowed MCP call.
    let (_, events, run) = scenario(Role::WebResearcher, Failure::None, None).await;
    // Then: the successful RPC metadata is correlated, not its response body.
    assert_eq!(
        diagnostic(&events, &run),
        json!({"server":"fixture","method":"tools/call","request_id":3,"class":"success","status":null})
    );
}

#[tokio::test]
async fn mcp_scope_emits_diagnostic_when_communication_fails() {
    for (index, method, id) in [
        (0, "initialize", 1),
        (2, "tools/list", 2),
        (3, "tools/call", 3),
    ] {
        // Given/When: the server fails a different stage of the allowed call.
        let (_, events, run) = scenario(Role::WebResearcher, Failure::Http(index), None).await;
        // Then: exactly one safe diagnostic accompanies the failed lifecycle pair.
        assert_eq!(
            diagnostic(&events, &run),
            json!({"server":"fixture","method":method,"request_id":id,"class":"http_status","status":503})
        );
        assert_eq!(events.iter().filter(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolStarted {call_id, run_id, ..}) if call_id == "mcp-call" && run_id.as_deref() == Some(&run))).count(), 1);
        assert_eq!(events.iter().filter(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolCompleted {call_id, run_id, is_error:true, ..}) if call_id == "mcp-call" && run_id.as_deref() == Some(&run))).count(), 1);
    }
}

#[tokio::test]
async fn mcp_scope_has_no_execution_lifecycle_when_denied() {
    // Given/When: scope denies the call before the adapter can run.
    let (server, events, _) = scenario_with_policy(
        Role::Worker,
        Failure::None,
        None,
        sandbox::PolicyDecision::Deny,
    )
    .await;
    // Then: denial has no Started/Completed events or network traffic.
    assert!(server.captured_requests().is_empty());
    assert!(!events.iter().any(|event| matches!(
        event.kind,
        EventKind::Tool(ToolEvent::ToolStarted { .. } | ToolEvent::ToolCompleted { .. })
    )));
}
