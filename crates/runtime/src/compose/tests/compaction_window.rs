use super::*;

fn routed_model_with_catalog_window(
    default_model: &str,
    route_model: Option<&str>,
    context_window: u64,
    reply: ChatResponse,
) -> RoutedModel {
    let (mut model, _) = routed_model(Ok(reply), default_model, route_model);
    let selected = route_model.unwrap_or(default_model);
    let mut catalog = ModelCatalog::new();
    catalog.merge_discovered(vec![selected.to_string()]);
    let mut entry = catalog
        .get(selected)
        .expect("discovered fixture model")
        .clone();
    entry.context_window = context_window;
    entry.capabilities.tool_calling = true;
    catalog.merge_models_dev(vec![entry]);
    model.router = Router::new(
        vec![model.providers["local"].profile.clone()],
        &RoutingConfig {
            routes: BTreeMap::from([(
                "worker".into(),
                vec![RouteCandidateConfig {
                    profile: "local".into(),
                    model: route_model.map(ToString::to_string),
                }],
            )]),
        },
        catalog,
    )
    .expect("valid fixture route");
    model.tool_router = model
        .router
        .clone()
        .requiring_capability(Capability::ToolCalling);
    model
}

#[test]
fn selected_catalog_window_uses_routed_model_not_profile_default() {
    // Given: the route selects a different model than the profile default.
    let model = routed_model_with_catalog_window(
        "fixture-default",
        Some("fixture-selected"),
        96_000,
        response(),
    );
    // When: looking up the exact selected identity through AgentModel.
    let selected = model.selected_model(Role::Worker, None);
    let window = model.catalog_context_window(&selected);
    // Then: the selected model's real catalog window is used.
    assert_eq!(selected, "local/fixture-selected");
    assert_eq!(window, Some(96_000));
}

#[test]
fn discovered_placeholder_has_no_catalog_window() {
    // Given: discovery only supplies an identity, not a known window.
    let (model, _) = routed_model(Ok(response()), "unknown-model", None);
    // When: looking up its selected identity.
    let window = model.catalog_context_window(&model.selected_model(Role::Worker, None));
    // Then: the zero-sized placeholder does not cause immediate compaction.
    assert_eq!(window, None);
}

#[test]
fn switchable_model_forwards_catalog_window() {
    // Given: the production adapter behind the live-reload wrapper.
    let routed = routed_model_with_catalog_window("fixture-model", None, 96_000, response());
    let model = SwitchableModel::new(Arc::new(routed));
    // When: querying the selected model through the wrapper.
    let window = model.catalog_context_window(&model.selected_model(Role::Worker, None));
    // Then: the catalog is not hidden by the wrapper's default implementation.
    assert_eq!(window, Some(96_000));
}

#[tokio::test]
async fn automatic_compaction_records_catalog_window_through_live_runtime() {
    // Given: a real routed runtime with a 32K catalog and default 200K config.
    let mut reply = response();
    reply.message.content = vec![ContentBlock::Text {
        text: "history ".repeat(16_000),
    }];
    let routed = routed_model_with_catalog_window("fixture-model", None, 32_000, reply);
    let bus = Arc::new(EventBus::new(128));
    let mut receiver = bus.subscribe();
    let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(
        bus,
        executor,
        Arc::new(SwitchableModel::new(Arc::new(routed))),
    )
    .with_compaction(config::CompactionConfig {
        summarizer: config::SummarizerKind::Structural,
        keep_recent_tokens: 1,
        ..config::CompactionConfig::default()
    });
    let run = runtime.delegate_background(
        Role::Worker,
        "retain the goal".into(),
        crate::RunConfig {
            interactive: true,
            ..crate::RunConfig::default()
        },
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !runtime
            .inspect_agent(run)
            .is_ok_and(|agent| agent.phase == crate::AgentRunPhase::Waiting)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // When: opening the next automatic-compaction boundary.
    runtime.send_message(run, "continue".into()).unwrap();
    runtime.wait(run).await.unwrap();
    // Then: both trigger and compaction use the real 32K catalog window.
    let compacted = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let event_bus::EventKind::Compaction(event) = receiver.recv().await.unwrap().kind {
                break event;
            }
        }
    })
    .await
    .unwrap();
    let event_bus::CompactionEvent::Compacted {
        context_window_tokens,
        window_source,
        estimated_tokens_before,
        ..
    } = compacted;
    assert_eq!(context_window_tokens, 32_000);
    assert_eq!(window_source, event_bus::WindowSource::Catalog);
    assert!((24_000..150_000).contains(&estimated_tokens_before));
}
