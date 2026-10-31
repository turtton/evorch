//! Codex subscription scenarios: official compaction, profile/model switches through a
//! live reconfiguration, ChatGPT account changes at token refresh, and forks.

use std::sync::Arc;

use event_bus::{AgentRunPhase, Event, EventBus, EventKind, EventReceiver, LifecycleEvent};
use mock_openai::codex_auth::{FAR_FUTURE_EXP, token_bundle};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use routing::MapEnv;
use runtime::compose::SwitchableModel;
use runtime::{
    AgentModel, AgentRuntime, ModelSource, Role, RunConfig, RunId, RuntimeComposition,
    compose_runtime,
};
use sandbox::credential::{CredentialStore, FileCredentialStore, Secret};
use serde_json::Value;
use tools::ToolExecutor;

use super::{
    Exchange, Route, assert_cache_chain, assert_compaction_settings,
    assert_compaction_shares_instructions, assert_precedes, assert_sealed, exchanges,
    is_assistant_text, is_call, posted, read_after, reasoning, reasoning_ids, route_config,
    text_after,
};
use crate::{BulkRead, CODEX_ACCOUNT, Wire, contract_prompts, through_phase};

#[path = "composite.rs"]
mod composite;

const MODEL_A: &str = "mock-model";
const MODEL_B: &str = "mock-model-b";
const ACCOUNT: &str = "acc";

/// A Codex worker whose routed model can be rebuilt in place, as a configuration
/// reload does, on one input-derived prompt-cache mock serving both models.
struct Live {
    directory: tempfile::TempDir,
    runtime: AgentRuntime,
    receiver: EventReceiver,
    mock: StreamingMockOpenAi,
    live: Arc<SwitchableModel>,
    store: Arc<FileCredentialStore>,
    bus: Arc<EventBus>,
    overrides: Vec<(String, u64)>,
    max_summary_bytes: u64,
    db: storage::StorageConfig,
    _storage: Option<storage::Storage>,
    _review: crate::support::ReviewStorage,
}

fn offline_env() -> Arc<MapEnv> {
    Arc::new(MapEnv::from_iter([(crate::KEY_ENV, "offline-test-key")]))
}

/// Stores the token bundle the Codex session reads for [`CODEX_ACCOUNT`].
fn sign_in(store: &FileCredentialStore, access: &str, account: &str, exp: u64) {
    store
        .set(
            CODEX_ACCOUNT,
            &Secret::from(
                token_bundle(access, &format!("refresh-{access}"), account, exp).to_string(),
            ),
        )
        .unwrap();
}

/// An already-expired mock ID token forces every token use through refresh.
const fn expiring() -> u64 {
    1
}

/// Routes `config` like production, keeping OAuth refreshes on the mock.
fn routed(
    config: &config::Config,
    store: &Arc<FileCredentialStore>,
    bus: &Arc<EventBus>,
    mock: &StreamingMockOpenAi,
) -> Arc<dyn AgentModel> {
    runtime::compose::compose_routed_model(
        config,
        routing::ComposeDeps {
            credential_store: store.clone(),
            event_bus: Some(bus.clone()),
            env: offline_env(),
            catalog: model::ModelCatalog::new(),
            factory: routing::factory::FactoryOptions {
                auth_base_url_override: Some(mock.codex_base_url()),
                ..Default::default()
            },
        },
        Some(test_support::model_catalog_dir()),
    )
    .unwrap()
}

/// Construction options beyond the script; [`Live::new`] uses the defaults.
#[derive(Default)]
struct Options {
    /// Compaction window overrides keyed by `profile/model`, fixed at composition.
    overrides: Vec<(String, u64)>,
    /// The `read` tool; defaults to [`BulkRead`].
    read: Option<Arc<dyn tools::Tool>>,
    /// Tools registered besides `read`.
    tools: Vec<Arc<dyn tools::Tool>>,
    /// Starting route; defaults to Codex `work` serving [`MODEL_A`].
    route: Option<Route<'static>>,
    /// Compaction summary byte limit; defaults to a tight 256.
    max_summary_bytes: Option<u64>,
    /// Use the HTTP model for text re-summaries instead of the structural fallback.
    model_summaries: bool,
}

impl Live {
    fn new(script: Vec<ScriptedResponse>, token_exp: u64, persist: bool) -> Self {
        Self::with(script, token_exp, persist, Options::default())
    }

    fn with(
        script: Vec<ScriptedResponse>,
        token_exp: u64,
        persist: bool,
        options: Options,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let route = options.route.unwrap_or(Route::codex("work", MODEL_A));
        let max_summary_bytes = options.max_summary_bytes.unwrap_or(256);
        let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
            script,
            vec![MODEL_A.into(), MODEL_B.into(), route.model.into()],
        );
        let store =
            Arc::new(FileCredentialStore::open(directory.path().join("credentials")).unwrap());
        sign_in(&store, "access-0", ACCOUNT, token_exp);
        let mut config = route_config(
            directory.path(),
            route,
            &mock.codex_base_url(),
            &options.overrides,
            max_summary_bytes,
        );
        if options.model_summaries {
            config.compaction.summarizer = config::SummarizerKind::Model;
        }
        let bus = Arc::new(EventBus::new(8192));
        let receiver = bus.subscribe();
        let mut executor = ToolExecutor::new(bus.clone());
        executor
            .register(options.read.unwrap_or_else(|| Arc::new(BulkRead)))
            .unwrap();
        for tool in options.tools {
            executor.register(tool).unwrap();
        }
        let live = Arc::new(SwitchableModel::new(routed(&config, &store, &bus, &mock)));
        let mut runtime = compose_runtime(RuntimeComposition {
            model_catalog_dir: Some(test_support::model_catalog_dir()),
            user_config_dir: Some(directory.path().join("empty-user-config")),
            config: &config,
            bus: bus.clone(),
            executor: Arc::new(executor),
            credential_store: store.clone(),
            env: offline_env(),
            model_source: ModelSource::Fixed(live.clone()),
            workspace: None,
        })
        .unwrap()
        .runtime
        .with_sequential_run_ids()
        .with_system_prompts(contract_prompts())
        .with_compaction(config.compaction.clone());
        let db = storage::StorageConfig {
            db_path: directory.path().join("history.db"),
            ..Default::default()
        };
        let storage = persist.then(|| storage::Storage::open(db.clone()).unwrap());
        if let Some(storage) = &storage {
            runtime =
                runtime.with_run_store(runtime::RunStore::open(&db, storage.handle()).unwrap());
        }
        // Follow-ups need review evidence storage; a persisted run store already has it.
        let review = crate::support::ReviewStorage::new();
        review.attach(&runtime);
        Self {
            directory,
            runtime,
            receiver,
            mock,
            live,
            store,
            bus,
            overrides: options.overrides,
            max_summary_bytes,
            db,
            _storage: storage,
            _review: review,
        }
    }

    /// Recreates the process on `db` (a copy of the store taken mid-run), serving
    /// `route`: a new event bus, routed model and runtime, as after a crash. The old
    /// runtime keeps its own store and is only used to cancel its blocked run.
    fn restart(&mut self, db: storage::StorageConfig, route: Route<'_>) -> AgentRuntime {
        self.bus = Arc::new(EventBus::new(8192));
        self.receiver = self.bus.subscribe();
        self.reroute(route, &self.mock.codex_base_url());
        let config = route_config(
            self.directory.path(),
            route,
            &self.mock.codex_base_url(),
            &self.overrides,
            self.max_summary_bytes,
        );
        let mut executor = ToolExecutor::new(self.bus.clone());
        executor.register(Arc::new(BulkRead)).unwrap();
        let storage = storage::Storage::open(db.clone()).unwrap();
        let runtime = compose_runtime(RuntimeComposition {
            model_catalog_dir: Some(test_support::model_catalog_dir()),
            user_config_dir: Some(self.directory.path().join("empty-user-config")),
            config: &config,
            bus: self.bus.clone(),
            executor: Arc::new(executor),
            credential_store: self.store.clone(),
            env: offline_env(),
            model_source: ModelSource::Fixed(self.live.clone()),
            workspace: None,
        })
        .unwrap()
        .runtime
        .with_system_prompts(contract_prompts())
        .with_compaction(config.compaction.clone())
        .with_run_store(runtime::RunStore::open(&db, storage.handle()).unwrap());
        self.db = db;
        self._storage = Some(storage);
        std::mem::replace(&mut self.runtime, runtime)
    }

    /// Rebuilds the routed model from a configuration serving `model` on `profile`.
    fn reconfigure(&self, profile: &str, model: &str) {
        self.reroute(Route::codex(profile, model), &self.mock.codex_base_url());
    }

    /// Rebuilds the routed model for `route` on `base_url`, as a configuration reload.
    fn reroute(&self, route: Route<'_>, base_url: &str) {
        let config = route_config(
            self.directory.path(),
            route,
            base_url,
            &self.overrides,
            self.max_summary_bytes,
        );
        self.live
            .replace(routed(&config, &self.store, &self.bus, &self.mock));
    }

    fn worker(&self, prompt: &str) -> RunId {
        self.runtime.delegate_background(
            Role::Worker,
            prompt.into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
    }

    async fn turn(&mut self, run: RunId, events: &mut Vec<Event>, message: Option<&str>) {
        if let Some(message) = message {
            self.runtime.send_message(run, message.into()).unwrap();
        }
        events.extend(through_phase(&mut self.receiver, run, AgentRunPhase::Waiting).await);
    }

    async fn finish(&self, runs: &[RunId]) {
        for run in runs {
            self.runtime.cancel(*run).unwrap();
            self.runtime.wait(*run).await.unwrap();
        }
        assert_eq!(self.mock.remaining_scripts(), 0);
    }

    fn requests(&self) -> Vec<Value> {
        posted(&self.mock, Wire::Codex.path())
    }

    fn accounts(&self) -> Vec<Option<String>> {
        self.mock
            .recorded_requests()
            .into_iter()
            .filter(|request| request.method == "POST" && request.path == Wire::Codex.path())
            .map(|request| request.chatgpt_account_id)
            .collect()
    }
}

fn compaction_blob(body: &Value) -> Option<&str> {
    super::input(body)
        .iter()
        .find(|item| item["type"] == "compaction")
        .and_then(|item| item["encrypted_content"].as_str())
}

fn ordinary(exchanges: &[Exchange]) -> Vec<&Value> {
    exchanges
        .iter()
        .filter(|exchange| !exchange.compaction)
        .map(|exchange| &exchange.body)
        .collect()
}

fn the_compaction(exchanges: &[Exchange]) -> &Value {
    let compactions: Vec<_> = exchanges
        .iter()
        .filter(|exchange| exchange.compaction)
        .collect();
    assert_eq!(compactions.len(), 1);
    &compactions[0].body
}

// Given: a Codex worker whose tool round carries R1 and whose answer carries R2
// When: a manual official compaction runs and the follow-up replies with R3 (tool call)
// and R4
// Then: the compaction request extends the cached prefix and receives R1/R2 verbatim
// (same scope), the boundary request is the one verified replacement carrying the new
// blob, R3 is replayed before its call afterwards, and each step reuses the prefix
#[tokio::test]
async fn official_compaction_receives_eligible_reasoning_then_reuses_the_new_prefix() {
    let (r1, r2, r3, r4) = (
        reasoning("rs_1"),
        reasoning("rs_2"),
        reasoning("rs_3"),
        reasoning("rs_4"),
    );
    let mut live = Live::new(
        vec![
            read_after(MODEL_A, 0, &r1),
            text_after(MODEL_A, "analysis before compaction", &r2),
            ScriptedResponse::compaction("compaction", MODEL_A, "blob-after-reasoning"),
            read_after(MODEL_A, 3, &r3),
            text_after(MODEL_A, "done", &r4),
        ],
        FAR_FUTURE_EXP,
        false,
    );
    let run = live.worker("Read with reasoning");
    let mut events = Vec::new();
    live.turn(run, &mut events, None).await;
    live.runtime.compact(run).unwrap();
    live.turn(run, &mut events, Some("Continue after this boundary"))
        .await;
    live.finish(&[run]).await;

    let exchanges = exchanges(live.requests(), run, &events);
    assert_eq!(exchanges.len(), 5);
    assert_cache_chain(&exchanges, &[]);
    for exchange in &exchanges {
        assert_sealed(&exchange.body, &[&r1, &r2, &r3, &r4]);
    }
    let compaction = the_compaction(&exchanges);
    // The compaction request is built from the same instructions, tools and settings as the
    // ordinary requests; only the history (and the cleared `include`) differs.
    let requests = ordinary(&exchanges);
    // Exactly one verified replacement, consumed by the first ordinary request after the
    // compaction; the requests after it reuse the new prefix (checked by the chain above).
    assert_eq!(
        exchanges
            .iter()
            .filter(|exchange| exchange.checkpoint.is_some())
            .count(),
        1
    );
    assert_compaction_shares_instructions(requests[1], compaction);
    assert_compaction_settings(requests[1], compaction);
    assert_eq!(reasoning_ids(compaction), ["rs_1", "rs_2"]);
    assert_precedes(compaction, &r1, is_call("call-0"));
    assert_precedes(
        compaction,
        &r2,
        is_assistant_text("analysis before compaction"),
    );
    assert_eq!(compaction_blob(requests[2]), Some("blob-after-reasoning"));
    assert_precedes(requests[3], &r3, is_call("call-3"));
}

// Given: a Codex worker on profile "work" with R1 (tool round) and R2 (answer)
// When: the configuration is reloaded with the same model, endpoint and account under
// profile "home", and the follow-up replies with R3 (tool call) and R4
// Then: the first request after the reload replays neither R1 nor R2 but keeps the
// rest of the history, and the next request replays R3 within the reused new prefix
#[tokio::test]
async fn profile_switch_drops_old_reasoning_then_reuses_the_new_scope() {
    let (r1, r2, r3, r4) = (
        reasoning("rs_1"),
        reasoning("rs_2"),
        reasoning("rs_3"),
        reasoning("rs_4"),
    );
    let mut live = Live::new(
        vec![
            read_after(MODEL_A, 0, &r1),
            text_after(MODEL_A, "on work", &r2),
            read_after(MODEL_A, 3, &r3),
            text_after(MODEL_A, "on home", &r4),
        ],
        FAR_FUTURE_EXP,
        false,
    );
    let run = live.worker("Read with reasoning");
    let mut events = Vec::new();
    live.turn(run, &mut events, None).await;
    live.reconfigure("home", MODEL_A);
    live.turn(run, &mut events, Some("Continue on the new profile"))
        .await;
    live.finish(&[run]).await;

    let exchanges = exchanges(live.requests(), run, &events);
    assert_eq!(exchanges.len(), 4);
    let requests = ordinary(&exchanges);
    assert_cache_chain(&exchanges, &[2]);
    for request in &requests {
        assert_sealed(request, &[&r1, &r2, &r3, &r4]);
    }
    assert_eq!(reasoning_ids(requests[1]), ["rs_1"]);
    assert!(reasoning_ids(requests[2]).is_empty());
    assert!(super::input(requests[2]).iter().any(is_call("call-0")));
    assert_eq!(reasoning_ids(requests[3]), ["rs_3"]);
    assert_precedes(requests[3], &r3, is_call("call-3"));
}

// Given: a Codex worker on model A with R1/R2, reloaded to model B (R3/R4), then back
// to model A (R5/R6)
// When: an official compaction runs on A and two more turns follow (R7, R8)
// Then: neither return to A revives R1/R2 nor B's R3/R4; the compaction input lacks
// every old item but carries A's new R5/R6 verbatim; after the compaction R7 is reused
#[tokio::test]
async fn model_round_trip_never_revives_old_reasoning_even_for_official_compaction() {
    let items: Vec<_> = (1..=8).map(|n| reasoning(&format!("rs_{n}"))).collect();
    let known: Vec<_> = items.iter().collect();
    let mut live = Live::new(
        vec![
            read_after(MODEL_A, 0, &items[0]),
            text_after(MODEL_A, "first on A", &items[1]),
            read_after(MODEL_B, 3, &items[2]),
            text_after(MODEL_B, "on B", &items[3]),
            read_after(MODEL_A, 4, &items[4]),
            text_after(MODEL_A, "back on A", &items[5]),
            ScriptedResponse::compaction("compaction", MODEL_A, "blob-back-on-a"),
            text_after(MODEL_A, "after compaction", &items[6]),
            text_after(MODEL_A, "done", &items[7]),
        ],
        FAR_FUTURE_EXP,
        false,
    );
    let run = live.worker("Read with reasoning");
    let mut events = Vec::new();
    live.turn(run, &mut events, None).await;
    live.reconfigure("work", MODEL_B);
    live.turn(run, &mut events, Some("Continue on B")).await;
    live.reconfigure("work", MODEL_A);
    live.turn(run, &mut events, Some("Back on A")).await;
    live.runtime.compact(run).unwrap();
    live.turn(run, &mut events, Some("After compaction")).await;
    live.turn(run, &mut events, Some("Once more")).await;
    live.finish(&[run]).await;

    let exchanges = exchanges(live.requests(), run, &events);
    assert_eq!(exchanges.len(), 9);
    assert_cache_chain(&exchanges, &[2, 4]);
    for exchange in &exchanges {
        assert_sealed(&exchange.body, &known);
    }
    let requests = ordinary(&exchanges);
    let models: Vec<_> = requests
        .iter()
        .map(|request| request["model"].as_str().unwrap())
        .collect();
    assert_eq!(
        models,
        [
            MODEL_A, MODEL_A, MODEL_B, MODEL_B, MODEL_A, MODEL_A, MODEL_A, MODEL_A
        ]
    );
    assert_eq!(reasoning_ids(requests[1]), ["rs_1"]);
    assert!(reasoning_ids(requests[2]).is_empty(), "A's items on B");
    assert_eq!(reasoning_ids(requests[3]), ["rs_3"]);
    assert!(
        reasoning_ids(requests[4]).is_empty(),
        "old A or B items back on A"
    );
    assert_eq!(reasoning_ids(requests[5]), ["rs_5"]);
    let compaction = the_compaction(&exchanges);
    assert_eq!(compaction["model"], MODEL_A);
    assert_eq!(reasoning_ids(compaction), ["rs_5", "rs_6"]);
    assert_precedes(compaction, &items[4], is_call("call-4"));
    assert_precedes(compaction, &items[5], is_assistant_text("back on A"));
    assert_eq!(compaction_blob(requests[6]), Some("blob-back-on-a"));
    assert!(
        !reasoning_ids(requests[6])
            .iter()
            .any(|id| ["rs_1", "rs_2", "rs_3", "rs_4"].contains(&id.as_str()))
    );
    assert_precedes(
        requests[7],
        &items[6],
        is_assistant_text("after compaction"),
    );
}

// Given: a Codex worker chain whose primary (model A) answers a tool round with R1 and
// a long answer with R2 and then fails, and whose fallback (model B) has a 40K window
// When: the next turn fails over to the fallback
// Then: B's official compaction input carries none of A's reasoning, one ModelSwitch
// boundary replaces history before B's first request (no R1/R2 there either), B's R3
// is replayed next and B's new prefix is reused in the input-derived cache
#[tokio::test]
async fn fallback_to_a_smaller_window_resummarizes_once_without_old_reasoning() {
    let review_storage = crate::support::ReviewStorage::new();
    let (r1, r2, r3, r4) = (
        reasoning("rs_1"),
        reasoning("rs_2"),
        reasoning("rs_3"),
        reasoning("rs_4"),
    );
    let old_reply = "old analysis that can be summarized ".repeat(3_000);
    let primary = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            read_after(MODEL_A, 0, &r1),
            text_after(MODEL_A, &old_reply, &r2),
        ],
        vec![MODEL_A.into()],
    );
    let small = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            ScriptedResponse::compaction("compaction", MODEL_B, "blob-on-b"),
            read_after(MODEL_B, 3, &r3),
            text_after(MODEL_B, "done on B", &r4),
        ],
        vec![MODEL_B.into()],
    );
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join(config::PROJECT_CONFIG_DIR)).unwrap();
    let credential =
        format!(r#"{{ type = "keyring", service = "evorch", account = "{CODEX_ACCOUNT}" }}"#);
    std::fs::write(
        config::project_main_config_path(directory.path()),
        format!(
            r#"
version = 3
[providers.primary]
type = "openai-codex"
api_protocol = "openai-codex-responses"
base_url = "{primary}"
credential = {credential}
models = ["{MODEL_A}"]
default_model = "{MODEL_A}"
[providers.small]
type = "openai-codex"
api_protocol = "openai-codex-responses"
base_url = "{small}"
credential = {credential}
models = [{{ id = "{MODEL_B}", enabled = true, metadata_source = "manual", context_window = 40000 }}]
default_model = "{MODEL_B}"
[[routing.routes.primary]]
profile = "primary"
[[routing.routes.small]]
profile = "small"
[agents.worker]
models = [{{ route = "primary" }}, {{ route = "small" }}]
[compaction]
context_window_tokens = 1000000
threshold = 0.5
keep_recent_tokens = 1
max_summary_bytes = 256
summarizer = "structural"
"#,
            primary = primary.codex_base_url(),
            small = small.codex_base_url(),
        ),
    )
    .unwrap();
    let config = config::Config::load(&config::LoadOptions {
        project_dir: Some(directory.path().into()),
        user_config_dir: Some(directory.path().join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(4096));
    let mut receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(Arc::new(BulkRead)).unwrap();
    let store = Arc::new(FileCredentialStore::open(directory.path().join("credentials")).unwrap());
    sign_in(&store, "access-0", ACCOUNT, FAR_FUTURE_EXP);
    let runtime = compose_runtime(RuntimeComposition {
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(directory.path().join("empty-user-config")),
        config: &config,
        bus: bus.clone(),
        executor: Arc::new(executor),
        credential_store: store.clone(),
        env: Arc::new(MapEnv::default()),
        model_source: ModelSource::Fixed(routed(&config, &store, &bus, &primary)),
        workspace: None,
    })
    .unwrap()
    .runtime
    .with_sequential_run_ids()
    .with_system_prompts(contract_prompts())
    .with_compaction(config.compaction.clone());
    review_storage.attach(&runtime);
    let run = runtime.delegate_background(
        Role::Worker,
        "Read with reasoning".into(),
        RunConfig {
            interactive: true,
            ..Default::default()
        },
    );
    let mut events = through_phase(&mut receiver, run, AgentRunPhase::Waiting).await;
    runtime
        .send_message(run, "Continue after the failover".into())
        .unwrap();
    events.extend(through_phase(&mut receiver, run, AgentRunPhase::Done).await);
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(primary.remaining_scripts() + small.remaining_scripts(), 0);

    let checkpoints: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Compaction(event_bus::CompactionEvent::Compacted {
                reason,
                checkpoint_id,
                ..
            }) => Some((*reason, checkpoint_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(checkpoints.len(), 1, "{checkpoints:?}");
    assert_eq!(checkpoints[0].0, event_bus::CompactionReason::ModelSwitch);
    let before = posted(&primary, Wire::Codex.path());
    let small_requests = posted(&small, Wire::Codex.path());
    let (compactions, after): (Vec<_>, Vec<_>) = small_requests
        .iter()
        .cloned()
        .partition(super::is_compaction_request);
    assert!(before.len() >= 3, "{}", before.len());
    assert_eq!((compactions.len(), after.len()), (1, 2));
    // B's official compaction receives only items eligible for B: none of A's.
    assert_sealed(&compactions[0], &[]);
    assert!(reasoning_ids(&compactions[0]).is_empty());
    for request in before.iter().chain(&after) {
        assert_sealed(request, &[&r1, &r2, &r3, &r4]);
    }
    let mut bodies = before.clone();
    bodies.extend(small_requests);
    let failed: Vec<_> = (2..before.len()).collect();
    let exchanges = super::exchanges_with_failures(bodies, run, &events, &failed);
    assert_cache_chain(&exchanges, &[2]);
    assert_eq!(compaction_blob(&after[0]), Some("blob-on-b"));
    assert_eq!(reasoning_ids(&before[1]), ["rs_1"]);
    assert!(reasoning_ids(&after[0]).is_empty());
    assert_eq!(reasoning_ids(&after[1]), ["rs_3"]);
    assert_precedes(&after[1], &r3, is_call("call-3"));
}

/// Runs two turns with already-expired mock tokens, so each request
/// refreshes first; the auth server answers the second turn for `second_account`.
async fn refresh_between_turns(second_account: &str) -> (Live, Vec<Exchange>) {
    let (r1, r2, r3, r4) = (
        reasoning("rs_1"),
        reasoning("rs_2"),
        reasoning("rs_3"),
        reasoning("rs_4"),
    );
    let mut live = Live::new(
        vec![
            read_after(MODEL_A, 0, &r1),
            text_after(MODEL_A, "first turn", &r2),
            read_after(MODEL_A, 3, &r3),
            text_after(MODEL_A, "second turn", &r4),
        ],
        expiring(),
        false,
    );
    live.mock
        .set_oauth_token_response(token_bundle("access-1", "refresh-1", ACCOUNT, expiring()));
    let run = live.worker("Read with reasoning");
    let mut events = Vec::new();
    live.turn(run, &mut events, None).await;
    live.mock.set_oauth_token_response(token_bundle(
        "access-2",
        "refresh-2",
        second_account,
        expiring(),
    ));
    live.turn(run, &mut events, Some("Continue after the refresh"))
        .await;
    live.finish(&[run]).await;

    let refreshes = live
        .mock
        .recorded_requests()
        .iter()
        .filter(|request| request.path.ends_with("/oauth/token"))
        .count();
    assert!(refreshes >= 4, "every request refreshes first: {refreshes}");
    let authorizations: Vec<_> = live
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == Wire::Codex.path())
        .map(|request| request.authorization)
        .collect();
    let bearer = |token: &str| Some(format!("Bearer {token}"));
    assert_eq!(
        authorizations,
        [
            bearer("access-1"),
            bearer("access-1"),
            bearer("access-2"),
            bearer("access-2")
        ]
    );
    assert_eq!(
        live.accounts(),
        [ACCOUNT, ACCOUNT, second_account, second_account].map(|account| Some(account.to_owned()))
    );
    let exchanges = exchanges(live.requests(), run, &events);
    assert_eq!(exchanges.len(), 4);
    for exchange in &exchanges {
        assert_sealed(&exchange.body, &[&r1, &r2, &r3, &r4]);
    }
    (live, exchanges)
}

// Given: a Codex worker whose access token is refreshed before every request
// When: the second turn uses a new access token for the same ChatGPT account
// Then: there is no boundary: R1/R2 are replayed verbatim after the refresh and every
// request reuses the previous input in the input-derived cache
#[tokio::test]
async fn same_account_token_refresh_keeps_replaying_reasoning() {
    let (_live, exchanges) = refresh_between_turns(ACCOUNT).await;
    assert_cache_chain(&exchanges, &[]);
    let requests = ordinary(&exchanges);
    assert_eq!(reasoning_ids(requests[2]), ["rs_1", "rs_2"]);
    assert_eq!(reasoning_ids(requests[3]), ["rs_1", "rs_2", "rs_3"]);
}

// Given: the same refresh-per-request worker
// When: the refresh before the second turn returns a token for another ChatGPT account
// Then: that turn is a boundary: R1/R2 are not replayed to the other account, the rest
// of the history is kept, and the other account's R3 is reused afterwards
#[tokio::test]
async fn account_change_at_refresh_drops_old_reasoning_then_reuses_the_new_scope() {
    let (_live, exchanges) = refresh_between_turns("acc-other").await;
    assert_cache_chain(&exchanges, &[2]);
    let requests = ordinary(&exchanges);
    assert!(reasoning_ids(requests[2]).is_empty());
    assert!(super::input(requests[2]).iter().any(is_call("call-0")));
    assert_eq!(reasoning_ids(requests[3]), ["rs_3"]);
}

// Given: a Codex chat thread whose completed turn carries R1 (tool round) and R2
// When: another thread forks it at that turn, runs a turn with R3/R4, compacts
// officially and follows up (R5)
// Then: the fork never replays the source's R1/R2, not even to the official
// compaction, which carries the fork's own R3/R4; after the boundary the fork reuses
// its new prefix, and the source thread is unchanged
#[tokio::test]
async fn fork_never_revives_source_reasoning_even_for_official_compaction() {
    let items: Vec<_> = (1..=5).map(|n| reasoning(&format!("rs_{n}"))).collect();
    let known: Vec<_> = items.iter().collect();
    let mut live = Live::new(
        vec![
            read_after(MODEL_A, 0, &items[0]),
            text_after(MODEL_A, "source answer", &items[1]),
            read_after(MODEL_A, 3, &items[2]),
            text_after(MODEL_A, "fork answer", &items[3]),
            ScriptedResponse::compaction("compaction", MODEL_A, "blob-in-fork"),
            text_after(MODEL_A, "fork done", &items[4]),
        ],
        FAR_FUTURE_EXP,
        true,
    );
    let config = RunConfig {
        interactive: true,
        keep_alive: true,
        ..Default::default()
    };
    let source = live
        .runtime
        .delegate_chat("source", Role::Worker, "Start".into(), config.clone())
        .unwrap();
    let mut source_events = Vec::new();
    let context_len = loop {
        let event = live.receiver.recv().await.unwrap();
        let completed = match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::TurnCompleted {
                run_id,
                context_len,
            }) if run_id == &source.to_string() => Some(*context_len),
            _ => None,
        };
        source_events.push(event);
        if let Some(context_len) = completed {
            break context_len;
        }
    };
    let fork = live
        .runtime
        .delegate_chat_seeded(
            "fork",
            Role::Worker,
            "Branch".into(),
            config,
            Some(runtime::ChatForkSeed {
                source_run_id: source.to_string(),
                context_len,
            }),
        )
        .unwrap();
    let mut events = Vec::new();
    live.turn(fork, &mut events, None).await;
    live.runtime.compact(fork).unwrap();
    live.turn(fork, &mut events, Some("After compaction in the fork"))
        .await;
    live.finish(&[source, fork]).await;
    source_events.extend(events.iter().cloned());

    let all = live.requests();
    assert_eq!(all.len(), 6);
    for body in &all {
        assert_sealed(body, &known);
    }
    let fork_exchanges = exchanges(all[2..].to_vec(), fork, &source_events);
    assert_cache_chain(&fork_exchanges, &[]);
    assert_eq!(reasoning_ids(&all[1]), ["rs_1"]);
    let requests = ordinary(&fork_exchanges);
    assert!(
        reasoning_ids(requests[0]).is_empty(),
        "fork revived the source"
    );
    assert!(super::input(requests[0]).iter().any(is_call("call-0")));
    assert!(
        super::input(requests[0])
            .iter()
            .any(is_assistant_text("source answer"))
    );
    assert_eq!(reasoning_ids(requests[1]), ["rs_3"]);
    let compaction = the_compaction(&fork_exchanges);
    assert_eq!(reasoning_ids(compaction), ["rs_3", "rs_4"]);
    assert_eq!(compaction_blob(requests[2]), Some("blob-in-fork"));
    assert!(
        !reasoning_ids(requests[2])
            .iter()
            .any(|id| id == "rs_1" || id == "rs_2")
    );
}
