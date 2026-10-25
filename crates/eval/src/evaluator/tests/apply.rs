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

const IMAGE_PROVIDER: &str = "[providers.codex]\ntype='openai-codex'\nmodels=['chat',{id='studio',enabled=true,kind='image_generation'}]\ndefault_model='chat'\n";
const STUDIO_WORKER: &str = "[routing.routes]\npool=[{profile='codex',model='studio'}]\n[agents.worker]\nmodels=[{route='pool'}]\n";

fn dropin(fixture: &Fixture, text: &str) -> PathBuf {
    let directory = fixture.user.path().join("config.d");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("providers.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn reload(fixture: &Fixture) -> config::Config {
    config::Config::load_unresolved(&config::LoadOptions {
        user_config_dir: Some(fixture.user.path().to_owned()),
        read_env: false,
        ..Default::default()
    })
    .unwrap()
}

fn assert_preserved(fixture: &Fixture, main: &str, path: &Path, expected_dropin: &str) {
    assert_eq!(saved(fixture), main);
    assert_eq!(std::fs::read_to_string(path).unwrap(), expected_dropin);
    assert!(!fixture.user.path().join("config.toml.tmp").exists());
    assert!(!path.with_extension("toml.tmp").exists());
    assert_eq!(fixture.host.reloads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn apply_accepts_valid_image_chain_with_explicit_kind_in_dropin() {
    let fixture = Fixture::new();
    let document = format!(
        "{CONFIG}\n[routing.routes]\nart=[{{profile='codex',model='studio'}}]\n[agents.roles.image_generator]\nmodels=[{{route='art'}}]\n"
    );
    std::fs::write(fixture.user.path().join("config.toml"), &document).unwrap();
    let path = dropin(&fixture, IMAGE_PROVIDER);
    let approval = fixture.answer_approval(true);
    let result = fixture
        .call_in_run(
            "eval_apply_config",
            json!({"overlay":"[agents.worker.generation]\ntemperature=0.2\n"}),
        )
        .await;
    if result.is_error {
        approval.abort();
    }
    assert!(!result.is_error, "{}", result.content);
    approval.await.unwrap();
    let loaded = reload(&fixture);
    assert!(loaded.model_kind_problems().is_empty());
    assert_eq!(loaded.agents.worker.generation.temperature, Some(0.2));
    assert_eq!(loaded.agents.roles.image_generator.models[0].route, "art");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), IMAGE_PROVIDER);
    assert!(!fixture.user.path().join("config.toml.tmp").exists());
    assert_eq!(fixture.host.reloads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn apply_rejects_image_worker_from_dropin_before_approval_or_write() {
    let fixture = Fixture::new();
    let main = "version=3\n";
    std::fs::write(fixture.user.path().join("config.toml"), main).unwrap();
    let path = dropin(&fixture, IMAGE_PROVIDER);
    let mut events = fixture.bus.subscribe();
    let approval = fixture.answer_approval(true);
    let result = fixture
        .call_in_run("eval_apply_config", json!({"overlay":STUDIO_WORKER}))
        .await;
    approval.abort();
    assert!(
        result.is_error && result.content.contains("agents.worker.models"),
        "{}",
        result.content
    );
    while let Ok(event) = tokio::time::timeout(Duration::from_millis(50), events.recv()).await {
        assert!(
            !matches!(
                event.unwrap().kind,
                EventKind::Tool(ToolEvent::ApprovalRequested { .. })
            ),
            "invalid candidate was offered for approval"
        );
    }
    assert_preserved(&fixture, main, &path, IMAGE_PROVIDER);
}

#[tokio::test]
async fn apply_revalidates_dropin_kind_after_waiting_for_approval() {
    let fixture = Fixture::new();
    let main = "version=3\n";
    std::fs::write(fixture.user.path().join("config.toml"), main).unwrap();
    let path = dropin(
        &fixture,
        &IMAGE_PROVIDER.replace("image_generation", "chat"),
    );
    let mut events = fixture.bus.subscribe();
    let bus = Arc::clone(&fixture.bus);
    let edit_path = path.clone();
    let approval = tokio::spawn(async move {
        loop {
            let event = events.recv().await.unwrap();
            if let EventKind::Tool(ToolEvent::ApprovalRequested { call_id, .. }) = event.kind {
                std::fs::write(edit_path, IMAGE_PROVIDER).unwrap();
                bus.emit(Event::new(ToolEvent::ApprovalResolved {
                    call_id,
                    approved: true,
                }));
                break;
            }
        }
    });
    let result = fixture
        .call_in_run("eval_apply_config", json!({"overlay":STUDIO_WORKER}))
        .await;
    approval.await.unwrap();
    assert!(
        result.is_error && result.content.contains("agents.worker.models"),
        "{}",
        result.content
    );
    assert_preserved(&fixture, main, &path, IMAGE_PROVIDER);
}

#[tokio::test]
async fn apply_commits_exact_preview_bytes_after_valid_dropin_change() {
    let fixture = Fixture::new();
    let main = saved(&fixture);
    let initial = IMAGE_PROVIDER.replace("image_generation", "chat");
    let path = dropin(&fixture, &initial);
    let latest = format!("{initial}\n[agents.worker.generation]\ntemperature=0.8\n");
    let mut events = fixture.bus.subscribe();
    let bus = Arc::clone(&fixture.bus);
    let edit_path = path.clone();
    let latest_for_approval = latest.clone();
    let approval = tokio::spawn(async move {
        loop {
            if let EventKind::Tool(ToolEvent::ApprovalRequested { call_id, input, .. }) =
                events.recv().await.unwrap().kind
            {
                std::fs::write(edit_path, latest_for_approval).unwrap();
                bus.emit(Event::new(ToolEvent::ApprovalResolved {
                    call_id,
                    approved: true,
                }));
                return input.unwrap();
            }
        }
    });
    let result = fixture
        .call_in_run(
            "eval_apply_config",
            json!({"overlay":"[agents.worker.generation]\ntemperature=0.2\n"}),
        )
        .await;
    let shown = approval.await.unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(
        !shown["summary"]
            .as_str()
            .unwrap()
            .contains("config.d still overrides")
    );
    let result: Value = serde_json::from_str(&result.content).unwrap();
    let after = saved(&fixture);
    let label = fixture
        .user
        .path()
        .join("config.toml")
        .display()
        .to_string();
    let exact_diff = similar::TextDiff::from_lines(&main, &after)
        .unified_diff()
        .context_radius(3)
        .header(&label, &label)
        .to_string();
    assert!(
        shown["summary"].as_str().unwrap().contains(&exact_diff),
        "committed text differs from the approved diff"
    );
    assert!(after.contains("temperature = 0.2"));
    assert_eq!(
        reload(&fixture).agents.worker.generation.temperature,
        Some(0.8)
    );
    assert_eq!(result["overridden_by_config_d"], json!(["agents"]));
    assert_eq!(std::fs::read_to_string(path).unwrap(), latest);
    assert!(!fixture.user.path().join("config.toml.tmp").exists());
}

#[tokio::test]
async fn apply_preview_and_commit_share_effective_dropin_override_result() {
    let fixture = Fixture::new();
    let text = format!("{IMAGE_PROVIDER}\n[agents.worker.generation]\ntemperature=0.8\n");
    let path = dropin(&fixture, &text);
    let approval = fixture.answer_approval(true);
    let result = fixture
        .call_in_run(
            "eval_apply_config",
            json!({"overlay":"[agents.worker.generation]\ntemperature=0.2\n"}),
        )
        .await;
    let shown = approval.await.unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(
        shown["summary"]
            .as_str()
            .unwrap()
            .contains("config.d still overrides agents")
    );
    let result: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(result["overridden_by_config_d"], json!(["agents"]));
    assert_eq!(
        reload(&fixture).agents.worker.generation.temperature,
        Some(0.8)
    );
    assert!(saved(&fixture).contains("temperature = 0.2"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), text);
}
