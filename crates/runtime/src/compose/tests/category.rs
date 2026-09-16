use super::*;

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
    model.router = Router::new(profiles, &routing, ModelCatalog::builtin()).expect("valid routes");
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
