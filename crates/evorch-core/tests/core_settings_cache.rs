//! Settings and authentication changes made through the production core are
//! bookkeeping for later runs: they never rewrite what a running conversation
//! already sent to the provider. The provider is the production wire (routing,
//! HTTP, and the subscription protocol too) with the shared input-derived cache
//! fixture, and the prefix assertions read the recorded requests.
//!
//! This file holds the baseline the integration PR S0 registers: a two-turn run
//! on the production wire with the production reload hook (`reload_models`, the
//! hook a settings save ends with) called between the turns. The test seam
//! supplies a fixed model, so the hook replaces nothing here: this is the
//! two-turn prefix baseline, not a proof that a recomposed model keeps it. PR 10
//! adds the scenario that goes through the settings commands and observes the
//! replacement.
use std::sync::Arc;

use event_bus::AgentRunPhase;
use sandbox::CredentialStore as _;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::{Command, ComposerRole, SendChat};
use workbench_protocol::ids::{ProjectId, SubmissionId, ThreadId};
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

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

#[tokio::test(flavor = "multi_thread")]
async fn two_turns_around_the_reload_hook_keep_the_sent_prefix_on_the_production_wire() {
    for subscription in [false, true] {
        let project = cache::Project::new();
        let provider = cache::Provider::new(subscription).await;
        let config = cache::configuration(&provider);
        let root = project.root().canonicalize().unwrap();
        let user = root.join("user");
        std::fs::create_dir_all(&user).unwrap();
        let store = SidebarStore::new(user.join("sidebar.json"));
        let mut document = SidebarDocument::default();
        document
            .sidebar
            .add_project(StateProject::new("p"), "repo", &root)
            .unwrap();
        document.sidebar.threads.push(ThreadRecord::new(
            StateThread::new("t1"),
            StateProject::new("p"),
            "first",
        ));
        document.sidebar.refresh_derived();
        {
            let lock = store.lock().unwrap();
            store.write(&lock, &document).unwrap();
        }
        project
            .secrets
            .set(
                "claude",
                &sandbox::Secret::from(
                    serde_json::json!({
                        "access_token": "fixture-mcp-token",
                        "refresh_token": "fixture-refresh",
                        "expires_at": 4_102_444_800u64
                    })
                    .to_string(),
                ),
            )
            .unwrap();
        let provider_bus = Arc::new(event_bus::EventBus::new(4096));
        let mut provider_events = provider_bus.subscribe();
        let model = workbench_host::bootstrap::models::compose_production_model(
            &config,
            &workbench_host::bootstrap::models::ProductionModel {
                load_options: config::LoadOptions {
                    user_config_dir: Some(user.clone()),
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
        let mut core = BootstrappedHost::open_with_test_seams(
            BootstrapOptions {
                config_dir: user.clone(),
                project_root: root.clone(),
                ownership_root: project.sockets().to_path_buf(),
            },
            TestSeams {
                model,
                sandbox: Arc::new(Unisolated),
            },
        )
        .await
        .unwrap();
        let mut h = cache::Harness {
            runtime: core.runtime.clone(),
            receiver: core.runtime.event_bus().subscribe(),
        };
        provider.enqueue(vec![cache::answer()]);
        let run: runtime::RunId = {
            let send = SendChat {
                project: ProjectId::new("p").unwrap(),
                thread: ThreadId::new("t1").unwrap(),
                // The production Claude transport requires a UUID prompt identity.
                submission_id: SubmissionId::new("00000000-0000-4000-8000-000000000009").unwrap(),
                composer_role: ComposerRole::Worker,
                text: cache::GOAL.into(),
                skills: vec![],
                images: vec![],
                model_preference: None,
                fork_seed: None,
            };
            let mut host = core.host.lock().unwrap();
            host.reserve_new_chat_thread(&send).unwrap();
            host.handle_command(
                Command::Conversation(
                    workbench_protocol::namespaces::ConversationCommand::SendChat(send),
                ),
                local_desktop(),
            )
            .unwrap()
            .run_id
            .unwrap()
            .as_str()
            .parse()
            .unwrap()
        };
        cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;

        // What a settings save ends with: every model is recomposed from the
        // files. The production hook is the one the Evaluator's reload tool uses.
        (core
            .evaluator
            .reload
            .get()
            .expect("the models are composed"))()
        .unwrap();

        provider.enqueue(vec![cache::answer()]);
        core.runtime
            .send_message(run, "Continue after the reload".into())
            .unwrap();
        cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        cache::assert_frozen(&provider, &requests);
        let mut completions = Vec::new();
        while completions.len() < 2 {
            let event = provider_events.recv().await.unwrap();
            if matches!(
                event.kind,
                event_bus::EventKind::Provider(event_bus::ProviderEvent::RequestCompleted { .. })
            ) {
                completions.push(event);
            }
        }
        cache::assert_usage_reuse(&completions, 2);
        provider.assert_drained();
        core.runtime
            .stop(run, runtime::StopScope::SelfOnly)
            .unwrap();
        core.runtime.wait(run).await.unwrap();
        core.shutdown().await.unwrap();
    }
}
