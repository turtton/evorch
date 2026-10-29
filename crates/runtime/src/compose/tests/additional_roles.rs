use super::*;

#[test]
fn web_researcher_binding_resolves_when_profile_preference_is_absent() {
    // Given: the runtime role and default config bindings.
    let agents = config::AgentsConfig::default();
    // When: using the same key resolution as RoutedModel without a preference.
    let binding = agents.binding_for(role_key(Role::WebResearcher), None);
    // Then: model resolution reaches the web_researcher logical model.
    assert_eq!(
        binding.expect("web_researcher binding").head_route(),
        "web_researcher"
    );
}

#[tokio::test]
async fn additional_roles_keep_affinity_per_run() {
    // Given: 各 role が default model とは異なる override を使うルート
    let (mut model, requests) = routed_model(Ok(response()), "default-model", Some("route-model"));
    model.agents.roles.web_researcher.models = vec![config::ChainEntryConfig::route("worker")];
    model.agents.roles.planner.models = vec![config::ChainEntryConfig::route("worker")];
    model.agents.roles.advisor.models = vec![config::ChainEntryConfig::route("worker")];
    model.agents.roles.multimodal_looker.models = vec![config::ChainEntryConfig::route("worker")];
    // When: 各 role で同じ run を二度、別 run を一度 complete する
    for role in [
        Role::WebResearcher,
        Role::Planner,
        Role::Advisor,
        Role::MultimodalLooker,
    ] {
        for run_id in ["same", "same", "other"] {
            model
                .complete(
                    &AgentInvocationContext {
                        attempts: None,
                        turn: None,
                        category: None,
                        run_id: format!("{}-{run_id}", role.name()),
                        model_preference: None,
                        purpose: Default::default(),
                    },
                    role,
                    &[],
                    &[],
                )
                .await
                .expect("role route");
        }
    }
    let models = requests
        .lock()
        .expect("requests")
        .iter()
        .map(|request| request.model.clone())
        .collect::<Vec<_>>();
    // Then: 同じ run の再解決でもピンした override が維持される
    assert_eq!(
        models,
        ["route-model", "route-model", "route-model"].repeat(4)
    );
}
