//! Real image HTTP capability with scripted chat, artifact capture and cancellation.
use base64::Engine;
use event_bus::{AgentRunPhase, EventBus};
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, ToolResultContent, ToolSpec, Usage,
};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RuntimeError};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

struct Model {
    image: Arc<dyn AgentModel>,
    steps: Mutex<HashMap<String, usize>>,
    observed: Mutex<Vec<Vec<Message>>>,
    forbidden: bool,
    artifact: Mutex<Option<String>>,
    artifact_ready: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl AgentModel for Model {
    fn image_generation_enabled(&self) -> bool {
        self.image.image_generation_enabled()
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "script".into()
    }
    async fn generate_image(
        &self,
        ctx: &AgentInvocationContext,
        request: providers::ImageGenerationRequest,
    ) -> Result<runtime::image_generation::GeneratedImage, String> {
        self.image.generate_image(ctx, request).await
    }
    async fn complete(
        &self,
        ctx: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let delegated_artifact = if role == Role::Orchestrator && !self.forbidden {
            self.artifact_ready.notified().await;
            self.artifact.lock().unwrap().clone().unwrap()
        } else {
            String::new()
        };
        self.observed.lock().unwrap().push(messages.to_vec());
        let mut steps = self.steps.lock().unwrap();
        let step = steps.entry(ctx.run_id.clone()).or_default();
        let offered = tools.iter().any(|s| s.name == "generate_image");
        assert_eq!(offered, !self.forbidden && role == Role::Worker);
        let call = match (*step, role) {
            (0, Role::Worker) => Some((
                "generate_image",
                json!({"prompt":"material","output_path":"hero.png","reference_image_paths":["reference.png"],"variant":{"max_side":64,"format":"jpeg"}}),
            )),
            (0, Role::Orchestrator) if self.forbidden => Some((
                "generate_image",
                json!({"prompt":"forbidden","output_path":"forbidden.png"}),
            )),
            (0, Role::Orchestrator) => Some((
                "present",
                json!({"artifact_ids":[delegated_artifact],"title":"Mock"}),
            )),
            (1, Role::Worker) if !self.forbidden => Some((
                "render_artifact",
                json!({"path":"mock.html","title":"Mock"}),
            )),
            (2, Role::Worker) if ctx.category.as_deref() == Some("conversation") => Some((
                "present",
                json!({"artifact_ids":[self.artifact_id(messages)],"title":"Mock"}),
            )),
            _ => None,
        };
        if role == Role::Orchestrator && !self.forbidden {
            self.artifact_ready.notify_one();
        }
        let finished = call.is_none();
        *step += 1;
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: call.map_or_else(
                    || {
                        vec![ContentBlock::Text {
                            text: "done".into(),
                        }]
                    },
                    |(name, input)| {
                        vec![ContentBlock::ToolUse {
                            id: format!("{}-{step}", ctx.run_id),
                            name: name.into(),
                            input,
                        }]
                    },
                ),
            },
            finish_reason: if finished {
                FinishReason::Stop
            } else {
                FinishReason::ToolUse
            },
            usage: Usage::default(),
        })
    }
}
impl Model {
    fn artifact_id(&self, messages: &[Message]) -> String {
        messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| {
                if let ContentBlock::ToolResult { content, .. } = b {
                    Some(content)
                } else {
                    None
                }
            })
            .flatten()
            .filter_map(|c| {
                if let ToolResultContent::Text { text } = c {
                    serde_json::from_str::<Value>(text).ok()
                } else {
                    None
                }
            })
            .find_map(|v| v["artifact_id"].as_str().map(Into::into))
            .unwrap_or_default()
    }
}
fn png() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(128, 64)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}
async fn image_model(server: &MockServer, bus: Arc<EventBus>) -> Arc<dyn AgentModel> {
    let directory = tempfile::tempdir().unwrap();
    let config = config::Config {
        providers: BTreeMap::from([(
            "images".into(),
            config::ProviderProfileConfig {
                provider_type: config::ProviderTypeConfig::OpenAi,
                api_protocol: config::ApiProtocolConfig::OpenAiResponses,
                base_url: format!("{}/v1", server.uri()),
                credential: config::CredentialRefConfig::Env {
                    var: "TEST_KEY".into(),
                },
                models: vec![config::ModelEntryConfig::enabled("gpt-image-2")],
                default_model: "gpt-image-2".into(),
                ..Default::default()
            },
        )]),
        routing: config::RoutingConfig {
            routes: BTreeMap::from([(
                "images".into(),
                vec![config::RouteCandidateConfig {
                    profile: "images".into(),
                    model: Some("gpt-image-2".into()),
                    auto: false,
                }],
            )]),
            ..Default::default()
        },
        agents: config::AgentsConfig {
            roles: config::types::agents::AdditionalRoleBindings {
                image_generator: config::RoleBindingConfig {
                    models: vec![config::ChainEntryConfig::route("images")],
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    runtime::compose::compose_routed_model(
        &config,
        routing::ComposeDeps {
            credential_store: Arc::new(
                sandbox::credential::FileCredentialStore::open(
                    directory.path().join("credentials"),
                )
                .unwrap(),
            ),
            event_bus: Some(bus),
            env: Arc::new(routing::MapEnv::from_iter([("TEST_KEY", "offline-key")])),
            catalog: model::ModelCatalog::new(),
            factory: Default::default(),
        },
        Some(test_support::model_catalog_dir()),
    )
    .unwrap()
}
async fn harness(
    server: &MockServer,
    forbidden: bool,
) -> (
    AgentRuntime,
    Arc<Model>,
    tempfile::TempDir,
    Arc<runtime::artifacts::ArtifactStore>,
) {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("reference.png"), png()).unwrap();
    std::fs::write(
        workspace.path().join("mock.html"),
        "<html><img src=\"hero.64.jpg\"></html>",
    )
    .unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let model = Arc::new(Model {
        image: image_model(server, bus.clone()).await,
        steps: Mutex::default(),
        observed: Mutex::default(),
        forbidden,
        artifact: Mutex::default(),
        artifact_ready: Default::default(),
    });
    let executor = tools::ToolExecutor::new(bus.clone());
    executor.set_default_cwd(workspace.path().into());
    let store = Arc::new(
        runtime::artifacts::ArtifactStore::new(workspace.path().join("artifacts")).unwrap(),
    );
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone())
        .with_artifact_store(store.clone());
    (runtime, model, workspace, store)
}
async fn mount_image(server: &MockServer, delay: std::time::Duration) {
    Mock::given(method("POST"))
        .and(path("/v1/images/edits"))
        .respond_with(ResponseTemplate::new(200).set_delay(delay).set_body_json(
            json!({"data":[{"b64_json":base64::engine::general_purpose::STANDARD.encode(png())}]}),
        ))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn ac12_ac14_root_worker_and_visual_worker_generate_embed_and_present() {
    for root_worker in [true, false] {
        let server = MockServer::start().await;
        mount_image(&server, std::time::Duration::ZERO).await;
        let (runtime, model, workspace, store) = harness(&server, false).await;
        let parent = runtime.reserve_run_id();
        runtime.bind_thread_root("images", parent).unwrap();
        let worker = if root_worker {
            parent
        } else {
            runtime.spawn_reserved(
                parent,
                None,
                Role::Orchestrator,
                "coordinate",
                RunConfig {
                    conversation: true,
                    ..Default::default()
                },
            );
            let worker = runtime.reserve_run_id();
            runtime.spawn_reserved_child(
                parent,
                worker,
                Role::Worker,
                "materials",
                RunConfig {
                    category: Some("visual".into()),
                    ..Default::default()
                },
            );
            worker
        };
        if root_worker {
            runtime.spawn_reserved(
                parent,
                None,
                Role::Worker,
                "mock",
                RunConfig {
                    conversation: true,
                    category: Some("conversation".into()),
                    ..Default::default()
                },
            );
        }
        assert_eq!(runtime.wait(worker).await.unwrap(), AgentRunPhase::Done);
        let observed = model.observed.lock().unwrap().clone();
        let results: Vec<_> = observed
            .iter()
            .flatten()
            .flat_map(|m| &m.content)
            .filter_map(|b| {
                if let ContentBlock::ToolResult {
                    content, is_error, ..
                } = b
                {
                    assert!(!is_error, "{content:?}");
                    Some(content)
                } else {
                    None
                }
            })
            .collect();
        assert!(results.iter().any(|c| c.iter().any(
            |p| matches!(p,ToolResultContent::Image {media_type,..} if media_type=="image/png")
        )));
        let text = results
            .iter()
            .flat_map(|c| c.iter())
            .filter_map(|p| {
                if let ToolResultContent::Text { text } = p {
                    Some(text)
                } else {
                    None
                }
            })
            .find(|s| s.contains("reference_images_sent"))
            .unwrap();
        let result: Value = serde_json::from_str(text).unwrap();
        assert_eq!(result["reference_images_sent"], true);
        assert_eq!(result["profile"], "images");
        assert_eq!(result["model"], "gpt-image-2");
        assert!(
            result["note"]
                .as_str()
                .unwrap()
                .contains("reference images were sent to OpenAI")
        );
        let messages = observed.last().unwrap().clone();
        let id = model.artifact_id(&messages);
        drop(observed);
        let record = store.load(&id).unwrap();
        assert_eq!(record.inlined_assets.len(), 1);
        assert!(
            String::from_utf8(std::fs::read(store.blob_path(&record)).unwrap())
                .unwrap()
                .contains("data:image/jpeg;base64,")
        );
        if !root_worker {
            *model.artifact.lock().unwrap() = Some(id);
            model.artifact_ready.notify_one();
            assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Done);
        }
        assert!(workspace.path().join("hero.png").is_file());
        assert!(workspace.path().join("hero.64.jpg").is_file());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(
            body["images"][0]["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
    }
}

#[tokio::test]
async fn ac15_cancellation_drops_request_and_writes_no_images() {
    let server = MockServer::start().await;
    mount_image(&server, std::time::Duration::from_secs(30)).await;
    let (runtime, _, workspace, _) = harness(&server, false).await;
    let run = runtime.delegate_background(
        Role::Worker,
        "generate".into(),
        RunConfig {
            category: Some("visual".into()),
            ..Default::default()
        },
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    runtime.cancel_subtree(run).unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
    assert!(!workspace.path().join("hero.png").exists());
    assert!(!workspace.path().join("hero.64.jpg").exists());
}

#[tokio::test]
async fn ac9_ownership_loss_during_generation_prevents_file_writes() {
    use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
    let server = MockServer::start().await;
    mount_image(&server, std::time::Duration::from_millis(300)).await;
    let (runtime, _, workspace, _) = harness(&server, false).await;
    let registry_path = workspace.path().join("ownership.db");
    let mut registry = Registry::open(&registry_path).unwrap();
    let owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "first".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    );
    registry.start(&owner).unwrap();
    let permit = OwnerPermit {
        registry_path,
        thread_id: "thread".into(),
        lease: owner.lease,
        run_id: None,
    };
    let run = runtime.delegate_background(
        Role::Worker,
        "material".into(),
        RunConfig {
            category: Some("visual".into()),
            ownership: Some(permit),
            ..Default::default()
        },
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    registry
        .update("thread", |owner| {
            owner.lease.generation += 1;
            owner.lease.owner_id = "second".into();
            Ok(())
        })
        .unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
    assert!(!workspace.path().join("hero.png").exists());
    assert!(!workspace.path().join("hero.64.jpg").exists());
}

#[tokio::test]
async fn ac6_dispatch_denies_orchestrator_and_child_conversation_worker() {
    for role in [Role::Orchestrator, Role::Worker] {
        let server = MockServer::start().await;
        let (runtime, model, workspace, _) = harness(&server, true).await;
        let parent = runtime.reserve_run_id();
        let run = runtime.reserve_run_id();
        if role == Role::Worker {
            runtime.spawn_reserved(
                parent,
                None,
                Role::Orchestrator,
                "parent",
                RunConfig::default(),
            );
        }
        runtime.spawn_reserved(
            run,
            (role == Role::Worker).then_some(parent),
            role,
            "attempt",
            RunConfig {
                conversation: true,
                category: Some("conversation".into()),
                ..Default::default()
            },
        );
        assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
        assert!(server.received_requests().await.unwrap().is_empty());
        assert!(!workspace.path().join("hero.png").exists());
        assert!(!workspace.path().join("forbidden.png").exists());
        assert!(
            model
                .observed
                .lock()
                .unwrap()
                .iter()
                .flatten()
                .flat_map(|m| &m.content)
                .any(|b| matches!(b, ContentBlock::ToolResult { is_error: true, .. }))
        );
    }
}
