use super::*;

#[tokio::test]
async fn recovery_requires_catalog_publication_without_loading_or_pinning_during_validation() {
    let (original, _) = output_limits::fixture("catalog-model").await;
    complete(&original, "run-1").await.unwrap();
    let input = original.checkpoint_provider_input("run-1").unwrap();
    assert_eq!(input.request.max_tokens, Some(128_000));
    let pins = original.route_pins("run-1");
    let (mut current, requests) = routed_model(Ok(response()), "catalog-model", None);
    current.agents.worker.base.generation.max_tokens = None;
    current.provider_configs = original.provider_configs.clone();
    let invocation = AgentInvocationContext {
        run_id: "run-1".into(),
        ..Default::default()
    };
    let check = || {
        current.validate_checkpoint_compatibility(&invocation, Role::Worker, &pins, Some(&input))
    };
    assert!(check().is_err());
    assert!(current.metadata_catalog.get().is_none());
    assert!(current.output_limit_state().current_catalog().is_none());
    assert_eq!(
        current.output_limit_state().is_pinned_to_current("run-1"),
        None
    );
    assert!(current.route_pins("run-1").is_empty());
    assert!(current.recovery_output_limits.lock().unwrap().is_empty());
    assert!(requests.lock().unwrap().is_empty());

    current.output_limit_state().publish(
        original
            .metadata_catalog
            .get()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone(),
    );
    check().unwrap();
    assert!(current.metadata_catalog.get().is_none());
    assert_eq!(
        current.output_limit_state().is_pinned_to_current("run-1"),
        None
    );
    assert!(current.route_pins("run-1").is_empty());
    assert!(current.recovery_output_limits.lock().unwrap().is_empty());
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn recovery_validation_is_pure_and_checks_effective_settings_not_display_model() {
    let (original, _) = routed_model_with_effort(Ok(response()), "model-a", None, Some("high"));
    complete(&original, "run-1").await.unwrap();
    let input = original.checkpoint_provider_input("run-1").unwrap();
    let pins = original.route_pins("run-1");
    assert_eq!(input.request.model, "model-a");
    assert_eq!(
        original.selected_model_for_run("run-1", Role::Worker, None),
        "local/model-a"
    );
    let invocation = AgentInvocationContext {
        run_id: "run-1".into(),
        ..Default::default()
    };
    for difference in [
        "none",
        "temperature",
        "reasoning",
        "max_tokens",
        "profile",
        "protocol",
        "edit",
        "affinity",
    ] {
        let (mut current, requests) =
            routed_model_with_effort(Ok(response()), "model-a", None, Some("high"));
        let mut saved = input.clone();
        match difference {
            "temperature" => current.agents.worker.base.generation.temperature = Some(0.9),
            "reasoning" => {
                current.agents.worker.base.models[0].reasoning_effort = Some("low".into())
            }
            "max_tokens" => current.agents.worker.base.generation.max_tokens = Some(322),
            "profile" => saved.profile = "other".into(),
            "protocol" => saved.protocol = ApiProtocol::OpenAiResponses,
            "edit" => saved.edit_tool = config::EditToolConfig::ApplyPatch,
            "affinity" => saved.affinity_key = "another-run".into(),
            _ => {}
        }
        let before = current.affinity.lock().unwrap().clone();
        let result = current.validate_checkpoint_compatibility(
            &invocation,
            Role::Worker,
            &pins,
            Some(&saved),
        );
        assert_eq!(
            result.is_ok(),
            difference == "none",
            "{difference}: {result:?}"
        );
        assert_eq!(
            *current.affinity.lock().unwrap(),
            before,
            "pure validation mutated affinity"
        );
        assert!(requests.lock().unwrap().is_empty());
        assert!(current.checkpoint_inputs.lock().unwrap().is_empty());
        assert!(current.recovery_output_limits.lock().unwrap().is_empty());
        assert_eq!(
            current.output_limit_state().is_pinned_to_current("run-1"),
            None
        );
    }
}

#[tokio::test]
async fn recovery_validates_fallback_pin_without_adopting_it_until_ack() {
    let (original, _) = fallback::fixture(vec![Some(ProviderError::Timeout), None]);
    complete(&original, "run-1").await.unwrap();
    let input = original.checkpoint_provider_input("run-1").unwrap();
    let pins = original.route_pins("run-1");
    assert_eq!(pins[0].position, 1);
    let (current, requests) = fallback::fixture(vec![None, None]);
    let invocation = AgentInvocationContext {
        run_id: "run-1".into(),
        ..Default::default()
    };
    current
        .validate_checkpoint_compatibility(&invocation, Role::Worker, &pins, Some(&input))
        .unwrap();
    assert!(current.route_pins("run-1").is_empty());
    current.adopt_recovery_checkpoint("run-1", &pins, Some(&input));
    complete(&current, "run-1").await.unwrap();
    assert_eq!(current.route_pins("run-1"), pins);
    assert!(requests[0].lock().unwrap().is_empty());
    assert_eq!(requests[1].lock().unwrap().len(), 1);
    let mut invalid = pins;
    invalid[0].model = "removed-model".into();
    assert!(
        current
            .validate_checkpoint_compatibility(&invocation, Role::Worker, &invalid, Some(&input))
            .is_err()
    );
}
