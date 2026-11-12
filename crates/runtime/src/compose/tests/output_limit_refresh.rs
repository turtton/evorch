//! A catalog refresh reaches only runs admitted after it; admitted runs keep the output
//! limits their first request resolved (prompt cache contract).
use super::output_limits::{fixture, models_catalog};
use super::*;

fn context(run_id: &str, model_id: &str) -> AgentInvocationContext {
    AgentInvocationContext {
        run_id: run_id.to_owned(),
        model_preference: Some(crate::ModelPreference {
            profile: "local".into(),
            model: Some(model_id.into()),
            reasoning_effort: None,
        }),
        ..Default::default()
    }
}

async fn fixture_for(model_id: &str) -> (RoutedModel, Arc<Mutex<Vec<ChatRequest>>>) {
    let (mut model, requests) = fixture(model_id).await;
    model.verification = tokio::sync::OnceCell::new_with(Some(BTreeMap::from([(
        "local".to_string(),
        Ok(vec![model_id.to_owned()]),
    )])));
    (model, requests)
}

async fn admit(model: &impl AgentModel, run_id: &str, model_id: &str) {
    model
        .admit(&context(run_id, model_id), Role::Worker)
        .await
        .unwrap();
}

async fn send(model: &impl AgentModel, run_id: &str, model_id: &str) {
    model
        .complete(&context(run_id, model_id), Role::Worker, &[], &[])
        .await
        .unwrap();
}

fn limits(requests: &Arc<Mutex<Vec<ChatRequest>>>) -> Vec<Option<u64>> {
    requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request.max_tokens)
        .collect()
}

async fn publish(model: &impl AgentModel, models: &[(&str, u64)]) {
    model.publish_model_catalog(models_catalog(models).await);
}

// Given: run A sent a request and a refresh lowers the limit
// When: A sends again and a new run B starts
// Then: A keeps its limit for every request; B starts with the refreshed one.
#[tokio::test]
async fn refresh_applies_to_new_runs_and_keeps_admitted_runs() {
    let (model, requests) = fixture_for("catalog-model").await;
    admit(&model, "run-a", "catalog-model").await;
    send(&model, "run-a", "catalog-model").await;

    publish(&model, &[("catalog-model", 64_000)]).await;
    send(&model, "run-a", "catalog-model").await;
    admit(&model, "run-b", "catalog-model").await;
    send(&model, "run-b", "catalog-model").await;
    send(&model, "run-a", "catalog-model").await;
    send(&model, "run-b", "catalog-model").await;

    assert_eq!(
        limits(&requests),
        [
            Some(128_000),
            Some(128_000),
            Some(64_000),
            Some(128_000),
            Some(64_000)
        ]
    );
}

// Given: run A is admitted but has sent nothing / When: a refresh lands before its first
// request / Then: that first request already uses the generation A was admitted with.
#[tokio::test]
async fn admitted_run_keeps_its_generation_before_the_first_request() {
    let (model, requests) = fixture_for("catalog-model").await;
    admit(&model, "run-a", "catalog-model").await;

    publish(&model, &[("catalog-model", 64_000)]).await;
    // A second admission of the same run (for example after a wait) does not re-pin it.
    admit(&model, "run-a", "catalog-model").await;
    send(&model, "run-a", "catalog-model").await;

    assert_eq!(limits(&requests), [Some(128_000)]);
}

// Given: the catalog did not know the model when A was admitted
// When: a refresh adds it / Then: A stays unlimited (None), the new run B gets the limit.
#[tokio::test]
async fn unknown_model_stays_unknown_for_the_admitted_run() {
    let (model, requests) = fixture_for("late-model").await;
    admit(&model, "run-a", "late-model").await;
    send(&model, "run-a", "late-model").await;

    publish(&model, &[("late-model", 32_000)]).await;
    send(&model, "run-a", "late-model").await;
    send(&model, "run-b", "late-model").await;

    assert_eq!(limits(&requests), [None, None, Some(32_000)]);
}

// Given: a request without an admission / When: a refresh follows its first request
// Then: direct calls are pinned just like admitted runs.
#[tokio::test]
async fn direct_calls_without_admission_are_pinned_on_first_use() {
    let (model, requests) = fixture_for("catalog-model").await;
    send(&model, "direct", "catalog-model").await;

    publish(&model, &[("catalog-model", 64_000)]).await;
    send(&model, "direct", "catalog-model").await;

    assert_eq!(limits(&requests), [Some(128_000), Some(128_000)]);
}

// Given: explicit and preset limits / When: the catalog is refreshed
// Then: precedence is unchanged (explicit, then positive preset, then positive catalog).
#[tokio::test]
async fn precedence_survives_a_refresh() {
    for (explicit, preset_limit, expected) in [
        (Some(777), Some(64_000), Some(777)),
        (None, Some(64_000), Some(64_000)),
        (None, Some(0), Some(32_000)),
        (None, None, Some(32_000)),
    ] {
        let (mut model, requests) = fixture_for("catalog-model").await;
        model.agents.worker.base.generation.max_tokens = explicit;
        if let Some(limit) = preset_limit {
            super::output_limits::preset(&mut model, "local", "catalog-model", limit);
        }
        publish(&model, &[("catalog-model", 32_000)]).await;
        // The role binding (no explicit preference) carries the configured explicit limit.
        complete(&model, "run-new").await.unwrap();
        assert_eq!(
            limits(&requests),
            [expected],
            "{explicit:?} {preset_limit:?}"
        );
    }
}

// Given: a model replaced by reconfiguration / When: the replacement adopts the shared state
// Then: runs of the old model keep their generation and later refreshes still reach new runs.
#[tokio::test]
async fn reconfiguration_carries_pins_and_generations_over() {
    let (old, _) = fixture_for("catalog-model").await;
    admit(&old, "run-a", "catalog-model").await;
    publish(&old, &[("catalog-model", 64_000)]).await;

    let (new, requests) = fixture_for("catalog-model").await;
    new.restore_output_limit_catalogs(&old.output_limit_catalogs().unwrap());
    send(&new, "run-a", "catalog-model").await;
    send(&new, "run-b", "catalog-model").await;
    publish(&new, &[("catalog-model", 32_000)]).await;
    send(&new, "run-b", "catalog-model").await;
    send(&new, "run-c", "catalog-model").await;

    assert_eq!(
        limits(&requests),
        [Some(128_000), Some(64_000), Some(64_000), Some(32_000)]
    );
}

// Given: a generation that could not load its catalog / When: a model adopts that state
// Then: new runs retry loading while runs pinned to the failed generation keep None.
#[tokio::test]
async fn reconfiguration_retries_an_unavailable_generation_for_new_runs_only() {
    let (mut failed, _) = fixture_for("catalog-model").await;
    failed.metadata_catalog = tokio::sync::OnceCell::new_with(Some(None));
    assert!(failed.pinned_catalog("run-a").await.is_none());
    let shared = failed.output_limit_catalogs().unwrap();

    let (fresh, requests) = fixture_for("catalog-model").await;
    fresh.restore_output_limit_catalogs(&shared);
    send(&fresh, "run-a", "catalog-model").await;
    send(&fresh, "run-b", "catalog-model").await;

    assert_eq!(limits(&requests), [None, Some(128_000)]);
}

// Given: two admissions racing on one generation / When: a refresh publishes meanwhile
// Then: both pinned the same generation, which no longer is the current one; a later
// run pins the published generation.
#[tokio::test]
async fn concurrent_admissions_share_a_generation_and_publish_supersedes_it() {
    let (model, _) = fixture_for("catalog-model").await;
    let shared = model.output_limit_catalogs().unwrap();
    tokio::join!(
        admit(&model, "run-a", "catalog-model"),
        admit(&model, "run-b", "catalog-model"),
    );
    assert_eq!(shared.is_pinned_to_current("run-a"), Some(true));
    assert_eq!(shared.is_pinned_to_current("run-b"), Some(true));

    publish(&model, &[("catalog-model", 64_000)]).await;
    admit(&model, "run-c", "catalog-model").await;

    assert_eq!(shared.is_pinned_to_current("run-a"), Some(false));
    assert_eq!(shared.is_pinned_to_current("run-b"), Some(false));
    assert_eq!(shared.is_pinned_to_current("run-c"), Some(true));
}

// Given: a switchable model wrapping a routed one / When: it is reconfigured and refreshed
// Then: publishing reaches whichever model is current, and admitted runs stay pinned.
#[tokio::test]
async fn switchable_model_publishes_to_the_current_model_and_keeps_pins() {
    let (first, _) = fixture_for("catalog-model").await;
    let switchable = SwitchableModel::new(Arc::new(first));
    admit(&switchable, "run-a", "catalog-model").await;

    let (second, requests) = fixture_for("catalog-model").await;
    switchable.replace(Arc::new(second));
    publish(&switchable, &[("catalog-model", 64_000)]).await;
    send(&switchable, "run-a", "catalog-model").await;
    send(&switchable, "run-b", "catalog-model").await;

    assert_eq!(limits(&requests), [Some(128_000), Some(64_000)]);
}
