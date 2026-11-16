//! Old wait catalog -> unified wait is a tool-only, one-request cache boundary.
//! The two old ToolSpecs are copied without semantic changes from mixed.intent.json,
//! produced by main 2eec1163e225272b2722f4796d1522fce758bfa0 (wait-fixtures/README).
//! Only catalog selection is emulated: request encoding, HTTP, input-derived cache,
//! checkpoint capture, handoff evidence and recovery activation remain production paths.
//! The retired call intentionally receives an error from the current runtime: this
//! test protects sent history, not old handler execution (covered by recovery fixtures).
use super::*;
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{AgentInvocationContext, RuntimeError};
use std::sync::Mutex;
use storage::recovery::restart::tools::RestartToolsEvidence;

#[derive(Default)]
pub(super) struct PredecessorCatalog {
    pub successor_tools: Mutex<Vec<ToolSpec>>,
}
impl PredecessorCatalog {
    fn tools(&self, successor: &[ToolSpec]) -> Vec<ToolSpec> {
        *self.successor_tools.lock().unwrap() = successor.to_vec();
        let path = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("tests/recovery_activation_cache/predecessor-wait-tools.json");
        let old: Vec<ToolSpec> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let mut tools = successor.to_vec();
        let wait = tools.iter_mut().find(|tool| tool.name == "wait").unwrap();
        *wait = old.iter().find(|tool| tool.name == "wait").unwrap().clone();
        tools.push(
            old.into_iter()
                .find(|tool| tool.name == "wait_reply")
                .unwrap(),
        );
        tools
    }
}

pub(super) struct PredecessorModel {
    pub inner: Arc<dyn AgentModel>,
    pub catalog: Arc<PredecessorCatalog>,
}
#[async_trait::async_trait]
impl AgentModel for PredecessorModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.inner
            .complete(invocation, role, messages, &self.catalog.tools(tools))
            .await
    }
    async fn complete_streaming(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
        bus: &EventBus,
    ) -> Result<ChatResponse, RuntimeError> {
        self.inner
            .complete_streaming(invocation, role, messages, &self.catalog.tools(tools), bus)
            .await
    }
    fn selected_model(&self, role: Role, category: Option<&str>) -> String {
        self.inner.selected_model(role, category)
    }
    fn selected_model_for_run(&self, run: &str, role: Role, category: Option<&str>) -> String {
        self.inner.selected_model_for_run(run, role, category)
    }
    fn checkpoint_provider_input(&self, run: &str) -> Option<runtime::quiesce::ProviderInput> {
        self.inner.checkpoint_provider_input(run)
    }
    fn route_pins(&self, run: &str) -> Vec<runtime::RoutePinRecord> {
        self.inner.route_pins(run)
    }
}

#[tokio::test]
async fn chat_planned_wait_schema_boundary_keeps_history_and_reuses_prefix() {
    check(Wire::ChatCompletions).await;
}
#[tokio::test]
async fn responses_planned_wait_schema_boundary_keeps_history_and_reuses_prefix() {
    check(Wire::Responses).await;
}
#[tokio::test]
async fn codex_planned_wait_schema_boundary_keeps_history_and_reuses_prefix() {
    check(Wire::Codex).await;
}
async fn check(wire: Wire) {
    for different_build in [false, true] {
        recover_case(false, true, different_build, false, wire, true).await;
    }
}

pub(super) fn assert_evidence(e: &RestartToolsEvidence) {
    assert!(e.boundary);
    assert_ne!(e.old_build, e.new_build);
    assert_ne!(e.old_catalog_fingerprint, e.new_catalog_fingerprint);
    assert_ne!(e.old_effective_fingerprint, e.new_effective_fingerprint);
    assert_eq!(e.old.stable_context, e.new.stable_context);
}

/// Independent protocol projection: never use the production encoder or sort the
/// captured request. The protocol's declared name order is part of the assertion.
pub(super) fn wire_tools(wire: Wire, tools: &[ToolSpec]) -> Value {
    let mut ordered = tools.iter().collect::<Vec<_>>();
    ordered.sort_by(|a, b| a.name.cmp(&b.name));
    Value::Array(
        ordered
            .into_iter()
            .map(|tool| match wire {
                Wire::ChatCompletions => json!({"type":"function", "function":{
            "name":tool.name, "description":tool.description, "parameters":tool.input_schema}}),
                Wire::Responses => json!({"type":"function", "name":tool.name,
            "description":tool.description, "parameters":tool.input_schema, "strict":false}),
                Wire::Codex => json!({"type":"function", "name":tool.name,
            "description":tool.description, "parameters":tool.input_schema}),
            })
            .collect(),
    )
}

pub(super) fn assert_transition(
    wire: Wire,
    requests: &[Value],
    boundary: usize,
    old: &[ToolSpec],
    new: &[ToolSpec],
) {
    let before = &requests[boundary - 1];
    let after = &requests[boundary];
    assert_eq!(before["tools"], wire_tools(wire, old));
    assert_eq!(after["tools"], wire_tools(wire, new));
    let old_wait = old.iter().find(|tool| tool.name == "wait").unwrap();
    let new_wait = new.iter().find(|tool| tool.name == "wait").unwrap();
    assert_ne!(old_wait.input_schema, new_wait.input_schema);
    assert!(old.iter().any(|tool| tool.name == "wait_reply"));
    assert!(!new.iter().any(|tool| tool.name == "wait_reply"));
    // Only these wait tools change. All unrelated definitions and their relative
    // order must survive; this is stronger than checking tool-name membership.
    let unrelated = |tools: &[ToolSpec]| {
        tools
            .iter()
            .filter(|tool| tool.name != "wait" && tool.name != "wait_reply")
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(unrelated(old), unrelated(new));
    for pair in requests[..boundary].windows(2) {
        assert_append_only(wire.protocol(), &pair[0], &pair[1]).unwrap();
    }
    let history = if matches!(wire, Wire::ChatCompletions) {
        "messages"
    } else {
        "input"
    };
    let prefix = before[history].as_array().unwrap();
    for request in &requests[boundary..] {
        assert_eq!(
            &request[history].as_array().unwrap()[..prefix.len()],
            prefix
        );
        // Prefix equality includes exact function argument/output strings, not
        // just their parsed JSON meaning. Renaming or reformatting them fails.
        let stable = |v: &Value| {
            let mut v = v.as_object().unwrap().clone();
            v.remove("tools");
            v.remove(history);
            v
        };
        assert_eq!(stable(before), stable(request));
    }
    if matches!(wire, Wire::ChatCompletions) {
        let call = prefix
            .iter()
            .find_map(|m| {
                m["tool_calls"]
                    .as_array()?
                    .iter()
                    .find(|c| c["id"] == "legacy-wait-reply")
            })
            .unwrap();
        assert_eq!(call["function"]["name"], "wait_reply");
        assert_eq!(
            serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap(),
            json!({"message_id":"msg-old", "timeout_ms":0})
        );
        assert_eq!(
            prefix
                .iter()
                .filter(|m| m["role"] == "tool" && m["tool_call_id"] == "legacy-wait-reply")
                .count(),
            1
        );
    } else {
        let call = prefix
            .iter()
            .find(|m| m["type"] == "function_call" && m["call_id"] == "legacy-wait-reply")
            .unwrap();
        assert_eq!(call["name"], "wait_reply");
        assert_eq!(
            serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(),
            json!({"message_id":"msg-old", "timeout_ms":0})
        );
        assert_eq!(
            prefix
                .iter()
                .filter(
                    |m| m["type"] == "function_call_output" && m["call_id"] == "legacy-wait-reply"
                )
                .count(),
            1
        );
    }
    assert_append_only(wire.protocol(), after, &requests[boundary + 1]).unwrap();
}
