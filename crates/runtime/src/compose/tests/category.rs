use super::*;

#[tokio::test]
async fn conversation_without_category_binding_uses_worker_model_and_generation() {
    let (model, requests) = routed_model(Ok(response()), "claude-sonnet-4-5", Some("gpt-5.5"));
    assert!(!model.agents.worker.categories.contains_key("conversation"));
    let binding = model
        .agents
        .binding_for("worker", Some("conversation"))
        .unwrap();
    assert_eq!(binding, model.agents.binding_for("worker", None).unwrap());
    assert_eq!(
        model.selected_model(Role::Worker, Some("conversation")),
        "local/gpt-5.5"
    );
    let bus = Arc::new(EventBus::new(64));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(model),
    );
    let run = runtime
        .delegate_chat(
            "conversation",
            Role::Worker,
            "hello".into(),
            crate::RunConfig {
                conversation: true,
                category: Some("conversation".into()),
                ..Default::default()
            },
        )
        .expect("direct chat");
    assert_eq!(runtime.wait(run).await, Ok(event_bus::AgentRunPhase::Done));
    let recorded = requests.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].model, "gpt-5.5");
    assert_eq!(recorded[0].temperature, Some(0.25));
    assert_eq!(recorded[0].max_tokens, Some(321));
}

#[tokio::test]
async fn delegated_worker_uses_category_model_when_quick_is_bound() {
    // Given: role と quick を異なるモデルへ結び、実際の run を provider seam で観測する。
    let (mut model, requests) = routed_model(Ok(response()), "claude-sonnet-4-5", Some("gpt-5.5"));
    model.agents.worker.categories.insert(
        "quick".into(),
        config::CategoryBindingConfig {
            logical_model: Some("worker-quick-lm".into()),
            ..Default::default()
        },
    );
    let profiles = model
        .providers
        .values()
        .map(|p| p.profile.clone())
        .collect();
    let routing = RoutingConfig {
        routes: BTreeMap::from([
            (
                "worker".into(),
                vec![RouteCandidateConfig {
                    profile: "local".into(),
                    model: Some("gpt-5.5".into()),
                }],
            ),
            (
                "worker-quick-lm".into(),
                vec![RouteCandidateConfig {
                    profile: "local".into(),
                    model: Some("claude-sonnet-4-5".into()),
                }],
            ),
        ]),
    };
    let mut catalog = ModelCatalog::new();
    catalog.merge_discovered(vec!["gpt-5.5".into(), "claude-sonnet-4-5".into()]);
    model.router = Router::new(profiles, &routing, catalog).expect("valid routes");
    model.tool_router = model
        .router
        .clone()
        .requiring_capability(Capability::ToolCalling);
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let model = Arc::new(model);
    let runtime = AgentRuntime::new(bus, executor, model.clone());

    // When: quick カテゴリ付き Worker を終端まで実行する。
    let run = runtime.delegate_background(
        Role::Worker,
        "work".into(),
        crate::RunConfig {
            category: Some("quick".into()),
            ..Default::default()
        },
    );
    assert_eq!(runtime.wait(run).await, Ok(event_bus::AgentRunPhase::Done));

    // Then: role モデルではなく quick モデルが provider に届く。
    let recorded = requests.lock().expect("requests lock");
    assert_eq!(recorded[0].model, "claude-sonnet-4-5");
    let selected = model.selected_model(Role::Worker, Some("quick"));
    assert_eq!(selected, "local/claude-sonnet-4-5");
    assert_eq!(model.selected_model(Role::Worker, None), "local/gpt-5.5");
    assert_eq!(
        crate::prompt::classify(&selected),
        crate::ModelFamily::Claude
    );
}
