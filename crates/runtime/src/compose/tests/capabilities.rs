use super::*;
use crate::ModelPreference;

fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "read".into(),
        description: String::new(),
        input_schema: serde_json::json!({"type": "object"}),
    }]
}

#[tokio::test]
async fn tools_follow_declaration_when_preferred_model_support_is_unknown_or_unsupported() {
    // Given: an explicitly selected model with unknown or unsupported tools.
    for confirmed in [false, true] {
        let (mut model, requests) = routed_model(Ok(response()), "custom", None);
        let mut catalog = ModelCatalog::builtin();
        catalog.merge_discovered(vec!["custom".into()]);
        if confirmed {
            let entry = catalog.get("custom").expect("placeholder").clone();
            catalog.merge_models_dev(vec![entry]);
        }
        model.router = Router::new(
            vec![profile("custom", &["custom"])],
            &RoutingConfig::default(),
            catalog,
        )
        .expect("router");
        // When: assembling the provider request with tool specs.
        model
            .complete(
                &AgentInvocationContext {
                    category: None,
                    run_id: "run".into(),
                    model_preference: Some(ModelPreference {
                        profile: "local".into(),
                        model: Some("custom".into()),
                    }),
                },
                Role::Worker,
                &[],
                &specs(),
            )
            .await
            .expect("text-only degradation");
        // Then: only an explicit unsupported declaration strips tool specs.
        assert_eq!(
            requests.lock().expect("requests")[0].tools.is_empty(),
            confirmed
        );
    }
}

#[tokio::test]
async fn automatic_tool_flow_proceeds_when_model_support_is_unknown() {
    // Given: a discovered model is the only routed candidate.
    let (model, requests) = routed_model(Ok(response()), "custom", None);
    // When: tools are required by an automatic flow.
    let result = model
        .complete(
            &AgentInvocationContext {
                category: None,
                run_id: "run".into(),
                model_preference: None,
            },
            Role::Worker,
            &[],
            &specs(),
        )
        .await;
    // Then: unknown capability is treated as eligible and the provider is called.
    result.expect("unknown capability must not fail routing");
    assert_eq!(requests.lock().expect("requests").len(), 1);
}

#[tokio::test]
async fn tool_specs_are_unchanged_when_canonical_support_is_supported() {
    // Given: a supported builtin model.
    let (model, requests) = routed_model(Ok(response()), "gpt-4o", None);
    let tools = specs();
    // When: completing a tool flow.
    model
        .complete(
            &AgentInvocationContext {
                category: None,
                run_id: "run".into(),
                model_preference: None,
            },
            Role::Worker,
            &[],
            &tools,
        )
        .await
        .expect("supported route");
    // Then: tool specs reach the provider unchanged.
    assert_eq!(requests.lock().expect("requests")[0].tools, tools);
}
