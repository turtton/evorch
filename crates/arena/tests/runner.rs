use arena::{ArenaConfig, ArenaSpec, Attribution, FailureAttribution, TaskSpec, run};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use providers::{ProviderAuth, provider::openai_compatible::OpenAiCompatibleClient};
use std::time::Duration;

fn spec() -> ArenaSpec {
    ArenaSpec {
        id: "run".into(),
        project: "p".into(),
        task: TaskSpec {
            id: "task".into(),
            prompt: "1+1".into(),
            expected_output: "2".into(),
        },
        configs: ["a", "b"]
            .into_iter()
            .map(|id| ArenaConfig {
                id: id.into(),
                profile: "local".into(),
                model: id.into(),
                attribution: Attribution::Worker,
            })
            .collect(),
        max_output_tokens: 16,
        total_token_budget: 200,
        timeout_ms: 1000,
    }
}

#[tokio::test]
async fn mock_comparison_persists_same_task_and_requires_confirmation() {
    // Given: two models evaluated through the real HTTP provider and SQLite writer.
    let server = StreamingMockOpenAi::spawn(vec![
        ScriptedResponse::text_stream("a", "a", ["2"]).with_usage(3, 1),
        ScriptedResponse::text_stream("b", "b", ["3"]).with_usage(3, 1),
    ]);
    let client =
        OpenAiCompatibleClient::new(server.base_url(), "local", Duration::from_secs(2), None)
            .expect("client");
    let dir = tempfile::tempdir().expect("tempdir");
    let config = storage::StorageConfig {
        db_path: dir.path().join("arena.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).expect("storage");
    let runner = arena::Runner {
        client: &client,
        auth: &ProviderAuth::new("test"),
        storage: storage.handle(),
    };
    // When: the arena runs one shared spec with two configurations.
    let report = run(&spec(), &runner).await.expect("run");
    // Then: only the exact-match config is eligible; routing promotion requires approval.
    assert_eq!(report.selected(), vec!["a"]);
    assert_eq!(
        report.traces()[1].failure,
        Some(FailureAttribution::OutputMismatch)
    );
    assert!(report.promote("a", arena::Confirmation::Declined).is_err());
    let candidate = report
        .promote("a", arena::Confirmation::Approved)
        .expect("candidate");
    assert_eq!(candidate.model.as_deref(), Some("a"));
    assert!(report.promote("b", arena::Confirmation::Approved).is_err());
    let partial = arena::ArenaReport::from_traces(report.traces()[..1].to_vec()).expect("partial");
    assert!(partial.promote("a", arena::Confirmation::Approved).is_err());
    let mut interrupted = report.traces().to_vec();
    interrupted[1].failure = Some(FailureAttribution::Timeout);
    let interrupted = arena::ArenaReport::from_traces(interrupted).expect("interrupted");
    assert!(
        interrupted
            .promote("a", arena::Confirmation::Approved)
            .is_err()
    );
    let requests = server.recorded_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body["messages"], requests[1].body["messages"]);
    assert_eq!(requests[0].body["max_tokens"], 16);
    assert_eq!(
        storage::Database::open(&config)
            .expect("db")
            .eval_traces("p")
            .expect("traces"),
        report.traces()
    );
}

#[tokio::test]
async fn exhausted_budget_prevents_http_requests() {
    // Given: a budget smaller than a single bounded attempt.
    let server = StreamingMockOpenAi::spawn(vec![]);
    let client =
        OpenAiCompatibleClient::new(server.base_url(), "local", Duration::from_secs(2), None)
            .expect("client");
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = storage::Storage::open(storage::StorageConfig {
        db_path: dir.path().join("budget.db"),
        ..Default::default()
    })
    .expect("storage");
    let mut spec = spec();
    spec.total_token_budget = 1;
    let runner = arena::Runner {
        client: &client,
        auth: &ProviderAuth::new("test"),
        storage: storage.handle(),
    };
    // When: the runner checks its budget before dispatch.
    let report = run(&spec, &runner).await.expect("run");
    // Then: all configurations fail closed without sending a request.
    assert!(server.recorded_requests().is_empty());
    assert!(
        report
            .traces()
            .iter()
            .all(|t| t.failure == Some(FailureAttribution::BudgetExceeded))
    );
}

#[tokio::test]
async fn candidate_budget_is_independent_of_order_and_other_usage() {
    let server = StreamingMockOpenAi::spawn(vec![
        ScriptedResponse::text_stream("a", "a", ["2"]).with_usage(120, 1),
        ScriptedResponse::text_stream("b", "b", ["2"]).with_usage(3, 1),
        ScriptedResponse::text_stream("b", "b", ["2"]).with_usage(3, 1),
        ScriptedResponse::text_stream("a", "a", ["2"]).with_usage(120, 1),
    ]);
    let client =
        OpenAiCompatibleClient::new(server.base_url(), "local", Duration::from_secs(2), None)
            .expect("client");
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = storage::Storage::open(storage::StorageConfig {
        db_path: dir.path().join("order.db"),
        ..Default::default()
    })
    .expect("storage");
    let runner = arena::Runner {
        client: &client,
        auth: &ProviderAuth::new("test"),
        storage: storage.handle(),
    };
    let mut spec = spec();
    let first = run(&spec, &runner).await.expect("first");
    spec.id = "reverse".into();
    spec.configs.reverse();
    let reversed = run(&spec, &runner).await.expect("reversed");
    for report in [first, reversed] {
        assert_eq!(
            report
                .traces()
                .iter()
                .find(|t| t.config_id == "a")
                .expect("a")
                .failure,
            Some(FailureAttribution::BudgetExceeded)
        );
        assert_eq!(
            report
                .traces()
                .iter()
                .find(|t| t.config_id == "b")
                .expect("b")
                .failure,
            None
        );
        assert!(report.promote("b", arena::Confirmation::Approved).is_err());
    }
    assert_eq!(server.recorded_requests().len(), 4);
}
