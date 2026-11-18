//! Settings and authentication changes made through the production core are
//! bookkeeping for later runs: they never rewrite what a running conversation
//! already sent to the provider. The provider is the production wire (routing,
//! HTTP, and the subscription protocol too) with the shared input-derived cache
//! fixture, and the prefix assertions read the recorded requests.
//!
//! Two scenarios. The baseline is a two-turn run on the production wire with the
//! production reload hook (`reload_models`) called between the turns; the test
//! seam supplies a fixed model there, so that hook replaces nothing and the
//! baseline is not a proof about a recomposed model. The second scenario goes
//! through the settings commands and observes the replacement: a project's
//! provider is saved to another endpoint and the models are reloaded between two
//! turns of one conversation. The next request must arrive at the new endpoint,
//! and it must still extend what the first endpoint received.
use std::sync::Arc;

use async_trait::async_trait;
use event_bus::AgentRunPhase;
use sandbox::CredentialStore as _;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::{Command, ComposerRole, SendChat};
use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
use workbench_protocol::ids::{OperationId, ProjectId, SessionId, SubmissionId, ThreadId};
use workbench_protocol::namespaces::settings::{
    CredentialRef, ModelInput, OpenaiCompatibleKind, ProviderInput, ReloadModels, SaveProvider,
    SettingsCommand, SettingsEvent, SettingsQuery, SettingsReply,
};
use workbench_protocol::operation::OperationState;
use workbench_protocol::queries::{Query, QueryReply};
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
                attachments: vec![],
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

        // The hook a settings save ends with, the one the Evaluator's reload tool
        // uses. The test seam's fixed model is not replaced, so this only shows that
        // calling it between two turns leaves the sent prefix alone.
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

struct Idle;
#[async_trait]
impl runtime::AgentModel for Idle {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        std::future::pending().await
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "settings-cache-startup".into()
    }
}

const KEY_VARIABLE: &str = "SETTINGS_CACHE_KEY";

fn provider_input(base_url: &str) -> ProviderInput {
    ProviderInput::OpenaiCompatible {
        name: "fixture".into(),
        kind: OpenaiCompatibleKind::OpenaiCompatible,
        base_url: base_url.into(),
        credential: CredentialRef::Env {
            var: KEY_VARIABLE.into(),
        },
        models: vec![ModelInput {
            id: cache::MODEL.into(),
            enabled: true,
        }],
        excluded_models: vec![],
        default_model: cache::MODEL.into(),
    }
}

/// Runs one settings command through the production command path and returns
/// how its operation ended, as the core announces it.
async fn settle(
    core: &BootstrappedHost,
    events: &mut workbench_host::subscriptions::Subscription,
    id: &OperationId,
    command: SettingsCommand,
) -> OperationState {
    core.host
        .lock()
        .unwrap()
        .handle_command(Command::Settings(command), local_desktop())
        .unwrap();
    loop {
        let envelope = events.recv().await.expect("the subscription is open");
        if let UiEvent::Settings(SettingsEvent::SettingsOperationCompleted { operation }) =
            envelope.event
            && &operation.operation.operation_id == id
        {
            return operation.operation.state;
        }
    }
}

/// A project other than the one the core started with composes its own model
/// from the saved files (`SwitchableModel`), which is what a settings save
/// replaces. The core is opened with the production composition of that part;
/// only the startup project's model and the sandbox are test seams.
#[tokio::test(flavor = "multi_thread")]
async fn saved_provider_replaces_the_model_and_the_next_request_still_extends_the_sent_prefix() {
    // The production model reads its key from the process environment; the
    // name is private to this test file, and nothing else reads it.
    unsafe { std::env::set_var(KEY_VARIABLE, "fixture-key") };
    let project = cache::Project::new();
    let first = cache::Provider::new(false).await;
    let second = cache::Provider::new(false).await;
    let first_url = first.profile().base_url;
    let second_url = second.profile().base_url;
    let root = project.root().canonicalize().unwrap();
    let user = root.join("user");
    std::fs::create_dir_all(&user).unwrap();
    let other = root.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let other = other.canonicalize().unwrap();
    std::fs::write(
        user.join("config.toml"),
        format!(
            "version = {}\n\
             [providers.fixture]\n\
             type = 'openai-compatible'\n\
             base_url = '{first_url}'\n\
             api_key_env = '{KEY_VARIABLE}'\n\
             models = ['{model}']\n\
             default_model = '{model}'\n\
             [routing.routes]\n\
             worker = [{{ profile = 'fixture' }}]\n",
            config::CURRENT_VERSION,
            model = cache::MODEL,
        ),
    )
    .unwrap();
    let store = SidebarStore::new(user.join("sidebar.json"));
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(StateProject::new("p"), "startup", &root)
        .unwrap();
    document
        .sidebar
        .add_project(StateProject::new("q"), "other", &other)
        .unwrap();
    document.sidebar.threads.push(ThreadRecord::new(
        StateThread::new("t1"),
        StateProject::new("q"),
        "first",
    ));
    document.sidebar.refresh_derived();
    {
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }
    let mut core = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: user.clone(),
            project_root: root.clone(),
            ownership_root: project.sockets().to_path_buf(),
        },
        TestSeams {
            model: Arc::new(Idle),
            sandbox: Arc::new(Unisolated),
        },
    )
    .await
    .unwrap();
    let mut h = cache::Harness {
        runtime: core.runtime.clone(),
        receiver: core.runtime.event_bus().subscribe(),
    };
    let mut provider_events = core.runtime.event_bus().subscribe();
    let mut settings_events = core
        .host
        .lock()
        .unwrap()
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap();

    first.enqueue(vec![cache::answer()]);
    let run: runtime::RunId = {
        let send = SendChat {
            project: ProjectId::new("q").unwrap(),
            thread: ThreadId::new("t1").unwrap(),
            submission_id: SubmissionId::new("00000000-0000-4000-8000-000000000009").unwrap(),
            composer_role: ComposerRole::Worker,
            text: cache::GOAL.into(),
            skills: vec![],
            attachments: vec![],
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
    assert_eq!(
        first.requests().len(),
        1,
        "the first turn reached the first endpoint"
    );
    assert!(second.requests().is_empty());

    // Save the provider with the other endpoint through the settings commands.
    let revision = match core
        .host
        .lock()
        .unwrap()
        .handle_query(
            &SessionId::new("settings-cache").unwrap(),
            &Query::Settings(SettingsQuery::GetSettingsSnapshot {}),
            &workbench_host::provenance::local_desktop(),
        )
        .unwrap()
    {
        QueryReply::Settings(SettingsReply::Settings(snapshot)) => snapshot.revision,
        other => panic!("{other:?}"),
    };
    let save = OperationId::new("save-provider").unwrap();
    assert_eq!(
        settle(
            &core,
            &mut settings_events,
            &save,
            SettingsCommand::SaveProvider(Box::new(SaveProvider {
                operation_id: save.clone(),
                expected_revision: revision,
                original_name: None,
                provider: provider_input(&second_url),
            })),
        )
        .await,
        OperationState::Ready
    );
    let saved = std::fs::read_to_string(user.join("config.toml")).unwrap();
    assert!(saved.contains(&second_url), "{saved}");
    let reload = OperationId::new("reload-models").unwrap();
    assert_eq!(
        settle(
            &core,
            &mut settings_events,
            &reload,
            SettingsCommand::ReloadModels(ReloadModels {
                operation_id: reload.clone(),
            }),
        )
        .await,
        OperationState::Ready
    );

    // The next two turns of the same run go to the replacement.
    second.enqueue(vec![cache::answer()]);
    core.runtime
        .send_message(run, "Continue after the settings change".into())
        .unwrap();
    cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    second.enqueue(vec![cache::answer()]);
    core.runtime
        .send_message(run, "Continue once more".into())
        .unwrap();
    cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;

    let before = first.requests();
    let after = second.requests();
    assert_eq!(
        before.len(),
        1,
        "the replaced endpoint received nothing more"
    );
    assert_eq!(after.len(), 2, "the replacement served the later turns");
    // What the first endpoint received is the prefix of what the replacement
    // received, and the tools, their order and descriptions did not move.
    let sequence = [before[0].clone(), after[0].clone(), after[1].clone()];
    cache::assert_frozen(&first, &sequence);
    // The cache of the replacement starts cold and is reused on its next turn.
    let mut completions = Vec::new();
    while completions.len() < 3 {
        let event = provider_events.recv().await.unwrap();
        if matches!(
            event.kind,
            event_bus::EventKind::Provider(event_bus::ProviderEvent::RequestCompleted { .. })
        ) {
            completions.push(event);
        }
    }
    cache::assert_usage_reuse(&completions[1..], 2);
    first.assert_drained();
    second.assert_drained();
    core.runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    core.runtime.wait(run).await.unwrap();
    core.shutdown().await.unwrap();
}
