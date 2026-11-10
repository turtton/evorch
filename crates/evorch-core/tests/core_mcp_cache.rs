//! Core bootstrap cache gate with the shared input-derived provider fixture.
use event_bus::AgentRunPhase;
use sandbox::{CredentialStore, Secret};
use serde_json::json;
use std::sync::Arc;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
// Share the established input-derived provider cache and independent wire
// assertions, rather than creating another fixed cached-token fixture.
#[allow(dead_code)]
#[path = "../../runtime/tests/mcp_cache_support/mod.rs"]
mod cache;

struct Unisolated;
impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}
#[tokio::test]
async fn core_mcp_cache_e2e_keeps_schema_order_and_prefix_across_config_reload() {
    use std::sync::atomic::Ordering;
    for subscription in [false, true] {
        let project = cache::Project::new();
        let provider = cache::Provider::new(subscription).await;
        let config = cache::configuration(&provider);
        std::fs::write(
            project.root().join("sidebar.json"),
            json!({
                "projects":[{"id":"p","repo_root":project.root()}]
            })
            .to_string(),
        )
        .unwrap();
        project.secrets.set("claude", &Secret::from(json!({
            "access_token":"fixture-mcp-token","refresh_token":"fixture-refresh","expires_at":4102444800u64
        }).to_string())).unwrap();
        let provider_bus = Arc::new(event_bus::EventBus::new(4096));
        let mut provider_events = provider_bus.subscribe();
        let model = workbench_host::bootstrap::models::compose_production_model(
            &config,
            &workbench_host::bootstrap::models::ProductionModel {
                load_options: config::LoadOptions {
                    user_config_dir: Some(project.root().join("user")),
                    ..Default::default()
                },
                credential_store: project.secrets.clone(),
                bus: provider_bus.clone(),
                env: Arc::new(routing::MapEnv::from_iter([(
                    "MCP_CACHE_KEY",
                    "fixture-key",
                )])),
                model_catalog_dir: Some(test_support::model_catalog_dir()),
            },
        )
        .unwrap();
        let mut core = BootstrappedHost::open_with_mcp_test_seams(
            BootstrapOptions {
                config_dir: project.root().join("user"),
                project_root: project.root().to_path_buf(),
                ownership_root: project.root().join("owners"),
            },
            TestSeams {
                model,
                sandbox: Arc::new(Unisolated),
            },
            runtime::mcp::McpHostServices::from_service((*project.service).clone()),
        )
        .await
        .unwrap();
        let mut h = cache::Harness {
            runtime: core.runtime.clone(),
            receiver: core.runtime.event_bus().subscribe(),
        };
        provider.enqueue(vec![cache::call(cache::READ), cache::answer()]);
        let run = cache::start(&h);
        cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        project.peer.tools.lock().unwrap().push(json!({
            "name":"added","inputSchema":{"type":"object"}
        }));
        project.write_config("roles=[\"worker\",\"planner\"]");
        project.approve();
        provider.enqueue(vec![cache::call(cache::READ), cache::answer()]);
        core.runtime
            .send_message(run, "Continue after reload".into())
            .unwrap();
        cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        let requests = provider.requests();
        assert_eq!(requests.len(), 4);
        cache::assert_frozen(&provider, &requests);
        assert_eq!(project.peer.lists.load(Ordering::SeqCst), 1);
        assert_eq!(cache::external_tools(&provider, &requests[0]).len(), 3);
        let mut completions = Vec::new();
        while completions.len() < 4 {
            let event = provider_events.recv().await.unwrap();
            if matches!(
                event.kind,
                event_bus::EventKind::Provider(event_bus::ProviderEvent::RequestCompleted { .. })
            ) {
                completions.push(event);
            }
        }
        cache::assert_usage_reuse(&completions, 4);
        core.runtime
            .stop(run, runtime::StopScope::SelfOnly)
            .unwrap();
        core.runtime.wait(run).await.unwrap();
        provider.enqueue(vec![cache::call(cache::READ), cache::answer()]);
        let fresh = cache::start(&h);
        cache::through(&mut h.receiver, fresh, AgentRunPhase::Waiting).await;
        let requests = provider.requests();
        assert_eq!(cache::external_tools(&provider, &requests[4]).len(), 4);
        mock_openai::cache_contract::assert_append_only(
            provider.protocol(),
            &requests[4],
            &requests[5],
        )
        .unwrap();
        provider.assert_drained();
        core.shutdown().await.unwrap();
    }
}
