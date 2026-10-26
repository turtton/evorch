//! B-3: confirmed Responses `reasoning` items (encrypted reasoning) are replayed verbatim
//! inside the reused prefix, and dropped exactly at scope boundaries.
//!
//! Every scenario checks two independent observations: the mock's input-derived
//! prompt cache (reported usage) and the recorded wire JSON (append-only prefix plus
//! explicit sentinel id / ciphertext / summary / order checks). The mock streams a
//! provisional `added` ciphertext that differs from the confirmed `done` item, so a
//! replay of the wrong value is visible on the wire.

mod codex;
mod helper_contract;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;

use config::{Config, LoadOptions};
use event_bus::{
    AgentRunPhase, CompactionEvent, CompactionReason, Event, EventBus, EventKind, ProviderEvent,
    RequestPurpose,
};
use mock_openai::cache_contract::{
    CacheProtocol, assert_append_only, assert_compaction_transition,
};
use mock_openai::{ReasoningItem, ScriptedResponse, StreamingMockOpenAi};
use routing::MapEnv;
use runtime::{ModelSource, Role, RunConfig, RunId, RuntimeComposition, compose_runtime};
use sandbox::credential::FileCredentialStore;
use serde_json::{Value, json};
use tools::ToolExecutor;

use super::{
    BulkRead, Harness, KEY_ENV, MODEL, Wire, contract_prompts, harness_with_wire, through_phase,
    verify_trace,
};

/// Ciphertexts are `enc:{id}:final`; the provisional `added` value is `enc:{id}:partial`.
/// No other wire text contains `enc:`.
fn reasoning(id: &str) -> ReasoningItem {
    ReasoningItem::new(0, id, &format!("enc:{id}:final"))
        .with_added_encrypted_content(&format!("enc:{id}:partial"))
        .with_summary([format!("Summary of {id}")])
}

/// Adds `item` to `script` (output index 0 before the script's own item).
fn reasoned(script: ScriptedResponse, item: &ReasoningItem) -> ScriptedResponse {
    script.with_reasoning(item.clone())
}

/// A `read` tool call at output index 1 after `item` at index 0.
fn read_after(model: &str, index: usize, item: &ReasoningItem) -> ScriptedResponse {
    reasoned(
        ScriptedResponse::tool_call(
            &format!("response-{index}"),
            model,
            1,
            &format!("call-{index}"),
            "read",
            [json!({"index": index}).to_string()],
        ),
        item,
    )
}

/// Assistant text at output index 1 after `item` at index 0.
fn text_after(model: &str, text: &str, item: &ReasoningItem) -> ScriptedResponse {
    reasoned(
        ScriptedResponse::text_stream("reply", model, [text]).with_text_output_index(1),
        item,
    )
}

fn input(body: &Value) -> &[Value] {
    body["input"].as_array().unwrap()
}

fn is_compaction_request(body: &Value) -> bool {
    input(body)
        .last()
        .is_some_and(|item| item["type"] == "compaction_trigger")
}

/// Reasoning ids in wire order.
fn reasoning_ids(body: &Value) -> Vec<String> {
    input(body)
        .iter()
        .filter(|item| item["type"] == "reasoning")
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect()
}

fn without_reasoning(body: &Value) -> Value {
    let mut body = body.clone();
    body["input"]
        .as_array_mut()
        .unwrap()
        .retain(|item| item["type"] != "reasoning");
    body
}

/// Every replayed reasoning item is a confirmed done item, compared as JSON values,
/// and no ciphertext (final or provisional) appears anywhere else in the request.
fn assert_sealed(body: &Value, known: &[&ReasoningItem]) {
    for item in input(body)
        .iter()
        .filter(|item| item["type"] == "reasoning")
    {
        assert!(
            known.iter().any(|known| &known.done_item() == item),
            "replayed reasoning is not a confirmed done item: {item}"
        );
    }
    let rest = without_reasoning(body).to_string();
    assert!(
        !rest.contains("enc:"),
        "ciphertext outside a reasoning item: {rest}"
    );
}

/// `item` is replayed immediately before the input item matching `next`.
fn assert_precedes(body: &Value, item: &ReasoningItem, next: impl Fn(&Value) -> bool) {
    let items = input(body);
    let done = item.done_item();
    let at = items
        .iter()
        .position(|candidate| candidate == &done)
        .unwrap_or_else(|| panic!("{} is not replayed: {body}", done["id"]));
    assert!(
        items.get(at + 1).is_some_and(&next),
        "unexpected item after {}: {:?}",
        done["id"],
        items.get(at + 1)
    );
}

fn is_call(id: &str) -> impl Fn(&Value) -> bool + '_ {
    move |item| item["type"] == "function_call" && item["call_id"] == id
}

fn is_assistant_text(text: &str) -> impl Fn(&Value) -> bool + '_ {
    move |item| item["role"] == "assistant" && item["content"].to_string().contains(text)
}

/// Ordinary Responses requests always ask for encrypted reasoning, unchanged.
fn assert_include(body: &Value) {
    assert_eq!(
        body["include"],
        json!(["reasoning.encrypted_content"]),
        "{body}"
    );
}

/// One recorded wire request with the usage the mock reported for it.
struct Exchange {
    body: Value,
    /// `(input_tokens, cache_read_tokens)` of the completed request; `None` only for a
    /// request the scenario declared as failing (a fallback attempt).
    usage: Option<(u64, u64)>,
    /// Official compaction or a model text-summary request, identified by purpose.
    compaction: bool,
    /// The verified boundary consumed by this request, if any.
    checkpoint: Option<(CompactionReason, String)>,
}

/// The POST bodies `mock` recorded on `path`, in arrival order.
fn posted(mock: &StreamingMockOpenAi, path: &str) -> Vec<Value> {
    mock.recorded_requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == path)
        .map(|request| request.body)
        .collect()
}

/// [`exchanges_with_failures`] for a trace in which every request succeeds.
fn exchanges(bodies: Vec<Value>, run: RunId, events: &[Event]) -> Vec<Exchange> {
    exchanges_with_failures(bodies, run, events, &[])
}

/// Pairs `run`'s provider requests with `bodies`, that run's recorded wire requests.
/// A run's requests are sequential, so both sides arrive in the same order.
///
/// Every request must have completed with usage from the input-derived cache:
/// positive input and no more cached than input. Only the request positions listed
/// in `failed` must instead have a failure event and no completion.
fn exchanges_with_failures(
    bodies: Vec<Value>,
    run: RunId,
    events: &[Event],
    failed: &[usize],
) -> Vec<Exchange> {
    let run = run.to_string();
    let mut started = Vec::new();
    let mut completed = HashMap::new();
    let mut failures = HashSet::new();
    let mut pending = None;
    for event in events {
        match &event.kind {
            EventKind::Compaction(CompactionEvent::Compacted {
                run_id,
                reason,
                checkpoint_id,
                ..
            }) if run_id == &run => {
                // One switch may prepare a text re-summary and then a window
                // compaction; any other replacement needs its own request.
                assert!(
                    pending.as_ref().is_none_or(|(previous, _)| {
                        *previous == CompactionReason::ModelSwitch
                            && *reason == CompactionReason::ModelSwitch
                    }),
                    "two boundaries without a request between them"
                );
                pending = Some((*reason, checkpoint_id.clone()));
            }
            EventKind::Provider(ProviderEvent::RequestStarted {
                request_id,
                run_id: Some(id),
                purpose,
                ..
            }) if id == &run => {
                let compaction = *purpose == Some(RequestPurpose::Compaction);
                let checkpoint = if compaction { None } else { pending.take() };
                started.push((request_id.clone(), compaction, checkpoint));
            }
            EventKind::Provider(ProviderEvent::RequestCompleted {
                request_id,
                input_tokens,
                cache_read_tokens,
                run_id: Some(id),
                ..
            }) if id == &run => {
                assert!(
                    completed
                        .insert(request_id.clone(), (*input_tokens, *cache_read_tokens))
                        .is_none(),
                    "request {request_id} completed twice"
                );
            }
            EventKind::Provider(ProviderEvent::RequestFailed {
                request_id,
                run_id: Some(id),
                ..
            }) if id == &run => {
                assert!(
                    failures.insert(request_id.clone()),
                    "request {request_id} failed twice"
                );
            }
            EventKind::Diagnostic(diagnostic) => {
                assert_ne!(diagnostic.code, "CacheRegression", "{diagnostic:?}");
            }
            _ => {}
        }
    }
    assert!(
        pending.is_none(),
        "a boundary must be followed by a request"
    );
    let mut bodies = VecDeque::from(bodies);
    assert_eq!(
        bodies.len(),
        started.len(),
        "one wire request per provider request"
    );
    started
        .into_iter()
        .enumerate()
        .map(|(position, (request_id, compaction, checkpoint))| {
            let body = bodies.pop_front().unwrap();
            assert!(
                !is_compaction_request(&body) || compaction,
                "official compaction must have compaction purpose: {body}"
            );
            let usage = completed.get(&request_id).copied();
            if failed.contains(&position) {
                assert!(usage.is_none(), "request {position} was expected to fail");
                assert!(
                    failures.contains(&request_id),
                    "request {position} has no failure event"
                );
            } else {
                assert!(
                    !failures.contains(&request_id),
                    "request {position} failed without an explicit exception"
                );
                let (input, cached) =
                    usage.unwrap_or_else(|| panic!("request {position} has no completed usage"));
                assert!(input > 0, "request {position} reported no input tokens");
                assert!(
                    cached <= input,
                    "request {position}: cached={cached} exceeds input={input}"
                );
            }
            Exchange {
                body,
                usage,
                compaction,
                checkpoint,
            }
        })
        .collect()
}

/// Root fields other than `input` that differ, for assertion messages.
fn root_diff(previous: &Value, current: &Value) -> String {
    let empty = serde_json::Map::new();
    let (previous, current) = (
        previous.as_object().unwrap_or(&empty),
        current.as_object().unwrap_or(&empty),
    );
    let mut keys: Vec<_> = previous.keys().chain(current.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|key| *key != "input" && previous.get(*key) != current.get(*key))
        .map(|key| format!("{key}: {:?} -> {:?}", previous.get(key), current.get(key)))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The cache contract over consecutive requests, with both observations.
///
/// `boundaries` lists ordinary-request positions (0-based, official compaction
/// requests and declared failures not counted) that start a new replay scope: a
/// model, profile or account switch. There no reasoning item sent before the boundary
/// may return, and the rest of the history is either kept (append-only over the
/// previous request without its reasoning, with the new model) or replaced at one
/// verified ModelSwitch compaction. Every other request must be append-only and the
/// mock cache must report at least the previous request's whole input as cached,
/// except at a verified compaction boundary, which must actually replace history
/// (less cached than the previous input). Official compaction requests are their own
/// contract (`include: []`, own instructions) and are not prefix steps; a declared
/// failed attempt must extend the previous request.
fn assert_cache_chain(exchanges: &[Exchange], boundaries: &[usize]) {
    let mut previous: Option<&Exchange> = None;
    let mut ordinary = 0;
    let mut seen = std::collections::HashSet::new();
    for exchange in exchanges {
        if exchange.compaction {
            if is_compaction_request(&exchange.body) {
                assert_eq!(
                    input(&exchange.body).last(),
                    Some(&json!({"type": "compaction_trigger"}))
                );
                assert_eq!(exchange.body["include"], json!([]));
            } else {
                // Text summaries use the regular Responses API, but their own
                // system prompt/history and are not ordinary prefix steps.
                assert_include(&exchange.body);
            }
            continue;
        }
        assert_include(&exchange.body);
        let Some(before) = previous else {
            assert!(exchange.usage.is_some(), "the first request must complete");
            seen.extend(reasoning_ids(&exchange.body));
            previous = Some(exchange);
            ordinary += 1;
            continue;
        };
        let Some((input_tokens, cached)) = exchange.usage else {
            assert_append_only(CacheProtocol::Codex, &before.body, &exchange.body)
                .unwrap_or_else(|error| panic!("failed attempt after {ordinary}: {error}"));
            continue;
        };
        let (previous_input, _) = before.usage.expect("previous request completed");
        let boundary = boundaries.contains(&ordinary);
        let mut baseline = before.body.clone();
        if boundary {
            let new = reasoning_ids(&exchange.body);
            assert!(
                new.iter().all(|id| !seen.contains(id)),
                "request {ordinary} revived reasoning sent before the boundary: {new:?}"
            );
            baseline = without_reasoning(&baseline);
            baseline["model"] = exchange.body["model"].clone();
        }
        match &exchange.checkpoint {
            Some((reason, checkpoint)) => {
                assert!(
                    !boundary || *reason == CompactionReason::ModelSwitch,
                    "{reason:?}"
                );
                assert_compaction_transition(
                    CacheProtocol::Codex,
                    &baseline,
                    &exchange.body,
                    checkpoint,
                )
                .unwrap_or_else(|error| {
                    panic!(
                        "request {ordinary} boundary: {error}; {}",
                        root_diff(&baseline, &exchange.body)
                    )
                });
                assert!(
                    cached < previous_input,
                    "request {ordinary}: a replacement must invalidate part of the old prefix"
                );
            }
            None => {
                assert_append_only(CacheProtocol::Codex, &baseline, &exchange.body).unwrap_or_else(
                    |error| {
                        panic!(
                            "request {ordinary}: {error}; {}",
                            root_diff(&baseline, &exchange.body)
                        )
                    },
                );
                assert!(
                    boundary || cached >= previous_input,
                    "request {ordinary}: cached={cached}, previous input={previous_input}"
                );
            }
        }
        assert!(cached <= input_tokens);
        previous = Some(exchange);
        seen.extend(reasoning_ids(&exchange.body));
        ordinary += 1;
    }
    assert!(ordinary >= 2, "a cache chain needs at least two requests");
}

fn wire_bodies(harness: &Harness) -> Vec<Value> {
    posted(&harness.mock, harness.wire.path())
}

// Given: a Responses wire whose first reply is reasoning R1 then a read call, whose
// second is R2 then text, and a follow-up reply with R3
// When: the tool round, the answer and a follow-up run on one worker
// Then: request 2 replays the done R1 (never the provisional value) right before the
// call, request 3 keeps R1 and adds R2 before its answer, every request is append-only
// with a fixed include, and the input-derived cache reports each previous input cached
async fn tool_round_replays_done_reasoning(wire: Wire) {
    let review_storage = crate::support::ReviewStorage::new();
    let (r1, r2, r3) = (reasoning("rs_1"), reasoning("rs_2"), reasoning("rs_3"));
    let mut harness = harness_with_wire(
        wire,
        vec![
            read_after(MODEL, 0, &r1),
            text_after(MODEL, "answered", &r2),
            text_after(MODEL, "done", &r3),
        ],
        1_000_000,
        Arc::new(BulkRead),
        Vec::new(),
        None,
        0.5,
        |_| {},
    );
    let run = harness.runtime.delegate_background(
        Role::Worker,
        "Read with reasoning".into(),
        RunConfig {
            interactive: true,
            ..Default::default()
        },
    );
    let mut events = through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await;
    review_storage.attach(&harness.runtime);
    harness
        .runtime
        .send_message(run, "Follow up".into())
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await);
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    verify_trace(&harness, run, &events, 0);

    let bodies = wire_bodies(&harness);
    assert_eq!(bodies.len(), 3);
    assert_cache_chain(&exchanges(bodies.clone(), run, &events), &[]);
    for body in &bodies {
        assert_include(body);
        assert_sealed(body, &[&r1, &r2, &r3]);
    }
    assert!(reasoning_ids(&bodies[0]).is_empty());
    assert_eq!(reasoning_ids(&bodies[1]), ["rs_1"]);
    assert_precedes(&bodies[1], &r1, is_call("call-0"));
    assert_eq!(reasoning_ids(&bodies[2]), ["rs_1", "rs_2"]);
    assert_precedes(&bodies[2], &r1, is_call("call-0"));
    assert_precedes(&bodies[2], &r2, is_assistant_text("answered"));
}

#[tokio::test]
async fn openai_responses_tool_round_replays_done_reasoning_inside_the_cached_prefix() {
    tool_round_replays_done_reasoning(Wire::Responses).await;
}

#[tokio::test]
async fn codex_tool_round_replays_done_reasoning_inside_the_cached_prefix() {
    tool_round_replays_done_reasoning(Wire::Codex).await;
}

/// Rebuilds the Responses harness's runtime from its saved configuration and storage,
/// as a new process would.
fn restart_responses(
    harness: &mut Harness,
    storage: &storage::Storage,
    db: &storage::StorageConfig,
) {
    let root = harness._directory.path();
    let current = Config::load(&LoadOptions {
        project_dir: Some(root.into()),
        user_config_dir: Some(root.join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(1024));
    harness.receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(Arc::new(BulkRead)).unwrap();
    harness.runtime = compose_runtime(RuntimeComposition {
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(root.join("empty-user-config")),
        config: &current,
        bus,
        executor: Arc::new(executor),
        credential_store: Arc::new(FileCredentialStore::open(root.join("credentials")).unwrap()),
        env: Arc::new(MapEnv::from_iter([(KEY_ENV, "offline-test-key")])),
        model_source: ModelSource::Configured,
        workspace: None,
    })
    .unwrap()
    .runtime
    .with_system_prompts(contract_prompts())
    .with_compaction(current.compaction.clone())
    .with_run_store(runtime::RunStore::open(db, storage.handle()).unwrap());
    assert!(harness.runtime.list_agents().is_empty());
}

// Given: a Responses worker that completed a tool round with R1 and an answer with R2,
// saved by the run store
// When: the process is recreated from the saved snapshot and the chat continues
// Then: the first request after restart replays the same R1 and R2 at the same
// positions, extends the last request before restart, and reuses its cached prefix;
// the next request keeps the new R3
#[tokio::test]
async fn restart_restores_the_same_reasoning_then_reuses_the_prefix() {
    let (r1, r2, r3, r4) = (
        reasoning("rs_1"),
        reasoning("rs_2"),
        reasoning("rs_3"),
        reasoning("rs_4"),
    );
    let mut harness = harness_with_wire(
        Wire::Responses,
        vec![
            read_after(MODEL, 0, &r1),
            text_after(MODEL, "answered", &r2),
            read_after(MODEL, 3, &r3),
            text_after(MODEL, "done", &r4),
        ],
        1_000_000,
        Arc::new(BulkRead),
        Vec::new(),
        None,
        0.5,
        |_| {},
    );
    let db = storage::StorageConfig {
        db_path: harness._directory.path().join("history.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(db.clone()).unwrap();
    harness.runtime = harness
        .runtime
        .with_run_store(runtime::RunStore::open(&db, storage.handle()).unwrap());
    let run = harness.runtime.delegate_background(
        Role::Worker,
        "Read with reasoning".into(),
        RunConfig {
            interactive: true,
            ..Default::default()
        },
    );
    let mut events = through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await;
    harness
        .runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Stopped).await);
    harness.runtime.wait(run).await.unwrap();

    restart_responses(&mut harness, &storage, &db);
    harness
        .runtime
        .resume_chat(run, RunConfig::default())
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);
    harness
        .runtime
        .send_message(run, "Continue after restart".into())
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);
    harness.runtime.cancel(run).unwrap();
    harness.runtime.wait(run).await.unwrap();

    let bodies = wire_bodies(&harness);
    assert_eq!(bodies.len(), 4, "the completed turn is not re-requested");
    assert_cache_chain(&exchanges(bodies.clone(), run, &events), &[]);
    for body in &bodies {
        assert_sealed(body, &[&r1, &r2, &r3, &r4]);
    }
    assert_eq!(reasoning_ids(&bodies[2]), ["rs_1", "rs_2"]);
    assert_precedes(&bodies[2], &r1, is_call("call-0"));
    assert_precedes(&bodies[2], &r2, is_assistant_text("answered"));
    assert_eq!(reasoning_ids(&bodies[3]), ["rs_1", "rs_2", "rs_3"]);
    assert_precedes(&bodies[3], &r3, is_call("call-3"));
}

// Given: a Responses worker with R1 (tool round) and R2 (long answer)
// When: a manual or automatic compaction replaces history before the follow-up, whose
// replies carry R3 (tool call) and R4
// Then: the runtime summary never contains ciphertext, the boundary is the one verified
// replacement, R3 is replayed before its call in the next request, and that request
// reuses the post-boundary prefix in the input-derived cache
async fn compaction_reuses_new_reasoning_after_the_boundary(reason: CompactionReason) {
    let review_storage = crate::support::ReviewStorage::new();
    let automatic = reason == CompactionReason::Automatic;
    let (r1, r2, r3, r4) = (
        reasoning("rs_1"),
        reasoning("rs_2"),
        reasoning("rs_3"),
        reasoning("rs_4"),
    );
    let old_reply =
        "old analysis that can be summarized ".repeat(if automatic { 6000 } else { 100 });
    let mut harness = harness_with_wire(
        Wire::Responses,
        vec![
            read_after(MODEL, 0, &r1),
            text_after(MODEL, &old_reply, &r2),
            read_after(MODEL, 3, &r3),
            text_after(MODEL, "done", &r4),
        ],
        if automatic { 64_000 } else { 1_000_000 },
        Arc::new(BulkRead),
        Vec::new(),
        None,
        0.5,
        |_| {},
    );
    let run = harness.runtime.delegate_background(
        Role::Worker,
        "Preserve this original goal".into(),
        RunConfig {
            interactive: true,
            ..Default::default()
        },
    );
    let mut events = through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await;
    if !automatic {
        harness.runtime.compact(run).unwrap();
    }
    review_storage.attach(&harness.runtime);
    harness
        .runtime
        .send_message(run, "Continue after this boundary".into())
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await);
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    assert!(events.iter().any(|event| matches!(&event.kind,
        EventKind::Compaction(CompactionEvent::Compacted { reason: actual, .. }) if *actual == reason)));
    verify_trace(&harness, run, &events, 1);

    let bodies = wire_bodies(&harness);
    assert_eq!(bodies.len(), 4);
    assert_cache_chain(&exchanges(bodies.clone(), run, &events), &[]);
    for body in &bodies {
        assert_include(body);
        assert_sealed(body, &[&r1, &r2, &r3, &r4]);
    }
    assert_precedes(&bodies[1], &r1, is_call("call-0"));
    assert_precedes(&bodies[3], &r3, is_call("call-3"));
    let boundary_ids = reasoning_ids(&bodies[2]);
    let mut next_ids = reasoning_ids(&bodies[3]);
    assert_eq!(next_ids.pop().as_deref(), Some("rs_3"));
    assert_eq!(
        next_ids, boundary_ids,
        "the boundary's reasoning stays fixed"
    );
}

#[tokio::test]
async fn manual_compaction_reuses_new_reasoning_after_the_boundary() {
    compaction_reuses_new_reasoning_after_the_boundary(CompactionReason::Manual).await;
}

#[tokio::test]
async fn automatic_compaction_reuses_new_reasoning_after_the_boundary() {
    compaction_reuses_new_reasoning_after_the_boundary(CompactionReason::Automatic).await;
}

/// One worker route: a Codex (`kind` "openai-codex") or official OpenAI Responses
/// (`kind` "openai") profile serving `model`, optionally with an editing override.
#[derive(Clone, Copy)]
struct Route<'a> {
    kind: &'a str,
    profile: &'a str,
    model: &'a str,
    edit_tool: Option<&'a str>,
}

impl<'a> Route<'a> {
    const fn codex(profile: &'a str, model: &'a str) -> Self {
        Self {
            kind: "openai-codex",
            profile,
            model,
            edit_tool: None,
        }
    }
}

/// Writes and loads a one-route configuration on `base_url`. `overrides` are the
/// compaction window overrides keyed by `profile/model`.
fn route_config(
    root: &Path,
    route: Route<'_>,
    base_url: &str,
    overrides: &[(String, u64)],
    max_summary_bytes: u64,
) -> Config {
    let Route {
        kind,
        profile,
        model,
        edit_tool,
    } = route;
    let credential = if kind == "openai-codex" {
        format!(
            "api_protocol = \"openai-codex-responses\"\ncredential = {{ type = \"keyring\", service = \"evorch\", account = \"{}\" }}",
            super::CODEX_ACCOUNT
        )
    } else {
        format!("api_key_env = \"{KEY_ENV}\"")
    };
    let edit = edit_tool.map_or_else(String::new, |tool| format!(", edit_tool = \"{tool}\""));
    let overrides: String = overrides
        .iter()
        .map(|(key, window)| format!("\"{key}\" = {window}\n"))
        .collect();
    std::fs::create_dir_all(root.join(config::PROJECT_CONFIG_DIR)).unwrap();
    std::fs::write(
        config::project_main_config_path(root),
        format!(
            r#"
version = 3
[providers.{profile}]
type = "{kind}"
base_url = "{base_url}"
{credential}
models = [{{ id = "{model}", enabled = true{edit} }}]
default_model = "{model}"
[[routing.routes.worker]]
profile = "{profile}"
[agents.worker]
models = [{{ route = "worker" }}]
[compaction]
context_window_tokens = 1000000
threshold = 0.5
keep_recent_tokens = 1
max_summary_bytes = {max_summary_bytes}
summarizer = "structural"
[compaction.model_overrides]
{overrides}"#,
        ),
    )
    .unwrap();
    Config::load(&LoadOptions {
        project_dir: Some(root.into()),
        user_config_dir: Some(root.join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    })
    .unwrap()
}
