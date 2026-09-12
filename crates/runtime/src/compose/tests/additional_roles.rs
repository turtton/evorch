use super::*;

#[test]
fn librarian_binding_resolves_when_profile_preference_is_absent() {
    // Given: the runtime role and default config bindings.
    let agents = config::AgentsConfig::default();
    // When: using the same key resolution as RoutedModel without a preference.
    let binding = agents.binding_for(role_key(Role::Librarian), None);
    // Then: model resolution reaches the librarian logical model.
    assert_eq!(
        binding.expect("librarian binding").logical_model,
        "librarian"
    );
}

#[tokio::test]
async fn additional_roles_keep_affinity_per_run() {
    let (mut model, requests) = routed_model(Ok(response()), "default-model", Some("route-model"));
    model.agents.roles.planner.logical_model = Some("worker".into());
    model.agents.roles.oracle.logical_model = Some("worker".into());
    model.agents.roles.multimodal_looker.logical_model = Some("worker".into());
    for role in [Role::Planner, Role::Oracle, Role::MultimodalLooker] {
        for run_id in ["same", "same", "other"] {
            model
                .complete(
                    &AgentInvocationContext {
                        run_id: format!("{}-{run_id}", role.name()),
                        model_preference: None,
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
    assert_eq!(
        models,
        ["route-model", "default-model", "route-model"].repeat(3)
    );
}
