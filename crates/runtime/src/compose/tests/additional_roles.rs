use super::*;

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
