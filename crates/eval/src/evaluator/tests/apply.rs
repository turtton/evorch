use super::*;

const ADOPTED: &str = "[routing.routes]\nfast = [{ profile = \"main\", model = \"gpt-test\" }]\n\n[agents.worker]\nmodels = [{ route = \"fast\" }]\n";

fn saved(fixture: &Fixture) -> String {
    std::fs::read_to_string(fixture.user.path().join("config.toml")).unwrap()
}

#[tokio::test]
async fn only_agents_and_routing_of_existing_profiles_are_applied() {
    // Given: the user's configuration without role profiles.
    let fixture = Fixture::new();
    let mut events = fixture.bus.subscribe();

    // When: overlays are applied that touch a section the savers do not write, the
    // evaluation settings, and a role profile that does not exist.
    let apply =
        |overlay: &str| fixture.call_in_run("eval_apply_config", json!({"overlay": overlay}));
    let sandbox = apply("[sandbox]\nweb_tools_enabled = false\n").await;
    let eval = apply("[eval]\nrepeat_target = 9\n").await;
    let profile = apply("[role_profiles.nope.agents.worker.generation]\ntemperature = 0.2\n").await;

    // Then: each is refused before the user is asked, and the file is untouched.
    assert!(
        sandbox.is_error && sandbox.content.contains("`sandbox` cannot be applied"),
        "{}",
        sandbox.content
    );
    assert!(
        eval.is_error && eval.content.contains("`eval` cannot be changed"),
        "{}",
        eval.content
    );
    assert!(
        profile.is_error && profile.content.contains("nope"),
        "{}",
        profile.content
    );
    let mut asked = 0;
    while let Ok(event) = tokio::time::timeout(Duration::from_millis(100), events.recv()).await {
        if let EventKind::Tool(ToolEvent::ApprovalRequested { .. }) = event.unwrap().kind {
            asked += 1;
        }
    }
    assert_eq!(asked, 0);
    assert_eq!(saved(&fixture), CONFIG);
}

#[tokio::test]
async fn a_declined_change_is_shown_as_a_diff_and_writes_nothing() {
    // Given: the user's configuration.
    let fixture = Fixture::new();

    // When: the user declines applying the adopted overlay.
    let approval = fixture.answer_approval(false);
    let result = fixture
        .call_in_run("eval_apply_config", json!({"overlay": ADOPTED}))
        .await;
    let shown = approval.await.unwrap();

    // Then: the request showed the change to config.toml as a diff, and nothing changed.
    assert_eq!(shown["kind"], "eval_apply_config");
    let summary = shown["summary"].as_str().unwrap();
    for expected in [
        "config.toml (agents, routing)",
        "+[agents.worker]",
        "+[[routing.routes.fast]]",
        " default_model = \"gpt-test\"",
    ] {
        assert!(summary.contains(expected), "{expected:?} in {summary}");
    }
    assert!(result.is_error && result.content.contains("nothing was written"));
    assert_eq!(saved(&fixture), CONFIG);
    assert_eq!(fixture.host.reloads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_approved_change_is_saved_and_the_models_are_reloaded() {
    // Given: the user's configuration.
    let fixture = Fixture::new();

    // When: the user approves applying the adopted overlay, which is then applied again.
    let approval = fixture.answer_approval(true);
    let result = fixture
        .call_in_run("eval_apply_config", json!({"overlay": ADOPTED}))
        .await;
    approval.await.unwrap();
    let again = fixture
        .call_in_run("eval_apply_config", json!({"overlay": ADOPTED}))
        .await;

    // Then: the saver wrote both sections and kept the rest, the models were
    // recomposed once, and the second application found nothing to change.
    assert!(!result.is_error, "{}", result.content);
    let value: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(value["changed"], true);
    assert_eq!(value["sections"], json!(["agents", "routing"]));
    assert_eq!(value["reload_error"], Value::Null);
    assert!(value["note"].as_str().unwrap().contains("recomposed"));
    let text = saved(&fixture);
    assert!(text.contains("[providers.main]"), "{text}");
    let config = config::Config::load_unresolved(&config::LoadOptions {
        user_config_dir: Some(fixture.user.path().to_path_buf()),
        read_env: false,
        ..config::LoadOptions::default()
    })
    .unwrap();
    assert_eq!(config.agents.worker.base.head_route(), Some("fast"));
    assert!(config.routing.routes.contains_key("fast"));
    assert_eq!(fixture.host.reloads.load(Ordering::SeqCst), 1);
    let again: Value = serde_json::from_str(&again.content).unwrap();
    assert_eq!(again["changed"], false);
}
