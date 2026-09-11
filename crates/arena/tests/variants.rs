use arena::{ArenaSpec, Confirmation, run};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use providers::{ProviderAuth, provider::openai_compatible::OpenAiCompatibleClient};
use std::time::Duration;

fn spec() -> ArenaSpec {
    serde_json::from_value(serde_json::json!({
        "id": "variants", "project": "p",
        "task": {"id": "task", "prompt": "1+1", "expected_output": "2"},
        "configs": [
            {"id": "baseline", "profile": "local", "model": "base", "attribution": "worker"},
            {"id": "variant", "profile": "local", "model": "base", "attribution": "worker",
             "variant": {
                "prompt": {"version": "v2", "system": "fixture-system"},
                "routing": [{"role": "reviewer", "model": "review-model"}],
                "topology": {"kind": "sequence", "roles": ["worker", "reviewer"]}
             }}
        ],
        "max_output_tokens": 16, "total_token_budget": 2000, "timeout_ms": 2000
    }))
    .expect("variant spec")
}

#[tokio::test]
async fn promoted_variant_is_used_by_the_next_run() {
    // Given: a baseline and a two-stage variant, using real HTTP and SQLite adapters.
    let server = StreamingMockOpenAi::spawn(vec![
        ScriptedResponse::text_stream("a", "base", ["3"]).with_usage(3, 1),
        ScriptedResponse::text_stream("b", "base", ["draft"]).with_usage(4, 1),
        ScriptedResponse::text_stream("c", "review-model", ["2"]).with_usage(5, 1),
        ScriptedResponse::text_stream("d", "base", ["3"]).with_usage(3, 1),
        ScriptedResponse::text_stream("e", "base", ["draft"]).with_usage(4, 1),
        ScriptedResponse::text_stream("f", "review-model", ["2"]).with_usage(5, 1),
    ]);
    let client =
        OpenAiCompatibleClient::new(server.base_url(), "local", Duration::from_secs(2), None)
            .expect("client");
    let dir = tempfile::tempdir().expect("tempdir");
    let storage_config = storage::StorageConfig {
        db_path: dir.path().join("arena.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(storage_config.clone()).expect("storage");
    let runner = arena::Runner {
        client: &client,
        auth: &ProviderAuth::new("test"),
        storage: storage.handle(),
    };
    let first = run(&spec(), &runner).await.expect("first run");
    let restored = arena::ArenaReport::from_traces(
        storage::Database::open(&storage_config)
            .expect("db")
            .eval_traces("p")
            .expect("traces"),
    )
    .expect("report");
    // When: the confirmed, persisted winner is used as a config in a new run.
    let mut next = spec();
    next.id = "next".into();
    next.configs[1] = restored
        .promote_config("variant", Confirmation::Approved)
        .expect("promotion");
    let report = run(&next, &runner).await.expect("next run");
    // Then: routing, prompt version, topology and actual requests all survive promotion.
    assert_eq!(first.selected(), vec!["variant"]);
    assert!(first.promote("variant", Confirmation::Approved).is_err());
    assert!(
        first
            .promote_config("variant", Confirmation::Declined)
            .is_err()
    );
    let differences = first.compare("baseline").expect("comparison");
    assert_eq!(differences.len(), 1);
    let difference = &differences[0];
    assert!(difference.prompt_changed && difference.routing_changed && difference.topology_changed);
    assert!(!difference.model_changed && !difference.attribution_changed);
    assert!(!difference.baseline_passed && difference.candidate_passed);
    assert_eq!(difference.token_delta, 7);
    let mut tampered = first.traces().to_vec();
    tampered[1]
        .execution
        .as_mut()
        .expect("execution")
        .variant
        .prompt = None;
    assert!(
        arena::ArenaReport::from_traces(tampered)
            .expect("report")
            .selected()
            .is_empty()
    );
    assert_eq!(report.selected(), vec!["variant"]);
    assert_eq!(report.traces()[1].input_tokens, 9);
    assert_eq!(report.traces()[1].output_tokens, 2);
    assert_eq!(first.traces()[1].execution, report.traces()[1].execution);
    let requests = server.recorded_requests();
    assert_eq!(requests.len(), 6);
    assert_eq!(requests[2].body["model"], "review-model");
    assert_eq!(requests[1].body["messages"][0]["role"], "system");
    assert_eq!(requests[2].body["messages"][2]["role"], "assistant");
    for index in 0..3 {
        assert_eq!(requests[index].body, requests[index + 3].body);
    }
}

#[test]
fn variant_spec_accepts_executable_differences() {
    // Given / When: a serialized prompt/routing/topology comparison is parsed.
    let spec = spec();
    // Then: its executable configuration is valid.
    assert!(spec.validate().is_ok());
}

#[test]
fn invalid_variants_are_rejected_before_dispatch() {
    // Given: invalid topology, routing and prompt configurations.
    let mut invalid = Vec::new();
    let mut empty = spec();
    empty.configs[1].variant.topology = arena::Topology::Sequence { roles: vec![] };
    invalid.push(empty);
    let mut duplicate = spec();
    let route = duplicate.configs[1].variant.routing[0].clone();
    duplicate.configs[1].variant.routing.push(route);
    invalid.push(duplicate);
    let mut unused = spec();
    unused.configs[1].variant.routing[0].role = arena::Attribution::Planner;
    invalid.push(unused);
    let mut empty_version = spec();
    empty_version.configs[1]
        .variant
        .prompt
        .as_mut()
        .expect("prompt")
        .version
        .clear();
    invalid.push(empty_version);
    // When / Then: all invalid configurations fail validation.
    for spec in invalid {
        assert!(spec.validate().is_err());
    }
}

#[tokio::test]
async fn variant_prompt_reservation_does_not_spend_another_candidates_budget() {
    // Given: a variant system prompt larger than its equal budget share.
    let server = StreamingMockOpenAi::spawn(vec![
        ScriptedResponse::text_stream("a", "base", ["2"]).with_usage(3, 1),
    ]);
    let client =
        OpenAiCompatibleClient::new(server.base_url(), "local", Duration::from_secs(2), None)
            .expect("client");
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = storage::Storage::open(storage::StorageConfig {
        db_path: dir.path().join("budget.db"),
        ..Default::default()
    })
    .expect("storage");
    let runner = arena::Runner {
        client: &client,
        auth: &ProviderAuth::new("test"),
        storage: storage.handle(),
    };
    let mut spec = spec();
    spec.configs[1]
        .variant
        .prompt
        .as_mut()
        .expect("prompt")
        .system = " x".repeat(1000);
    // When: both candidates receive independent equal shares.
    let report = run(&spec, &runner).await.expect("run");
    // Then: only the affordable baseline is dispatched; incomplete comparisons cannot promote.
    assert_eq!(server.recorded_requests().len(), 1);
    assert_eq!(report.traces()[0].failure, None);
    assert_eq!(
        report.traces()[1].failure,
        Some(arena::FailureAttribution::BudgetExceeded)
    );
    assert!(report.selected().is_empty());
}
