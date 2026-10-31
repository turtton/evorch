use super::*;
use config::EditToolConfig;

pub(super) fn edit_specs() -> Vec<ToolSpec> {
    ["read", "edit", "write", "apply_patch", "shell"]
        .into_iter()
        .map(|name| ToolSpec {
            name: name.into(),
            description: name.into(),
            input_schema: serde_json::json!({"type": "object"}),
        })
        .collect()
}

fn names(tools: &[ToolSpec]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_str()).collect()
}

fn openai(model: &mut RoutedModel) {
    let profile = &mut model.providers.get_mut("local").unwrap().profile;
    profile.provider_type = ProviderType::OpenAi;
    profile.api_protocol = ApiProtocol::OpenAiResponses;
}

async fn call(model: &dyn AgentModel, run_id: &str) -> Result<ChatResponse, RuntimeError> {
    model
        .complete(
            &AgentInvocationContext {
                attempts: None,
                run_id: run_id.into(),
                ..Default::default()
            },
            Role::Worker,
            &[],
            &edit_specs(),
        )
        .await
}

fn checkpoint_history() -> Vec<Message> {
    vec![Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Compaction {
            encrypted_content: "opaque-from-a-previous-process".into(),
        }],
    }]
}

#[tokio::test]
async fn reconfiguration_preserves_known_edit_interface_and_replayable_checkpoint() {
    for selection in [EditToolConfig::ApplyPatch, EditToolConfig::Edit] {
        let (mut first, _) = routed_model(Ok(response()), "gpt-5.5", None);
        first
            .providers
            .get_mut("local")
            .unwrap()
            .profile
            .edit_tool
            .profile = Some(selection);
        let first_requests = super::switch::QueueClient::install(&mut first, "local", vec![], true);
        let live = SwitchableModel::new(Arc::new(first));
        call(&live, "reload").await.unwrap();
        let (mut second, _) = routed_model(Ok(response()), "gpt-5.5", None);
        second
            .providers
            .get_mut("local")
            .unwrap()
            .profile
            .edit_tool
            .profile = Some(selection);
        let second_requests =
            super::switch::QueueClient::install(&mut second, "local", vec![], true);
        live.replace(Arc::new(second));
        live.complete(
            &AgentInvocationContext {
                attempts: None,
                run_id: "reload".into(),
                ..Default::default()
            },
            Role::Worker,
            &checkpoint_history(),
            &edit_specs(),
        )
        .await
        .unwrap();
        assert_eq!(first_requests.lock().unwrap().len(), 1);
        assert_eq!(second_requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn restored_checkpoint_with_unknown_edit_interface_resummarizes_once() {
    for selection in [EditToolConfig::ApplyPatch, EditToolConfig::Edit] {
        for explicit in [false, true] {
            let (mut model, _) = routed_model(Ok(response()), "gpt-5.5", None);
            model
                .providers
                .get_mut("local")
                .unwrap()
                .profile
                .edit_tool
                .profile = Some(selection);
            let requests = super::switch::QueueClient::install(&mut model, "local", vec![], true);
            model.restore_route_pins(
                "restored",
                &[crate::RoutePinRecord {
                    chain: "worker".into(),
                    position: 0,
                    profile: "local".into(),
                    model: "gpt-5.5".into(),
                }],
            );
            let invocation = AgentInvocationContext {
                attempts: None,
                run_id: "restored".into(),
                model_preference: explicit.then(|| crate::ModelPreference {
                    profile: "local".into(),
                    model: Some("gpt-5.5".into()),
                    reasoning_effort: None,
                }),
                ..Default::default()
            };
            assert_eq!(
                model
                    .complete(
                        &invocation,
                        Role::Worker,
                        &checkpoint_history(),
                        &edit_specs()
                    )
                    .await,
                Err(RuntimeError::ModelSwitch {
                    selected: "local/gpt-5.5".into(),
                    resummarize: true
                })
            );
            assert!(requests.lock().unwrap().is_empty());
            model.model_switch_prepared(&invocation, Role::Worker, "local/gpt-5.5");
            model
                .complete(&invocation, Role::Worker, &[], &edit_specs())
                .await
                .unwrap();
            model
                .complete(&invocation, Role::Worker, &[], &edit_specs())
                .await
                .unwrap();
            assert_eq!(requests.lock().unwrap().len(), 2);
        }
    }
}

#[tokio::test]
async fn unknown_edit_history_uses_only_incompatible_tool_calls_as_switch_evidence() {
    for selection in [EditToolConfig::ApplyPatch, EditToolConfig::Edit] {
        for old_tool in [
            None,
            Some("read"),
            Some("apply_patch"),
            Some("edit"),
            Some("write"),
        ] {
            let (mut model, requests) = routed_model(Ok(response()), "mock-model", None);
            model
                .providers
                .get_mut("local")
                .unwrap()
                .profile
                .edit_tool
                .profile = Some(selection);
            // A restored pin must not make the new configuration evidence of old tools.
            model.restore_route_pins(
                "unknown",
                &[crate::RoutePinRecord {
                    chain: "worker".into(),
                    position: 0,
                    profile: "local".into(),
                    model: "mock-model".into(),
                }],
            );
            let messages = old_tool
                .map(|name| Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "old-call".into(),
                        name: name.into(),
                        input: serde_json::json!({}),
                    }],
                })
                .into_iter()
                .collect::<Vec<_>>();
            let invocation = AgentInvocationContext {
                attempts: None,
                run_id: "unknown".into(),
                ..Default::default()
            };
            let incompatible = matches!(
                (selection, old_tool),
                (EditToolConfig::ApplyPatch, Some("edit" | "write"))
                    | (EditToolConfig::Edit, Some("apply_patch"))
            );
            let result = model
                .complete(&invocation, Role::Worker, &messages, &edit_specs())
                .await;
            if incompatible {
                assert_eq!(
                    result,
                    Err(RuntimeError::ModelSwitch {
                        selected: "local/mock-model".into(),
                        resummarize: true
                    })
                );
                assert!(requests.lock().unwrap().is_empty());
                model.model_switch_prepared(&invocation, Role::Worker, "local/mock-model");
                model
                    .complete(&invocation, Role::Worker, &[], &edit_specs())
                    .await
                    .unwrap();
            } else {
                result.unwrap();
            }
            assert_eq!(requests.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn an_unprepared_opaque_switch_remains_pending_until_preparation_commits() {
    let (mut model, _) = routed_model(Ok(response()), "gpt-5.5", None);
    openai(&mut model);
    let invocation = AgentInvocationContext {
        attempts: None,
        run_id: "pending".into(),
        ..Default::default()
    };
    model
        .complete(&invocation, Role::Worker, &[], &edit_specs())
        .await
        .unwrap();
    model
        .providers
        .get_mut("local")
        .unwrap()
        .profile
        .edit_tool
        .profile = Some(EditToolConfig::Edit);
    super::switch::QueueClient::install(&mut model, "local", vec![], true);
    let history = checkpoint_history();
    for _ in 0..2 {
        assert!(matches!(
            model
                .complete(&invocation, Role::Worker, &history, &edit_specs())
                .await,
            Err(RuntimeError::ModelSwitch {
                resummarize: true,
                ..
            })
        ));
    }
    model.model_switch_prepared(&invocation, Role::Worker, "local/gpt-5.5");
    model
        .complete(&invocation, Role::Worker, &[], &edit_specs())
        .await
        .unwrap();
    model
        .complete(&invocation, Role::Worker, &[], &edit_specs())
        .await
        .unwrap();
}

#[tokio::test]
async fn edit_switch_resummarizes_even_when_the_target_replays_compaction() {
    super::switch::fallback_resummarizes_checkpoint(true).await;
}

#[tokio::test]
async fn explicit_model_change_resummarizes_once_and_summaries_do_not_reset_it() {
    let (mut model, requests) = routed_model(Ok(response()), "gpt-5.5", Some("other-model"));
    openai(&mut model);
    let mut invocation = AgentInvocationContext {
        attempts: None,
        run_id: "explicit".into(),
        model_preference: Some(crate::ModelPreference {
            profile: "local".into(),
            model: Some("gpt-5.5".into()),
            reasoning_effort: None,
        }),
        ..Default::default()
    };
    model
        .complete(&invocation, Role::Worker, &[], &edit_specs())
        .await
        .unwrap();
    invocation.model_preference.as_mut().unwrap().model = Some("other-model".into());
    assert!(matches!(
        model
            .complete(&invocation, Role::Worker, &[], &edit_specs())
            .await,
        Err(RuntimeError::ModelSwitch {
            resummarize: true,
            ..
        })
    ));
    assert_eq!(requests.lock().unwrap().len(), 1);
    model.model_switch_prepared(&invocation, Role::Worker, "local/other-model");
    let mut summary = invocation.clone();
    summary.purpose = event_bus::RequestPurpose::Compaction;
    summary.model_preference.as_mut().unwrap().model = Some("gpt-5.5".into());
    model
        .complete(&summary, Role::Worker, &[], &[])
        .await
        .unwrap();
    model
        .complete(&invocation, Role::Worker, &[], &edit_specs())
        .await
        .unwrap();
    assert_eq!(
        names(&requests.lock().unwrap().last().unwrap().tools),
        ["read", "edit", "write", "shell"]
    );
}

#[tokio::test]
async fn selected_edit_tool_follows_the_run_pin_and_frozen_benchmark_route() {
    let (mut model, _) = super::fallback::fixture(vec![None, None]);
    model
        .providers
        .get_mut("profile-0")
        .unwrap()
        .profile
        .edit_tool
        .profile = Some(EditToolConfig::ApplyPatch);
    model.restore_route_pins(
        "pinned",
        &[crate::RoutePinRecord {
            chain: "worker".into(),
            position: 1,
            profile: "profile-1".into(),
            model: "model-1".into(),
        }],
    );
    assert_eq!(
        model.edit_tool_for_run("fresh", Role::Worker, None),
        Some(EditToolConfig::ApplyPatch)
    );
    assert_eq!(
        model.edit_tool_for_run("pinned", Role::Worker, None),
        Some(EditToolConfig::Edit)
    );
    let model = Arc::new(model);
    let settings = model
        .benchmark_settings(
            &AgentInvocationContext::default(),
            Role::Worker,
            &edit_specs(),
        )
        .await
        .unwrap();
    let frozen = model.freeze_for_benchmark(settings).unwrap();
    assert!(frozen.narrows_edit_tools());
    assert_eq!(
        frozen.edit_tool_for_run("pinned", Role::Worker, None),
        Some(EditToolConfig::ApplyPatch)
    );
}

#[tokio::test]
async fn preview_reports_the_model_specific_exclusion_reason() {
    for apply_patch in [false, true] {
        let (mut model, _) = routed_model(Ok(response()), "gpt-5.5", None);
        if apply_patch {
            openai(&mut model);
        }
        let bus = Arc::new(EventBus::new(128));
        let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        ));
        let runtime = AgentRuntime::new(bus, executor, Arc::new(model));
        let preview = runtime
            .preview_base_context(crate::base_context::BaseContextRequest::new(Role::Worker))
            .await
            .unwrap();
        let names = preview
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names.contains(&"apply_patch"), apply_patch);
        for name in ["edit", "write"] {
            assert_eq!(names.contains(&name), !apply_patch);
        }
        let expected = if apply_patch {
            vec!["edit", "write"]
        } else {
            vec!["apply_patch"]
        };
        for name in expected {
            assert_eq!(
                preview
                    .excluded_tools
                    .iter()
                    .find(|tool| tool.item == name)
                    .unwrap()
                    .reason,
                if apply_patch {
                    "Replaced by apply_patch for this model"
                } else {
                    "This model uses edit and write instead of apply_patch"
                }
            );
        }
    }
}

#[tokio::test]
async fn responses_gpt5_only_exposes_apply_patch() {
    let (mut model, requests) = routed_model(Ok(response()), "gpt-5.5", None);
    openai(&mut model);
    call(&model, "run").await.unwrap();
    assert_eq!(
        names(&requests.lock().unwrap()[0].tools),
        ["read", "apply_patch", "shell"]
    );
}

#[tokio::test]
async fn compatible_only_exposes_edit_and_write() {
    let (model, requests) = routed_model(Ok(response()), "mock-model", None);
    call(&model, "run").await.unwrap();
    assert_eq!(
        names(&requests.lock().unwrap()[0].tools),
        ["read", "edit", "write", "shell"]
    );
}

#[tokio::test]
async fn compatible_model_entry_can_select_apply_patch() {
    let (mut model, requests) = routed_model(Ok(response()), "mock-model", None);
    model
        .providers
        .get_mut("local")
        .unwrap()
        .profile
        .edit_tool
        .models
        .insert("mock-model".into(), EditToolConfig::ApplyPatch);
    call(&model, "run").await.unwrap();
    assert_eq!(
        names(&requests.lock().unwrap()[0].tools),
        ["read", "apply_patch", "shell"]
    );
}

#[tokio::test]
async fn fast_model_entry_override_is_shared_by_requests_preview_and_frozen_benchmarks() {
    let (mut model, requests) = routed_model(Ok(response()), "gpt-5.5+fast", None);
    openai(&mut model);
    let selection = &mut model.providers.get_mut("local").unwrap().profile.edit_tool;
    selection
        .models
        .insert("gpt-5.5".into(), EditToolConfig::ApplyPatch);
    selection
        .models
        .insert("gpt-5.5+fast".into(), EditToolConfig::Edit);
    call(&model, "fast").await.unwrap();
    assert_eq!(
        names(&requests.lock().unwrap()[0].tools),
        ["read", "edit", "write", "shell"]
    );
    assert_eq!(requests.lock().unwrap()[0].model, "gpt-5.5");
    assert_eq!(
        model.edit_tool_for_run("fast", Role::Worker, None),
        Some(EditToolConfig::Edit)
    );
    let model = Arc::new(model);
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::with_standard_tools(
            bus,
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )),
        model.clone(),
    );
    let preview = runtime
        .preview_base_context(crate::base_context::BaseContextRequest::new(Role::Worker))
        .await
        .unwrap();
    assert!(preview.tools.iter().any(|tool| tool.name == "edit"));
    assert!(!preview.tools.iter().any(|tool| tool.name == "apply_patch"));
    let settings = model
        .benchmark_settings(
            &AgentInvocationContext::default(),
            Role::Worker,
            &edit_specs(),
        )
        .await
        .unwrap();
    let frozen = model.freeze_for_benchmark(settings).unwrap();
    assert_eq!(
        frozen.edit_tool_for_run("fast", Role::Worker, None),
        Some(EditToolConfig::Edit)
    );
}

#[tokio::test]
async fn fast_model_entry_override_is_used_for_official_compaction() {
    let (mut model, requests) = routed_model(Ok(response()), "gpt-5.5+fast", None);
    openai(&mut model);
    let provider = model.providers.get_mut("local").unwrap();
    provider
        .profile
        .edit_tool
        .models
        .insert("gpt-5.5+fast".into(), EditToolConfig::Edit);
    provider.client = Arc::new(super::official_compaction::OfficialClient {
        requests: requests.clone(),
        fail: false,
    });
    model
        .compact_context(
            &AgentInvocationContext::default(),
            Role::Worker,
            &[],
            &[],
            &edit_specs(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        names(&requests.lock().unwrap()[0].tools),
        ["read", "edit", "write", "shell"]
    );
}

#[tokio::test]
async fn compaction_also_narrows_edit_tools() {
    for (provider_type, expected) in [
        (ProviderType::OpenAi, vec!["read", "apply_patch", "shell"]),
        (
            ProviderType::OpenAiCompatible,
            vec!["read", "edit", "write", "shell"],
        ),
    ] {
        let (mut model, requests) = routed_model(Ok(response()), "gpt-5.5", None);
        let provider = model.providers.get_mut("local").unwrap();
        provider.profile.provider_type = provider_type;
        provider.client = Arc::new(super::official_compaction::OfficialClient {
            requests: requests.clone(),
            fail: false,
        });
        model
            .compact_context(
                &AgentInvocationContext::default(),
                Role::Worker,
                &[],
                &[],
                &edit_specs(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(names(&requests.lock().unwrap()[0].tools), expected);
    }
}

#[tokio::test]
async fn unsupported_capability_still_sends_no_tools() {
    let (mut model, requests) = routed_model(Ok(response()), "gpt-5.5", None);
    openai(&mut model);
    let mut catalog = ModelCatalog::new();
    catalog.merge_discovered(vec!["gpt-5.5".into()]);
    let entry = catalog.get("gpt-5.5").unwrap().clone();
    catalog.merge_models_dev(vec![entry]);
    model.router = Router::new(
        vec![model.providers["local"].profile.clone()],
        &RoutingConfig::default(),
        catalog,
    )
    .unwrap();
    model
        .complete(
            &AgentInvocationContext {
                attempts: None,
                model_preference: Some(crate::ModelPreference {
                    profile: "local".into(),
                    model: None,
                    reasoning_effort: None,
                }),
                ..Default::default()
            },
            Role::Worker,
            &[],
            &edit_specs(),
        )
        .await
        .unwrap();
    assert!(requests.lock().unwrap()[0].tools.is_empty());
    model.providers.get_mut("local").unwrap().client =
        Arc::new(super::official_compaction::OfficialClient {
            requests: requests.clone(),
            fail: false,
        });
    model
        .compact_context(
            &AgentInvocationContext {
                attempts: None,
                model_preference: Some(crate::ModelPreference {
                    profile: "local".into(),
                    model: None,
                    reasoning_effort: None,
                }),
                ..Default::default()
            },
            Role::Worker,
            &[],
            &[],
            &edit_specs(),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(requests.lock().unwrap()[1].tools.is_empty());
}

#[tokio::test]
async fn gpt5_to_non_gpt_fallback_rebuilds_the_full_authorized_set() {
    let (mut model, requests) = super::fallback::fixture(vec![Some(ProviderError::Timeout), None]);
    let profile = &mut model.providers.get_mut("profile-0").unwrap().profile;
    profile.provider_type = ProviderType::OpenAi;
    profile.api_protocol = ApiProtocol::OpenAiResponses;
    profile.models = vec!["gpt-5.5".into()];
    profile.default_model = "gpt-5.5".into();
    let mut catalog = ModelCatalog::new();
    catalog.merge_discovered(vec!["gpt-5.5".into(), "model-1".into()]);
    model.router = Router::new(
        model
            .providers
            .values()
            .map(|p| p.profile.clone())
            .collect(),
        &RoutingConfig {
            routes: BTreeMap::from([(
                "worker".into(),
                (0..2)
                    .map(|i| RouteCandidateConfig {
                        profile: format!("profile-{i}"),
                        model: None,
                        auto: false,
                    })
                    .collect(),
            )]),
            ..Default::default()
        },
        catalog,
    )
    .unwrap();
    model.tool_router = model
        .router
        .clone()
        .requiring_capability(Capability::ToolCalling);
    let invocation = AgentInvocationContext {
        attempts: None,
        run_id: "fallback".into(),
        purpose: event_bus::RequestPurpose::Compaction,
        ..Default::default()
    };
    // Summary fallback does not move the agent's pin or request a switch boundary.
    model
        .complete(&invocation, Role::Worker, &[], &edit_specs())
        .await
        .unwrap();
    assert_eq!(
        names(&requests[0].lock().unwrap()[0].tools),
        ["read", "apply_patch", "shell"]
    );
    assert_eq!(
        names(&requests[1].lock().unwrap()[0].tools),
        ["read", "edit", "write", "shell"]
    );
}

#[tokio::test]
async fn same_profile_same_family_fallback_still_resummarizes_when_edit_tools_change() {
    let (mut model, _) = routed_model(Ok(response()), "gpt-5.4", Some("gpt-5.5"));
    openai(&mut model);
    model
        .providers
        .get_mut("local")
        .unwrap()
        .profile
        .edit_tool
        .models
        .insert("gpt-5.4".into(), EditToolConfig::Edit);
    let mut catalog = ModelCatalog::new();
    catalog.merge_discovered(vec!["gpt-5.4".into(), "gpt-5.5".into()]);
    model.router = Router::new(
        vec![model.providers["local"].profile.clone()],
        &RoutingConfig {
            routes: BTreeMap::from([(
                "worker".into(),
                ["gpt-5.4", "gpt-5.5"]
                    .into_iter()
                    .map(|model| RouteCandidateConfig {
                        profile: "local".into(),
                        model: Some(model.into()),
                        auto: false,
                    })
                    .collect(),
            )]),
            ..Default::default()
        },
        catalog,
    )
    .unwrap()
    .with_health(model.health.clone());
    model.tool_router = model
        .router
        .clone()
        .requiring_capability(Capability::ToolCalling);
    let requests = super::switch::QueueClient::install(
        &mut model,
        "local",
        vec![Err(ProviderError::Timeout)],
        false,
    );
    assert_eq!(
        call(&model, "same-family").await,
        Err(RuntimeError::ModelSwitch {
            selected: "local/gpt-5.5".into(),
            resummarize: true,
        }),
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(
        names(&requests.lock().unwrap()[0].tools),
        ["read", "edit", "write", "shell"]
    );
    model.model_switch_prepared(
        &AgentInvocationContext {
            attempts: None,
            run_id: "same-family".into(),
            ..Default::default()
        },
        Role::Worker,
        "local/gpt-5.5",
    );
    call(&model, "same-family").await.unwrap();
    assert_eq!(
        names(&requests.lock().unwrap()[1].tools),
        ["read", "apply_patch", "shell"]
    );
}

#[tokio::test]
async fn fallback_edit_family_switch_resummarizes_before_rebuilding_from_original_tools() {
    // Same model family, different profile override: identity/family heuristics alone are insufficient.
    let (mut model, requests) = super::fallback::fixture(vec![Some(ProviderError::Timeout), None]);
    model
        .providers
        .get_mut("profile-0")
        .unwrap()
        .profile
        .edit_tool
        .profile = Some(EditToolConfig::ApplyPatch);
    let first = call(&model, "run").await;
    assert!(
        matches!(
            first,
            Err(RuntimeError::ModelSwitch {
                resummarize: true,
                ..
            })
        ),
        "{first:?}"
    );
    assert!(requests[1].lock().unwrap().is_empty());
    assert_eq!(
        names(&requests[0].lock().unwrap()[0].tools),
        ["read", "apply_patch", "shell"]
    );
    // The agent loop has prepared the context and retries on the updated pin.
    model.model_switch_prepared(
        &AgentInvocationContext {
            attempts: None,
            run_id: "run".into(),
            ..Default::default()
        },
        Role::Worker,
        "profile-1/model-1",
    );
    call(&model, "run").await.unwrap();
    assert_eq!(
        names(&requests[1].lock().unwrap()[0].tools),
        ["read", "edit", "write", "shell"]
    );
}
