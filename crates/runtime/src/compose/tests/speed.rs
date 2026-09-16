use super::*;

#[tokio::test]
async fn fast_model_routes_base_id_and_preserves_tools() {
    // Given: builtinで能力が既知のbaseと、それを使うfastプロファイル。
    let (model, requests) = routed_model(Ok(response()), "gpt-4o+fast", None);
    let tools = vec![ToolSpec {
        name: "read_file".into(),
        description: "Read".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }];
    // When: tool付きのルートを解決して送信する。
    model
        .complete(
            &AgentInvocationContext {
                category: None,
                run_id: "fast".into(),
                model_preference: None,
            },
            Role::Worker,
            &[],
            &tools,
        )
        .await
        .expect("fast route is eligible");
    // Then: base IDとpriority、全toolがcanonical requestへ渡る。
    let recorded = requests.lock().expect("requests");
    assert_eq!(recorded[0].model, "gpt-4o");
    assert_eq!(
        recorded[0].service_tier,
        Some(providers::ServiceTier::Priority)
    );
    assert_eq!(recorded[0].tools, tools);
}

#[tokio::test]
async fn fast_explicit_preference_strips_marker() {
    // Given: 明示的なfast選択 / When: complete / Then: wire用モデルにmarkerを残さない。
    let (model, requests) = routed_model(Ok(response()), "gpt-x+fast", None);
    model
        .complete(
            &AgentInvocationContext {
                category: None,
                run_id: "fast-preference".into(),
                model_preference: Some(crate::ModelPreference {
                    profile: "local".into(),
                    model: None,
                }),
            },
            Role::Worker,
            &[],
            &[],
        )
        .await
        .expect("explicit fast request");
    let recorded = requests.lock().expect("requests");
    assert_eq!(recorded[0].model, "gpt-x");
    assert_eq!(
        recorded[0].service_tier,
        Some(providers::ServiceTier::Priority)
    );
}

#[tokio::test]
async fn standard_model_omits_service_tier() {
    // Given: standard ID / When: complete / Then: tierは未指定。
    let (model, requests) = routed_model(Ok(response()), "gpt-x", None);
    complete(&model, "standard")
        .await
        .expect("standard request");
    let recorded = requests.lock().expect("requests");
    assert_eq!(recorded[0].model, "gpt-x");
    assert_eq!(recorded[0].service_tier, None);
}
