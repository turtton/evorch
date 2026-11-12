//! Core bootstrap cache gate with the shared input-derived provider fixture.
use event_bus::AgentRunPhase;
use sandbox::{CredentialStore, Secret};
use serde_json::json;
use std::sync::Arc;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::{
    commands::{Command, CommandResult},
    envelope::{Provenance, Surface},
    events::{Subscribe, SubscriptionScope, UiEvent},
    ids::{ClientId, ProjectId, SessionId},
    namespaces::mcp::*,
    queries::{Query, QueryReply},
};
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
struct Composed {
    project: cache::Project,
    provider: cache::Provider,
    provider_events: event_bus::EventReceiver,
    core: BootstrappedHost,
}

async fn compose(subscription: bool) -> Composed {
    let project = cache::Project::new();
    let provider = cache::Provider::new(subscription).await;
    let config = cache::configuration(&provider);
    std::fs::create_dir_all(project.root().join("user")).unwrap();
    // The host reads the project directory from the sidebar in its config dir.
    for directory in [project.root().to_path_buf(), project.root().join("user")] {
        std::fs::write(
            directory.join("sidebar.json"),
            json!({
                "projects":[{"id":"p","repo_root":project.root()}]
            })
            .to_string(),
        )
        .unwrap();
    }
    project.secrets.set("claude", &Secret::from(json!({
        "access_token":"fixture-mcp-token","refresh_token":"fixture-refresh","expires_at":4102444800u64
    }).to_string())).unwrap();
    let provider_bus = Arc::new(event_bus::EventBus::new(4096));
    let provider_events = provider_bus.subscribe();
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
    let core = BootstrappedHost::open_with_mcp_test_seams(
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
    Composed {
        project,
        provider,
        provider_events,
        core,
    }
}

#[tokio::test]
async fn core_mcp_cache_e2e_keeps_schema_order_and_prefix_across_config_reload() {
    use std::sync::atomic::Ordering;
    for subscription in [false, true] {
        let Composed {
            project,
            provider,
            mut provider_events,
            mut core,
        } = compose(subscription).await;
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

fn send(core: &BootstrappedHost, command: McpCommand) {
    let reply = core
        .host
        .lock()
        .unwrap()
        .handle_command(
            Command::Mcp(command),
            Provenance::human(Surface::Cli, ClientId::new("cache").unwrap()),
        )
        .unwrap();
    let Some(CommandResult::Mcp(McpResult::Operation(view))) = reply.result.as_deref().cloned()
    else {
        panic!("an MCP command answers with an operation");
    };
    assert_eq!(view.state, McpOperationState::Running);
}

async fn finished(
    events: &mut workbench_host::subscriptions::Subscription,
    id: &McpOperationId,
) -> McpOperationState {
    loop {
        let envelope = events.recv().await.expect("the subscription stays open");
        if let UiEvent::Mcp(McpEvent::OperationCompleted { operation }) = envelope.event
            && &operation.operation_id == id
        {
            return operation.state;
        }
    }
}

/// The settings screen's commands (not file edits) change the project while a
/// run is in flight: the run keeps its tools and prefix, the next run alone
/// sees the saved definition and the renewed approval.
#[tokio::test]
async fn core_mcp_cache_e2e_area_commands_change_new_runs_and_never_the_old_prefix() {
    use std::sync::atomic::Ordering;
    let Composed {
        project,
        provider,
        mut provider_events,
        mut core,
    } = compose(false).await;
    let mut events = core
        .host
        .lock()
        .unwrap()
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap();
    let mut h = cache::Harness {
        runtime: core.runtime.clone(),
        receiver: core.runtime.event_bus().subscribe(),
    };
    provider.enqueue(vec![cache::call(cache::READ), cache::answer()]);
    let run = cache::start(&h);
    cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;

    let scope = McpScope {
        project: ProjectId::new("p").unwrap(),
        server_id: "fixture".into(),
    };
    let operation = |name: &str| McpOperationId::new(name).unwrap();
    project.peer.tools.lock().unwrap().push(json!({
        "name":"added","inputSchema":{"type":"object"}
    }));
    let id = operation("refresh");
    send(
        &core,
        McpCommand::Refresh(Refresh {
            operation_id: id.clone(),
            project: scope.project.clone(),
        }),
    );
    assert!(matches!(
        finished(&mut events, &id).await,
        McpOperationState::Succeeded { .. }
    ));
    let QueryReply::Mcp(McpReply::Settings(settings)) = core
        .host
        .lock()
        .unwrap()
        .handle_query(
            &SessionId::new("cache").unwrap(),
            &Query::Mcp(McpQuery::GetSettings {
                project: scope.project.clone(),
            }),
        )
        .unwrap()
    else {
        panic!("settings expected");
    };
    let mut definition = settings
        .servers
        .iter()
        .find(|server| server.server_id == "fixture")
        .and_then(|server| server.definition.clone())
        .unwrap();
    definition.roles = vec!["worker".into(), "planner".into()];
    let id = operation("save");
    send(
        &core,
        McpCommand::SaveOverride(Box::new(SaveOverride {
            operation_id: id.clone(),
            project: scope.project.clone(),
            expected_revision: settings.revision.clone(),
            mutation: McpMutation::Upsert {
                server_id: "fixture".into(),
                definition,
            },
        })),
    );
    let McpOperationState::Succeeded {
        outcome: McpOutcome::Settings { revision },
    } = finished(&mut events, &id).await
    else {
        panic!("the save should succeed");
    };
    let id = operation("preview");
    send(
        &core,
        McpCommand::PreviewApproval(PreviewApproval {
            operation_id: id.clone(),
            scope: scope.clone(),
            expected_revision: revision,
        }),
    );
    let McpOperationState::Succeeded {
        outcome: McpOutcome::ApprovalPreview { preview },
    } = finished(&mut events, &id).await
    else {
        panic!("the preview should succeed");
    };
    let id = operation("confirm");
    send(
        &core,
        McpCommand::ConfirmApproval(ConfirmApproval {
            operation_id: id.clone(),
            preview_id: preview.preview_id.clone(),
            scope,
            expected_revision: preview.source_revision.clone(),
            expected_fingerprint: preview.fingerprint.clone(),
        }),
    );
    assert_eq!(
        finished(&mut events, &id).await,
        McpOperationState::Succeeded {
            outcome: McpOutcome::ApprovalConfirmed
        }
    );

    provider.enqueue(vec![cache::call(cache::READ), cache::answer()]);
    core.runtime
        .send_message(run, "Continue after the settings screen saved".into())
        .unwrap();
    cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    cache::assert_frozen(&provider, &requests);
    assert_eq!(cache::external_tools(&provider, &requests[0]).len(), 3);
    assert_eq!(project.peer.lists.load(Ordering::SeqCst), 1);
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
