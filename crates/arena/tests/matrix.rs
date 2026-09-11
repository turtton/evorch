use arena::{ArenaReport, ArenaSpec};

fn report(split: &str) -> ArenaReport {
    let spec: ArenaSpec = serde_json::from_value(serde_json::json!({
        "id": "matrix", "project": "p", "split": split,
        "task": {"id": "t", "prompt": "task", "expected_output": "ok"},
        "configs": [
            {"id": "a", "profile": "local", "model": "a", "attribution": "worker"},
            {"id": "b", "profile": "local", "model": "b", "attribution": "qa"}
        ],
        "max_output_tokens": 16, "total_token_budget": 200, "timeout_ms": 1000
    }))
    .expect("split and role spec");
    let manifest = serde_json::to_string(&spec).expect("manifest");
    ArenaReport::from_traces(
        spec.configs
            .iter()
            .enumerate()
            .map(|(index, config)| arena::EvalTrace {
                id: config.id.clone(),
                arena_id: spec.id.clone(),
                project: spec.project.clone(),
                task_id: spec.task.id.clone(),
                task_spec: manifest.clone(),
                config_id: config.id.clone(),
                profile: config.profile.clone(),
                model: config.model.clone(),
                attribution: config.attribution,
                execution: None,
                output: "ok".into(),
                input_tokens: 3 + u64::try_from(index).expect("index"),
                output_tokens: 1,
                elapsed_ms: 5,
                failure: None,
            })
            .collect(),
    )
    .expect("report")
}

#[test]
fn different_roles_do_not_eliminate_each_other() {
    // Given / When: successful worker and QA candidates with different costs.
    let report = report("validation");
    // Then: both role-specific winners survive.
    assert_eq!(report.selected(), vec!["a", "b"]);
}

#[test]
fn all_evaluation_splits_round_trip_in_the_manifest() {
    // Given / When / Then: every supported split is retained in persisted evidence.
    for split in ["train", "validation", "holdout", "redteam"] {
        let report = report(split);
        let manifest: serde_json::Value =
            serde_json::from_str(&report.traces()[0].task_spec).expect("json");
        assert_eq!(manifest["split"], split);
    }
}

#[test]
fn training_results_cannot_promote() {
    // Given / When / Then: training success is not independent adoption evidence.
    assert!(
        report("train")
            .promote("a", arena::Confirmation::Approved)
            .is_err()
    );
}

#[test]
fn activation_changes_the_consumed_route_only_after_confirmation() {
    let report = report("holdout");
    let dir = tempfile::tempdir().expect("dir");
    let mut active = config::Config::default();
    let original = active.clone();
    assert!(
        report
            .activate(
                "a",
                arena::Confirmation::Declined,
                arena::ActiveConfig {
                    config: &mut active,
                    user_config_dir: dir.path(),
                }
            )
            .is_err()
    );
    assert_eq!(active, original);
    report
        .activate(
            "a",
            arena::Confirmation::Approved,
            arena::ActiveConfig {
                config: &mut active,
                user_config_dir: dir.path(),
            },
        )
        .expect("activate");
    let binding = active.agents.binding_for("worker", None).expect("binding");
    assert_eq!(
        active.routing.routes[&binding.logical_model][0]
            .model
            .as_deref(),
        Some("a")
    );
}

#[test]
fn activated_prompt_is_resolved_by_the_normal_preset_path() {
    let mut traces = report("validation").traces().to_vec();
    let mut spec: ArenaSpec = serde_json::from_str(&traces[0].task_spec).expect("spec");
    spec.configs[0].variant.prompt = Some(arena::PromptVersion {
        version: "v2".into(),
        system: "fixture".into(),
    });
    let manifest = serde_json::to_string(&spec).expect("manifest");
    for trace in &mut traces {
        trace.task_spec.clone_from(&manifest);
    }
    traces[0].execution = Some(Box::new(arena::EvalExecution {
        variant: spec.configs[0].variant.clone(),
        steps: vec![arena::EvalStep {
            role: arena::Attribution::Worker,
            model: "a".into(),
            output: "ok".into(),
            input_tokens: 3,
            output_tokens: 1,
        }],
    }));
    let report = ArenaReport::from_traces(traces).expect("report");
    let dir = tempfile::tempdir().expect("dir");
    let mut active = config::Config::default();
    report
        .activate(
            "a",
            arena::Confirmation::Approved,
            arena::ActiveConfig {
                config: &mut active,
                user_config_dir: dir.path(),
            },
        )
        .expect("activate");
    let binding = active.agents.binding_for("worker", None).expect("binding");
    let sources = config::resolve_prompt_sources(&active, Some(dir.path())).expect("sources");
    assert!(
        sources
            .appendices
            .contains_key(binding.preset.as_ref().expect("preset reference"))
    );
}
