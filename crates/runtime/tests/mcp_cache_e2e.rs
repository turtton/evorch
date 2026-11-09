//! MCP frozen catalogs are a cost-correctness contract, through actual provider wire.
mod mcp_cache_support;
use event_bus::{AgentRunPhase, CompactionEvent, CompactionReason, EventKind};
use mcp_cache_support::*;
use mock_openai::cache_contract::{assert_append_only, assert_compaction_transition};
use runtime::mcp::McpGuiService;
use serde_json::json;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn mcp_multiple_tool_turns_preserve_frozen_wire_and_provider_names() {
    for subscription in [false, true] {
        let project = Project::new();
        let provider = Provider::new(subscription).await;
        let hash = hashed_name();
        provider.enqueue(vec![call(READ), call(&hash), answer()]);
        let mut h = harness(&project, &configuration(&provider));
        let run = start(&h);
        let events = through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        let requests = provider.requests();
        assert_eq!(requests.len(), 3);
        assert_frozen(&provider, &requests);
        assert_usage_reuse(&events, 3);
        assert_eq!(*project.peer.calls.lock().unwrap(), ["read", "odd.tool"]);
        assert_eq!(project.peer.lists.load(Ordering::SeqCst), 1);
        let tools = external_tools(&provider, &requests[0]);
        assert_eq!(tools.len(), 3);
        for (name, remote) in [(READ, "read"), (hash.as_str(), "odd.tool")] {
            let wire_name = provider.tool_name(name);
            let tool = tools
                .iter()
                .find(|t| t["name"] == wire_name || t["function"]["name"] == wire_name)
                .unwrap();
            let (description, schema) = if subscription {
                (&tool["description"], &tool["input_schema"])
            } else {
                (
                    &tool["function"]["description"],
                    &tool["function"]["parameters"],
                )
            };
            assert_eq!(*description, format!("Remote {remote} documentation"));
            assert_eq!(
                *schema,
                definitions()
                    .into_iter()
                    .find(|d| d["name"] == remote)
                    .unwrap()["inputSchema"]
            );
        }
        assert!(
            requests[2]["messages"]
                .to_string()
                .contains("remote-ok-odd.tool")
        );
        provider.assert_drained();
        stop(&mut h, run).await;
    }
}

#[tokio::test]
async fn mcp_discovery_order_does_not_change_snapshot() {
    for subscription in [false, true] {
        let project = Project::new();
        let provider = Provider::new(subscription).await;
        let mut h = harness(&project, &configuration(&provider));
        let mut catalogs = vec![];
        for _ in 0..2 {
            provider.enqueue(vec![call(READ), answer()]);
            let start_index = provider.requests().len();
            let run = start(&h);
            through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
            catalogs.push(provider.requests()[start_index]["tools"].clone());
            stop(&mut h, run).await;
            project.peer.tools.lock().unwrap().reverse();
        }
        assert_eq!(catalogs[0], catalogs[1]);
        assert_eq!(project.peer.lists.load(Ordering::SeqCst), 2);
        provider.assert_drained();
    }
}

#[tokio::test]
async fn mcp_live_changes_and_new_run_alone_refreshes_tools() {
    for subscription in [false, true] {
        let project = Project::new();
        let provider = Provider::new(subscription).await;
        provider.enqueue(vec![call(READ), answer()]);
        let mut h = harness(&project, &configuration(&provider));
        let run = start(&h);
        let mut events = through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        project
            .peer
            .tools
            .lock()
            .unwrap()
            .push(json!({"name":"new_tool","inputSchema":{"type":"object"}}));
        project.peer.changed.store(true, Ordering::SeqCst);
        // A notification is consumed by the real MCP client on this explicit call.
        // No new discovery is allowed, even after a new approval and secret rotation.
        project.rotate_secret("secret-generation-two");
        project.write_config("roles=[\"worker\",\"planner\"]");
        project.approve();
        provider.enqueue(vec![call(READ), answer()]);
        h.runtime
            .send_message(run, "Continue with the frozen tools".into())
            .unwrap();
        events.extend(through(&mut h.receiver, run, AgentRunPhase::Waiting).await);
        let requests = provider.requests();
        assert_frozen(&provider, &requests);
        assert_usage_reuse(&events, 4);
        assert_eq!(project.peer.notifications.load(Ordering::SeqCst), 1);
        assert_eq!(project.peer.lists.load(Ordering::SeqCst), 1);
        let old_tools = external_tools(&provider, &requests[0]);
        assert_eq!(old_tools.len(), 3);
        stop(&mut h, run).await;
        provider.enqueue(vec![call(READ), answer()]);
        let fresh = start(&h);
        through(&mut h.receiver, fresh, AgentRunPhase::Waiting).await;
        let requests = provider.requests();
        assert_eq!(external_tools(&provider, &requests[4]).len(), 4);
        assert_eq!(&requests[4]["tools"], &requests[5]["tools"]);
        assert_eq!(project.peer.lists.load(Ordering::SeqCst), 2);
        provider.assert_drained();
        stop(&mut h, fresh).await;
    }
}

#[tokio::test]
async fn mcp_approval_recovery_only_adds_tools_to_a_new_run() {
    for subscription in [false, true] {
        let project = Project::new();
        project
            .service
            .revoke_approval(project.scope())
            .await
            .unwrap();
        let provider = Provider::new(subscription).await;
        provider.enqueue(vec![answer()]);
        let mut h = harness(&project, &configuration(&provider));
        let run = start(&h);
        let mut events = through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        assert_eq!(project.peer.connections.load(Ordering::SeqCst), 0);
        project.approve();
        provider.enqueue(vec![answer()]);
        h.runtime
            .send_message(run, "The server is approved now".into())
            .unwrap();
        events.extend(through(&mut h.receiver, run, AgentRunPhase::Waiting).await);
        let requests = provider.requests();
        assert_frozen(&provider, &requests);
        assert_usage_reuse(&events, 2);
        assert!(external_tools(&provider, &requests[0]).is_empty());
        assert_eq!(project.peer.connections.load(Ordering::SeqCst), 0);
        stop(&mut h, run).await;
        provider.enqueue(vec![call(READ), answer()]);
        let fresh = start(&h);
        through(&mut h.receiver, fresh, AgentRunPhase::Waiting).await;
        let requests = provider.requests();
        assert_eq!(external_tools(&provider, &requests[2]).len(), 3);
        assert_eq!(project.peer.connections.load(Ordering::SeqCst), 1);
        provider.assert_drained();
        stop(&mut h, fresh).await;
    }
}

#[tokio::test]
async fn mcp_cancel_and_revoke_preserve_sent_prefix() {
    for subscription in [false, true] {
        for revoke in [false, true] {
            let project = Project::new();
            let provider = Provider::new(subscription).await;
            provider.enqueue(vec![call(READ), answer()]);
            let mut h = harness(&project, &configuration(&provider));
            let run = start(&h);
            let mut events = through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
            if revoke {
                project
                    .service
                    .revoke_approval(project.scope())
                    .await
                    .unwrap();
            } else {
                project.write_config("args=[\"changed-launch-argument\"]");
            }
            provider.enqueue(vec![call(READ), answer()]);
            h.runtime
                .send_message(run, "Attempt the same tool again".into())
                .unwrap();
            events.extend(through(&mut h.receiver, run, AgentRunPhase::Waiting).await);
            let requests = provider.requests();
            assert_frozen(&provider, &requests);
            assert_usage_reuse(&events, 4);
            assert_eq!(
                *project.peer.calls.lock().unwrap(),
                ["read"],
                "revoked call must not reach MCP peer"
            );
            assert!(
                requests[3]["messages"]
                    .to_string()
                    .contains("needs_approval")
            );
            assert_eq!(project.peer.connections.load(Ordering::SeqCst), 1);
            provider.assert_drained();
            stop(&mut h, run).await;
        }
    }
}

#[tokio::test]
async fn mcp_compaction_preserves_post_boundary_prefix() {
    for subscription in [false, true] {
        let project = Project::new();
        let provider = Provider::new(subscription).await;
        provider.enqueue(vec![call(READ), answer()]);
        let mut h = harness(&project, &configuration(&provider));
        let run = start(&h);
        through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        h.runtime.compact(run).unwrap();
        provider.enqueue(vec![call(READ), answer()]);
        h.runtime
            .send_message(run, "Continue after compaction".into())
            .unwrap();
        let events = through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        let checkpoints = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Compaction(CompactionEvent::Compacted {
                    reason,
                    checkpoint_id,
                    ..
                }) => {
                    assert_eq!(*reason, CompactionReason::Manual);
                    Some(checkpoint_id)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(checkpoints.len(), 1);
        let requests = provider.requests();
        assert_eq!(requests.len(), 4);
        assert_append_only(provider.protocol(), &requests[0], &requests[1]).unwrap();
        assert_compaction_transition(
            provider.protocol(),
            &requests[1],
            &requests[2],
            checkpoints[0],
        )
        .unwrap();
        assert_append_only(provider.protocol(), &requests[2], &requests[3]).unwrap();
        for request in &requests[1..] {
            assert_eq!(request["tools"], requests[0]["tools"]);
        }
        let usage = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Provider(event_bus::ProviderEvent::RequestCompleted {
                    input_tokens,
                    cache_read_tokens,
                    ..
                }) => Some((*input_tokens, *cache_read_tokens)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(usage.len(), 2);
        assert!(usage[1].1 >= usage[0].0);
        assert_eq!(project.peer.lists.load(Ordering::SeqCst), 1);
        provider.assert_drained();
        stop(&mut h, run).await;
    }
}

// The old and new chain members own separate input-derived cache baselines.
#[tokio::test]
async fn mcp_role_chain_switch_preserves_post_boundary_prefix() {
    use config::{ChainEntryConfig, MetadataSource, ModelEntryConfig, RouteCandidateConfig};
    use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
    const SMALL: &str = "small-model";
    let project = Project::new();
    let primary = Provider::new(false).await;
    primary.enqueue(vec![
        call(READ),
        Step::Text("old analysis that can be summarized ".repeat(3_000)),
    ]);
    let small = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            ScriptedResponse::tool_call(
                "small-read",
                SMALL,
                0,
                "small-call",
                READ,
                [json!({"index":3}).to_string()],
            ),
            ScriptedResponse::text_stream("small-answer", SMALL, ["done"]),
        ],
        vec![SMALL.into()],
    );
    let mut config = configuration(&primary);
    let mut profile = primary.profile();
    profile.base_url = small.base_url();
    profile.default_model = SMALL.into();
    let mut model = ModelEntryConfig::enabled(SMALL);
    model.metadata_source = Some(MetadataSource::Manual);
    model.context_window = Some(40_000);
    model.preset = Some("output".into());
    profile.models = vec![model];
    config.providers.insert("small".into(), profile);
    config.routing.routes.insert(
        "fallback".into(),
        vec![RouteCandidateConfig {
            profile: "small".into(),
            ..Default::default()
        }],
    );
    config.agents.worker.base.models = vec![
        ChainEntryConfig::route("worker"),
        ChainEntryConfig::route("fallback"),
    ];
    let mut h = harness(&project, &config);
    let run = start(&h);
    let before_events = through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    // Exhausting only the primary script makes its next request fail, selecting
    // the smaller configured chain member without changing the MCP lease.
    h.runtime
        .send_message(run, "Continue after the member failure".into())
        .unwrap();
    let events = through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    let checkpoints = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Compaction(CompactionEvent::Compacted {
                reason,
                checkpoint_id,
                ..
            }) => {
                assert_eq!(*reason, CompactionReason::ModelSwitch);
                Some(checkpoint_id)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(checkpoints.len(), 1);
    let before = primary.requests();
    let after = small
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .map(|r| r.body)
        .collect::<Vec<_>>();
    assert!(before.len() >= 3);
    assert_eq!(after.len(), 2);
    assert_frozen(&primary, &before);
    assert_usage_reuse(&before_events, 2);
    let mut boundary = before.last().unwrap().clone();
    boundary["model"] = json!(SMALL);
    assert_compaction_transition(primary.protocol(), &boundary, &after[0], checkpoints[0]).unwrap();
    assert_append_only(primary.protocol(), &after[0], &after[1]).unwrap();
    assert_eq!(before[0]["tools"], after[0]["tools"]);
    assert_usage_reuse(&events, 2);
    assert_eq!(project.peer.connections.load(Ordering::SeqCst), 1);
    assert_eq!(*project.peer.calls.lock().unwrap(), ["read", "read"]);
    assert_eq!(small.remaining_scripts(), 0);
    stop(&mut h, run).await;
}

#[tokio::test]
async fn mcp_server_restart_preserves_conversation_tools() {
    use runtime::mcp::ProjectMcpService;
    use std::sync::Arc;
    use tools::{NetworkGuard, mcp::types::McpCredentialBackend};
    for subscription in [false, true] {
        let mut project = Project::new();
        let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join("sh"))
            .find(|p| p.is_file())
            .unwrap()
            .canonicalize()
            .unwrap();
        // IDs restart per MCP client generation. EOF of the first call is the
        // completion signal, never a wall-clock wait or an ambiguous retry.
        let script = r#"
if [ -e marker ]; then first=0; else first=1; printf x > marker; fi
printf 'start\n' >> starts
IFS= read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"}}'
IFS= read -r request
if [ "$first" = 1 ]; then
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"read","inputSchema":{"type":"object","properties":{"index":{"type":"integer"}},"required":["index"]}}],"ttlMs":0,"cacheScope":"private"}}'
else
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"new_tool","inputSchema":{"type":"object"}},{"name":"read","inputSchema":{"type":"object","properties":{"index":{"type":"integer"}},"required":["index"]}}],"ttlMs":0,"cacheScope":"private"}}'
fi
IFS= read -r request
printf 'call\n' >> calls
if [ "$first" = 1 ]; then exit 0; fi
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","content":[{"type":"text","text":"remote-restarted-ok"}]}}'
IFS= read -r request
"#;
        std::fs::write(project.root().join(".evorch/mcp.toml"),format!("version=1\n[servers.fixture]\ntype=\"stdio\"\ncommand={shell:?}\nargs=[\"-c\",{}]\n",serde_json::to_string(script).unwrap())).unwrap();
        project.approve();
        project.service = Arc::new(ProjectMcpService::new(
            McpCredentialBackend::Available(project.secrets.clone()),
            project.approvals.clone(),
            Arc::new(NetworkGuard::new().unwrap()),
        ));
        let provider = Provider::new(subscription).await;
        provider.enqueue(vec![call(READ), call(READ), answer()]);
        let mut h = harness(&project, &configuration(&provider));
        let run = start(&h);
        let events = through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        let requests = provider.requests();
        assert_eq!(requests.len(), 3);
        assert_frozen(&provider, &requests);
        assert_usage_reuse(&events, 3);
        assert_eq!(external_tools(&provider, &requests[0]).len(), 1);
        assert!(
            requests[1]["messages"]
                .to_string()
                .contains("outcome_unknown")
        );
        assert!(
            requests[2]["messages"]
                .to_string()
                .contains("remote-restarted-ok")
        );
        for file in ["starts", "calls"] {
            assert_eq!(
                std::fs::read_to_string(project.root().join(file))
                    .unwrap()
                    .lines()
                    .count(),
                2
            );
        }
        provider.assert_drained();
        stop(&mut h, run).await;
    }
}

#[tokio::test]
async fn mcp_restore_and_continue_keep_snapshot() {
    // Both the same-runtime continuation and a newly composed runtime must use
    // persisted definitions. A text-only turn proves restoration itself is inert;
    // the subsequent call must lazily connect and reject a changed definition.
    for subscription in [false, true] {
        for restart in [false, true] {
            let project = Project::new();
            let provider = Provider::new(subscription).await;
            let config = configuration(&provider);
            provider.enqueue(vec![call(READ), answer()]);
            let mut h = harness(&project, &config);
            let run = start(&h);
            let mut events = through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
            stop(&mut h, run).await;
            through(&mut h.receiver, run, AgentRunPhase::Stopped).await;
            {
                let mut definitions = project.peer.tools.lock().unwrap();
                definitions
                    .iter_mut()
                    .find(|d| d["name"] == "read")
                    .unwrap()["description"] = json!("changed definition");
                definitions.push(json!({"name":"new_tool","inputSchema":{"type":"object"}}));
            }
            if restart {
                drop(h);
                h = harness(&project, &config);
            }
            assert_eq!(project.peer.connections.load(Ordering::SeqCst), 1);
            provider.enqueue(vec![answer()]);
            assert_eq!(
                h.runtime
                    .continue_goal(run, "Continue without calling MCP".into(), authority())
                    .unwrap(),
                run
            );
            events.extend(through(&mut h.receiver, run, AgentRunPhase::Waiting).await);
            assert_eq!(
                project.peer.connections.load(Ordering::SeqCst),
                1,
                "restore must not discover"
            );
            provider.enqueue(vec![call(READ), answer()]);
            h.runtime
                .send_message(run, "Now call the saved remote tool".into())
                .unwrap();
            events.extend(through(&mut h.receiver, run, AgentRunPhase::Waiting).await);
            let requests = provider.requests();
            assert_eq!(requests.len(), 5);
            assert_frozen(&provider, &requests);
            assert_usage_reuse(&events, 5);
            assert_eq!(project.peer.connections.load(Ordering::SeqCst), 2);
            assert_eq!(*project.peer.calls.lock().unwrap(), ["read"]);
            assert!(
                requests[4]["messages"]
                    .to_string()
                    .contains("definition_changed")
            );
            provider.assert_drained();
            stop(&mut h, run).await;
        }
    }
}
