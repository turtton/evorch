use super::*;
use crate::context::responses_replay_tests::opaque;
use providers::{
    CompactionResult, Compactor, ResolvedResponsesScope, ResponsesAccount, ResponsesDialect,
};

struct ReplayClient {
    account: Mutex<String>,
    endpoint: Mutex<String>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    changed_on_send: Mutex<Option<ResolvedResponsesScope>>,
}
impl ReplayClient {
    fn scope(&self, request: &ChatRequest) -> ResolvedResponsesScope {
        ResolvedResponsesScope::resolve(
            request,
            ResponsesDialect::Codex,
            Some("local"),
            &self.endpoint.lock().unwrap(),
            ResponsesAccount::Codex {
                identity: self.account.lock().unwrap().clone(),
            },
        )
    }
}
#[async_trait]
impl Compactor for ReplayClient {
    async fn compact(&self, request: &ChatRequest) -> Result<CompactionResult, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(CompactionResult {
            usage_reported: true,
            encrypted_content: "compacted".into(),
            usage: Usage::default(),
        })
    }
}
#[async_trait]
impl ProviderClient for ReplayClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_use: true,
            reasoning: true,
        }
    }
    fn compactor(&self) -> Option<&dyn Compactor> {
        Some(self)
    }
    async fn resolve_responses_scope(
        &self,
        _: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<Option<ResolvedResponsesScope>, ProviderError> {
        Ok(Some(self.scope(request)))
    }
    async fn send(
        &self,
        _: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        if let Some(actual) = self.changed_on_send.lock().unwrap().take() {
            return Err(ProviderError::ResponsesScopeChanged {
                actual: Box::new(actual),
            });
        }
        providers::responses_replay::validate_request_scope(request, &self.scope(request))?;
        self.requests.lock().unwrap().push(request.clone());
        Ok(response())
    }
    async fn stream(
        &self,
        _: &ProviderAuth,
        _: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        unreachable!()
    }
}
fn fixture() -> (RoutedModel, Arc<ReplayClient>) {
    let (mut model, requests) = routed_model(Ok(response()), "model-a", Some("model-a+fast"));
    let client = Arc::new(ReplayClient {
        account: Mutex::new("account-a".into()),
        endpoint: Mutex::new("https://example.test/v1".into()),
        requests,
        changed_on_send: Mutex::new(None),
    });
    model.providers.get_mut("local").unwrap().client = client.clone();
    (model, client)
}
fn invocation(model: &str) -> AgentInvocationContext {
    AgentInvocationContext {
        attempts: None,
        run_id: "replay-run".into(),
        model_preference: Some(crate::ModelPreference {
            profile: "local".into(),
            model: Some(model.into()),
            reasoning_effort: None,
        }),
        ..Default::default()
    }
}
fn ids(request: &ChatRequest) -> Vec<&str> {
    request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ResponsesReasoning { item, .. } => item["id"].as_str(),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn b3_a_b_a_official_compaction_keeps_only_new_r2_and_real_compactor_scope() {
    let (model, client) = fixture();
    let a = model
        .resolve_replay_scope(&invocation("model-a"), Role::Worker, &[])
        .await
        .unwrap()
        .unwrap();
    let mut b = a.clone();
    b.route_model = "model-b".into();
    b.wire_model = "model-b".into();
    let mut context = crate::AgentContext::new(crate::RunId::new(1), Role::Worker);
    context.responses_replay.prepare(Some(a.clone()), 0);
    context.push_assistant(opaque("R1", a.clone()));
    context
        .responses_replay
        .prepare(Some(b), context.messages.len());
    context
        .responses_replay
        .prepare(Some(a.clone()), context.messages.len());
    context.push_assistant(opaque("R2", a.clone()));
    let compacted: Vec<_> = context
        .compaction_view()
        .into_iter()
        .map(|(_, m)| m)
        .collect();
    model
        .compact_context(&invocation("model-a"), Role::Worker, &compacted, &[], &[])
        .await
        .unwrap();
    assert_eq!(ids(&client.requests.lock().unwrap()[0]), ["R2"]);
    let fast_invocation = invocation("model-a+fast");
    model
        .compact_context(&fast_invocation, Role::Worker, &compacted, &[], &[])
        .await
        .unwrap();
    let recorded = client.requests.lock().unwrap();
    assert!(
        ids(&recorded[1]).is_empty(),
        "different speed compactor must not replay R2"
    );
    let metadata = recorded[1].responses_replay.as_ref().unwrap();
    assert_eq!(metadata.route_model, "model-a+fast");
    assert_eq!(
        metadata.expected_scope.as_ref().unwrap().wire_model,
        "model-a"
    );
    assert_eq!(
        metadata.expected_scope.as_ref().unwrap().service_tier,
        Some(providers::ServiceTier::Priority)
    );
    assert_eq!(context.messages.len(), 2);
}

#[tokio::test]
async fn b3_account_endpoint_and_send_time_changes_request_typed_preparation() {
    let (model, client) = fixture();
    let invocation = invocation("model-a");
    let scope = model
        .resolve_replay_scope(&invocation, Role::Worker, &[])
        .await
        .unwrap()
        .unwrap();
    let mut replay = crate::ResponsesReplayState::default();
    replay.prepare(Some(scope.clone()), 0);
    let messages = vec![opaque("R1", scope.clone())];
    let bus = EventBus::new(64);
    model
        .complete_streaming_with_replay(&invocation, Role::Worker, &messages, &[], &bus, &replay)
        .await
        .unwrap();
    model
        .complete_streaming_with_replay(&invocation, Role::Worker, &messages, &[], &bus, &replay)
        .await
        .unwrap();
    {
        let recorded = client.requests.lock().unwrap();
        assert_eq!(recorded[0], recorded[1]);
    }
    *client.account.lock().unwrap() = "account-b".into();
    let changed = model
        .complete_streaming_with_replay(&invocation, Role::Worker, &messages, &[], &bus, &replay)
        .await;
    assert!(matches!(
        changed,
        Err(RuntimeError::ResponsesScopeChanged { .. })
    ));
    assert_eq!(client.requests.lock().unwrap().len(), 2);
    *client.account.lock().unwrap() = "account-a".into();
    *client.endpoint.lock().unwrap() = "https://other.test/v1".into();
    assert!(matches!(
        model
            .complete_streaming_with_replay(
                &invocation,
                Role::Worker,
                &messages,
                &[],
                &bus,
                &replay
            )
            .await,
        Err(RuntimeError::ResponsesScopeChanged { .. })
    ));
    *client.endpoint.lock().unwrap() = "https://example.test/v1".into();
    let mut actual = scope;
    actual.account = ResponsesAccount::Codex {
        identity: "account-new-at-send".into(),
    };
    *client.changed_on_send.lock().unwrap() = Some(actual.clone());
    let result = model
        .complete_streaming_with_replay(&invocation, Role::Worker, &messages, &[], &bus, &replay)
        .await;
    assert!(
        matches!(result, Err(RuntimeError::ResponsesScopeChanged { actual: Some(scope), .. }) if *scope == actual)
    );
    assert_eq!(client.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn b3_benchmark_tier_override_is_part_of_resolved_and_sent_scope() {
    let (model, client) = fixture();
    let invocation = invocation("model-a+fast");
    let settings = crate::benchmark::BenchmarkModelSettings {
        protocol: ApiProtocol::OpenAiCompletions,
        preference: invocation.model_preference.clone().unwrap(),
        generation: Default::default(),
        service_tier: None, // Explicitly overrides fast's normal Priority.
    };
    let frozen = Arc::new(model).freeze_for_benchmark(settings).unwrap();
    let scope = frozen
        .resolve_replay_scope(&invocation, Role::Worker, &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(scope.speed, providers::ResponsesSpeed::Fast);
    assert_eq!(scope.service_tier, None);
    let mut replay = crate::ResponsesReplayState::default();
    replay.prepare(Some(scope.clone()), 0);
    frozen
        .prepare_replay_context(&invocation, Role::Worker, &[], &[], &replay)
        .await
        .unwrap();
    let tools = [ToolSpec {
        name: "read".into(),
        description: "read".into(),
        input_schema: serde_json::json!({"type":"object"}),
    }];
    frozen
        .complete_streaming_with_replay(
            &invocation,
            Role::Worker,
            &[opaque("R1", scope.clone())],
            &tools,
            &EventBus::new(16),
            &replay,
        )
        .await
        .unwrap();
    let requests = client.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].service_tier, None);
    assert_eq!(
        requests[0]
            .responses_replay
            .as_ref()
            .unwrap()
            .expected_scope
            .as_ref(),
        Some(&scope)
    );
    assert_eq!(ids(&requests[0]), ["R1"]);
}

#[tokio::test]
async fn b3_routed_responses_binds_before_receive_and_saves_without_snapshot_warning() {
    assert_routed_snapshot_behavior(false).await;
}

#[tokio::test]
async fn b3_routed_plaintext_secret_rejection_keeps_the_pre_response_snapshot_and_completes() {
    assert_routed_snapshot_behavior(true).await;
}

async fn assert_routed_snapshot_behavior(reject_plaintext: bool) {
    use mock_openai::{ReasoningItem, ResponseGate, ScriptedResponse, StreamingMockOpenAi};
    use providers::provider::openai::{OpenAiClient, OpenAiConfig};

    for selected in ["model-a", "model-a+fast"] {
        let (gate, arrivals) = ResponseGate::new();
        let secret = ["sk-", &"A".repeat(40)].concat();
        let mut item = ReasoningItem::new(0, "R1", "cipher-routed-snapshot");
        if reject_plaintext {
            item = item.with_extra("unknown_plaintext", serde_json::json!({"nested": secret}));
        }
        let mock = StreamingMockOpenAi::spawn(vec![
            ScriptedResponse::text_stream("reply", "model-a", ["saved answer"])
                .with_text_output_index(1)
                .with_reasoning(item.clone())
                .with_gate(gate.clone()),
        ]);
        let (mut model, _) = routed_model(Ok(response()), "model-a", Some(selected));
        let provider = model.providers.get_mut("local").unwrap();
        provider.profile.provider_type = ProviderType::OpenAi;
        provider.profile.api_protocol = ApiProtocol::OpenAiResponses;
        provider.client = Arc::new(
            OpenAiClient::new(OpenAiConfig {
                base_url: mock.base_url(),
                ..Default::default()
            })
            .unwrap()
            .with_profile("local"),
        );
        let directory = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: directory.path().join("routed-snapshot.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(256));
        let mut events = bus.subscribe();
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            Arc::new(SwitchableModel::new(Arc::new(model))),
        )
        .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let run = runtime.delegate_background(Role::Worker, "task".into(), Default::default());
        tokio::select! {
            arrival = tokio::task::spawn_blocking(move || arrivals.recv()) => {
                arrival.unwrap().unwrap();
            }
            phase = runtime.wait(run) => {
                gate.release();
                let observed: Vec<_> = events.drain_pending_snapshot().into_iter()
                    .filter_map(|event| match event.kind {
                        event_bus::EventKind::Diagnostic(diagnostic) => Some(diagnostic.detail),
                        event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged { reason, .. }) => reason,
                        _ => None,
                    }).collect();
                panic!("run finished before contacting mock: {phase:?}, diagnostics={observed:?}");
            }
        }
        // The real provider is paused before its response: binding must already
        // be durable, not inferred from the response item's origin afterwards.
        let prepared = runtime.shared.run_store.get().unwrap().restore_record(run);
        gate.release();
        assert_eq!(
            runtime.wait(run).await.unwrap(),
            event_bus::AgentRunPhase::Done
        );
        let prepared_record = prepared.unwrap().unwrap();
        let prepared: crate::restore::RunRestoreDescriptor =
            serde_json::from_str(&prepared_record.config_json).unwrap();
        let prepared_scope = prepared.responses_replay.as_ref().unwrap().scope().unwrap();
        assert_eq!(prepared_scope.route_model, selected);
        assert_eq!(prepared_scope.wire_model, "model-a");
        let saved = runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(run)
            .unwrap()
            .unwrap();
        let descriptor: crate::restore::RunRestoreDescriptor =
            serde_json::from_str(&saved.config_json).unwrap();
        assert_eq!(descriptor.responses_replay, prepared.responses_replay);
        let messages: Vec<Message> = serde_json::from_str(&saved.messages_json).unwrap();
        let native: Vec<_> = messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ResponsesReasoning { item, origin } => Some((item, origin)),
                _ => None,
            })
            .collect();
        if reject_plaintext {
            assert_eq!(
                saved, prepared_record,
                "both checkpoint and terminal rejection must retain the last valid snapshot"
            );
            assert!(native.is_empty());
        } else {
            assert_eq!(native, [(&item.done_item(), prepared_scope)]);
        }
        assert_eq!(mock.recorded_requests().len(), 1);
        assert_eq!(
            runtime.run_result(run).unwrap().as_deref(),
            Some("saved answer")
        );
        let mut rejected_snapshots = 0;
        for event in events.drain_pending_snapshot() {
            if let event_bus::EventKind::Diagnostic(diagnostic) = event.kind {
                assert!(!diagnostic.detail.contains(&secret));
                if diagnostic.code == event_bus::event::diagnostic_codes::CONTEXT_SNAPSHOT_FAILED {
                    rejected_snapshots += 1;
                }
            }
        }
        if reject_plaintext {
            assert_eq!(
                rejected_snapshots, 2,
                "checkpoint and terminal saves both report the storage rejection"
            );
        } else {
            assert_eq!(
                rejected_snapshots, 0,
                "a production response with a valid scope must be saved without warnings"
            );
        }
    }
}
