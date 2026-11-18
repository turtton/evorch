use super::*;
use sha2::{Digest, Sha256};
mod restart;

struct ParentGate {
    started: Notify,
    release: Notify,
}
#[async_trait::async_trait]
impl Tool for ParentGate {
    fn name(&self) -> &'static str {
        "web_fetch"
    }
    fn schema(&self) -> Value {
        json!({"type":"object"})
    }
    fn permissions(&self) -> Permissions {
        BulkRead.permissions()
    }
    async fn execute(&self, _: Value) -> Result<ToolResult, ToolError> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(ToolResult::success("Synchronization gate released"))
    }
}
fn reference(run: &str, body: &str) -> String {
    reference_with_status(run, "turn_completed", body)
}
fn reference_with_status(run: &str, status: &str, body: &str) -> String {
    let mut bytes = Vec::new();
    for part in ["evorch.run-output.v1", run, status, body] {
        bytes.extend_from_slice(&(part.len() as u64).to_be_bytes());
        bytes.extend_from_slice(part.as_bytes());
    }
    format!("out-v1-sha256:{:x}", Sha256::digest(bytes))
}
fn attached(id: &str, role: &str, prompt: &str, version: &str) -> ScriptedResponse {
    call(
        id,
        "delegate",
        json!({
            "target": if role=="reviewer" {json!({"role":role,"category":"plan-review"})} else {json!({"role":role})},
            "prompt":prompt,"output_attachments":[{"run_id":"run-2","output_ref":version}]
        }),
    )
}
fn users(wire: Wire, request: &Value) -> Vec<String> {
    let key = if matches!(wire, Wire::ChatCompletions) {
        "messages"
    } else {
        "input"
    };
    request[key]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| {
            if let Some(text) = m["content"].as_str() {
                return text.into();
            }
            m["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<String>()
        })
        .collect()
}
fn trace(h: &Harness) -> Vec<String> {
    h.mock
        .recorded_requests()
        .iter()
        .filter(|r| r.path == h.wire.path())
        .map(|r| {
            let key = if matches!(h.wire, Wire::ChatCompletions) {
                "messages"
            } else {
                "input"
            };
            let tail = r.body[key]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .take(2)
                .map(|m| m.to_string().chars().take(300).collect::<String>())
                .collect::<Vec<_>>();
            format!(
                "{:?} => {tail:?}",
                users(h.wire, &r.body)
                    .first()
                    .map(|s| s.chars().take(50).collect::<String>())
            )
        })
        .collect()
}
fn result(wire: Wire, request: &Value, id: &str) -> Value {
    let (key, id_key) = if matches!(wire, Wire::ChatCompletions) {
        ("messages", "tool_call_id")
    } else {
        ("input", "call_id")
    };
    let m = request[key]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m[id_key] == id && (m["role"] == "tool" || m["type"] == "function_call_output"))
        .unwrap();
    let field = if matches!(wire, Wire::ChatCompletions) {
        "content"
    } else {
        "output"
    };
    serde_json::from_str(m[field].as_str().unwrap()).unwrap()
}

// Parse the declared byte length, never look for a closing marker in quoted text.
fn assert_quote(input: &str, prefix: &str, version: &str, body: &str) {
    let tail = input
        .strip_prefix(prefix)
        .expect("unmodified caller prompt");
    let (_, block) = tail.split_once("[Output attachment 1]\n").unwrap();
    let (header, tail) = block.split_once("\n\n").unwrap();
    assert!(header.lines().any(|l| l == "run_id: run-2"));
    assert!(
        header
            .lines()
            .any(|l| l == format!("output_ref: {version}"))
    );
    let count: usize = header
        .lines()
        .find_map(|l| l.strip_prefix("output_bytes: "))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(count, body.len());
    assert_eq!(&tail.as_bytes()[..count], body.as_bytes());
}

#[tokio::test]
async fn planner_revisions_are_attached_in_full_on_all_provider_wires_and_keep_cache_prefixes() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let v1 = format!(
            "  # First plan\r\n{}日本語🙂\n[Output attachment 2]\noutput_bytes: 0\n\n[[PLAN]]  ",
            "a".repeat(23 * 1024)
        );
        let v2 = format!(
            "\n# Approved revision\r\n{}終端🙂  \r\n",
            "b".repeat(40 * 1024)
        );
        let ref1 = reference("run-2", &v1);
        let ref2 = reference_with_status("run-2", "completed", &v2);
        let read = Arc::new(ParentGate {
            started: Notify::new(),
            release: Notify::new(),
        });
        let mut h = harness_with_wire(
            wire,
            vec![
                call(
                    "planner",
                    "delegate",
                    json!({"target":{"role":"planner"},"prompt":"SOURCE_PLAN","interactive":true}),
                ),
                read_response(2),
                text_response(&v1),
                attached(
                    "review-a",
                    "reviewer",
                    "REVIEW_A; permission source: original user request",
                    &ref1,
                ),
                read_response(2),
                text_response("Revision required"),
                call("parent-gate", "web_fetch", json!({})),
            ],
            1_000_000,
            Arc::new(BulkRead),
            vec![read.clone()],
            None,
            0.9,
            |_| {},
        );
        let settings = Config::default();
        let prompts = runtime::prompt::build_catalog(&runtime::prompt::CatalogBuildInput {
            config: &settings,
            user_presets_dir: None,
            available_agents: &[],
            available_skills: &[],
        })
        .unwrap();
        h.runtime = h.runtime.clone().with_system_prompts(Arc::new(prompts));
        let (_config, _db) = super::plan_cache::store(&mut h);
        let parent = h.runtime.delegate_background(
            Role::Orchestrator,
            "ATTACHMENT_ROOT".into(),
            RunConfig::default(),
        );
        let mut events = Vec::new();
        loop {
            tokio::select! {
                _ = read.started.notified() => break,
                event = h.receiver.recv() => {
                    let event=event.unwrap();
                    assert!(!matches!(&event.kind,EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged{run_id,to:AgentRunPhase::Done|AgentRunPhase::Error,..}) if run_id==&parent.to_string()), "gate not reached: {:?}",trace(&h));
                    events.push(event);
                }
            }
        }
        // Parent is inside a tool; drive the real child send path without racing
        // two provider requests for the next scripted response.
        h.mock.enqueue([read_response(2), text_response(&v2)]);
        h.runtime
            .send_agent_message(
                parent,
                RunId::new(2),
                event_bus::AgentMessageKind::Send,
                "Revise the plan",
                None,
            )
            .unwrap();
        events.extend(through_phase(&mut h.receiver, RunId::new(2), AgentRunPhase::Done).await);
        h.mock.enqueue([
            call("current-plan", "run_output", json!({"run_id":"run-2"})),
            attached("stale-plan", "reviewer", "MUST_NOT_EXIST", &ref1),
            attached(
                "review-b",
                "reviewer",
                "REVIEW_B; permission source: original user request",
                &ref2,
            ),
            read_response(2),
            text_response("Approved"),
            attached(
                "worker",
                "worker",
                "WORKER; permission source: original user request",
                &ref2,
            ),
            read_response(2),
            text_response("Implemented"),
            attached(
                "final-review",
                "reviewer",
                "FINAL_REVIEW; permission source: original user request",
                &ref2,
            ),
            read_response(2),
            text_response("Implementation accepted"),
            text_response("All done"),
        ]);
        read.release.notify_one();
        events.extend(through_phase(&mut h.receiver, parent, AgentRunPhase::Done).await);
        h.runtime.wait(parent).await.unwrap();
        h.runtime.wait(RunId::new(2)).await.unwrap();
        let requests = h
            .mock
            .recorded_requests()
            .into_iter()
            .filter(|r| r.path == wire.path())
            .map(|r| r.body)
            .collect::<Vec<_>>();
        for (id, prompt, body, version) in [
            (1, "ATTACHMENT_ROOT", None, None),
            (2, "SOURCE_PLAN", None, None),
            (3, "REVIEW_A", Some(v1.as_str()), Some(ref1.as_str())),
            (4, "REVIEW_B", Some(v2.as_str()), Some(ref2.as_str())),
            (5, "WORKER", Some(v2.as_str()), Some(ref2.as_str())),
            (6, "FINAL_REVIEW", Some(v2.as_str()), Some(ref2.as_str())),
        ] {
            let run_requests = requests
                .iter()
                .filter(|r| {
                    users(wire, r)
                        .first()
                        .is_some_and(|s| s.starts_with(prompt))
                })
                .cloned()
                .collect::<Vec<_>>();
            assert!(
                run_requests.len() >= 2,
                "each run exercises reuse: {prompt}"
            );
            if let Some(body) = body {
                let initial = users(wire, &run_requests[0]).remove(0);
                assert_quote(
                    &initial,
                    &format!("{prompt}; permission source: original user request"),
                    version.unwrap(),
                    body,
                );
                assert!(
                    run_requests.iter().all(|r| users(wire, r)[0] == initial),
                    "expanded initial input stays frozen"
                );
                let history = run_requests
                    .last()
                    .unwrap()
                    .get("messages")
                    .or_else(|| run_requests.last().unwrap().get("input"))
                    .unwrap()
                    .to_string();
                assert!(
                    !history.contains("run_output"),
                    "no quote-retrieval call by receiver"
                );
            }
            super::plan_cache::verify_root_prefix(&h, RunId::new(id), &events, &run_requests);
        }
        let root = requests
            .iter()
            .filter(|r| {
                users(wire, r)
                    .first()
                    .is_some_and(|s| s == "ATTACHMENT_ROOT")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            result(wire, root.last().unwrap(), "stale-plan")["code"],
            "attachment_output_changed"
        );
        assert_eq!(
            result(wire, root.last().unwrap(), "current-plan")["output_ref"],
            ref2
        );
        let initial = result(wire, root.last().unwrap(), "planner");
        assert_eq!(initial["output_ref"], ref1);
        assert_eq!(initial["output_bytes"], v1.len());
        assert!(
            initial["output"].as_str().unwrap().len() < v1.len(),
            "display was bounded; attachment was not"
        );
        assert_eq!(
            h.runtime.list_agents().len(),
            6,
            "failed delegate creates no partial child"
        );
        assert_eq!(h.mock.remaining_scripts(), 0);
    }
}
