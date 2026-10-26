//! §6.3 composite Codex scenarios: a rolled-back candidate preparation, protected D3
//! summaries across stacked compactions and a non-replaying switch, a clipped snapshot
//! restored onto another scope, and token refreshes across official compaction.

use std::sync::Arc;

use event_bus::{AgentRunPhase, CompactionEvent, CompactionReason, EventKind};
use mock_openai::ScriptedResponse;
use mock_openai::codex_auth::{FAR_FUTURE_EXP, token_bundle};
use runtime::RunConfig;
use serde_json::Value;

use super::super::{
    assert_cache_chain, assert_precedes, assert_sealed, exchanges, input, is_assistant_text,
    is_call, posted, read_after, reasoning, reasoning_ids, text_after,
};
use super::{
    ACCOUNT, Live, MODEL_A, MODEL_B, Options, compaction_blob, expiring, ordinary, the_compaction,
};
use crate::{Wire, through_phase};

fn checkpoints(events: &[event_bus::Event]) -> Vec<(CompactionReason, String)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Compaction(CompactionEvent::Compacted {
                reason,
                checkpoint_id,
                ..
            }) => Some((*reason, checkpoint_id.clone())),
            _ => None,
        })
        .collect()
}

fn compaction_requests(bodies: &[Value]) -> Vec<&Value> {
    bodies
        .iter()
        .filter(|body| super::super::is_compaction_request(body))
        .collect()
}

// Given: a Codex worker whose model A history holds R1/R2 and a long answer, reloaded to
// model B whose 40K window forces a candidate-scope official compaction, with the
// snapshot that would bind B rejected by the store
// When: the follow-up prepares B (the compaction request is sent, then the save
// fails), the store recovers, and the chat resumes
// Then: nothing of the failed candidate is committed (no B binding, blob or boundary on
// disk or in events); the retried candidate sends the same compaction input, still
// without A's reasoning; B's first request carries only the accepted blob, B's R3 is
// replayed next, and the new prefix is reused with positive cache usage
#[tokio::test]
async fn rejected_candidate_snapshot_rolls_back_then_reprepares_safely() {
    let (r1, r2, r3, r4) = (
        reasoning("rs_1"),
        reasoning("rs_2"),
        reasoning("rs_3"),
        reasoning("rs_4"),
    );
    let old_reply = "old analysis that can be summarized ".repeat(3_000);
    let mut live = Live::with(
        vec![
            read_after(MODEL_A, 0, &r1),
            text_after(MODEL_A, &old_reply, &r2),
            ScriptedResponse::compaction("rejected", MODEL_B, "blob-rejected"),
            ScriptedResponse::compaction("accepted", MODEL_B, "blob-accepted"),
            read_after(MODEL_B, 3, &r3),
            text_after(MODEL_B, "done on B", &r4),
        ],
        FAR_FUTURE_EXP,
        true,
        Options {
            overrides: vec![(format!("work/{MODEL_B}"), 40_000)],
            ..Options::default()
        },
    );
    let run = live.worker("Read with reasoning");
    let mut events = Vec::new();
    live.turn(run, &mut events, None).await;
    let original = storage::Database::open(&live.db)
        .unwrap()
        .run_context(&run.to_string())
        .unwrap()
        .unwrap();
    let original_config: Value = serde_json::from_str(&original.config_json).unwrap();
    live.reconfigure("work", MODEL_B);
    let store = rusqlite::Connection::open(&live.db.db_path).unwrap();
    store
        .execute_batch(
            r#"CREATE TRIGGER reject_candidate BEFORE UPDATE ON run_contexts
               WHEN NEW.config_json LIKE '%"wire_model":"mock-model-b"%'
               BEGIN SELECT RAISE(ABORT, 'candidate snapshot rejected'); END;"#,
        )
        .unwrap();
    live.runtime
        .send_message(run, "Continue on the smaller model".into())
        .unwrap();
    events.extend(through_phase(&mut live.receiver, run, AgentRunPhase::Error).await);
    live.runtime.wait(run).await.unwrap();
    let saved = storage::Database::open(&live.db)
        .unwrap()
        .run_context(&run.to_string())
        .unwrap()
        .unwrap();
    assert!(!saved.config_json.contains(r#""wire_model":"mock-model-b""#));
    assert!(saved.config_json.contains(r#""wire_model":"mock-model""#));
    assert!(!saved.messages_json.contains("blob-rejected"));
    let rejected_config: Value = serde_json::from_str(&saved.config_json).unwrap();
    assert_eq!(
        rejected_config["responses_replay"], original_config["responses_replay"],
        "failed preparation must not commit the candidate's floor or binding"
    );
    assert!(
        checkpoints(&events).is_empty(),
        "the failed candidate emitted a boundary"
    );
    store
        .execute_batch("DROP TRIGGER reject_candidate;")
        .unwrap();
    live.runtime.resume_chat(run, RunConfig::default()).unwrap();
    events.extend(through_phase(&mut live.receiver, run, AgentRunPhase::Waiting).await);
    live.finish(&[run]).await;

    let bodies = live.requests();
    assert_eq!(bodies.len(), 6);
    for body in &bodies {
        assert_sealed(body, &[&r1, &r2, &r3, &r4]);
    }
    let exchanges = exchanges(bodies.clone(), run, &events);
    assert_cache_chain(&exchanges, &[2]);
    let candidates = compaction_requests(&bodies);
    assert_eq!(candidates.len(), 2);
    for candidate in &candidates {
        assert_eq!(candidate["model"], MODEL_B);
        assert!(
            reasoning_ids(candidate).is_empty(),
            "A's reasoning sent to B"
        );
    }
    assert_eq!(
        candidates[0]["input"], candidates[1]["input"],
        "the retry repeats the same safe preparation"
    );
    let reasons: Vec<_> = checkpoints(&events).into_iter().map(|(r, _)| r).collect();
    assert_eq!(reasons, [CompactionReason::ModelSwitch]);
    let requests = ordinary(&exchanges);
    assert_eq!(compaction_blob(requests[2]), Some("blob-accepted"));
    assert!(!requests[2].to_string().contains("blob-rejected"));
    assert!(reasoning_ids(requests[2]).is_empty());
    assert_eq!(reasoning_ids(requests[3]), ["rs_3"]);
    assert_precedes(requests[3], &r3, is_call("call-3"));
}

// Given: a Codex worker whose turn on model A ends with R1/R2, reloaded to model B
// whose first reply (R3) starts a read that is still running
// When: the process is recreated from the store as it was during that read (the
// snapshot clips the outstanding batch), back on model A, and the chat resumes,
// compacts officially and continues
// Then: the clipped snapshot keeps the B binding; the resumed A request replays none of
// R1/R2 (old A, before the B boundary) nor the clipped R3 or its call; the official
// compaction receives only A's new R4; after the blob A's R5 is reused within the new
// prefix, all with positive cache usage
#[tokio::test]
async fn clipped_snapshot_restore_and_official_compaction_never_revive_old_reasoning() {
    let items: Vec<_> = (1..=6).map(|n| reasoning(&format!("rs_{n}"))).collect();
    let known: Vec<_> = items.iter().collect();
    let read = Arc::new(crate::GatedRead {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        child_started: tokio::sync::Notify::new(),
    });
    let mut live = Live::with(
        vec![
            // GatedRead blocks on index 0 and 1; index 4 reads immediately.
            read_after(MODEL_A, 4, &items[0]),
            text_after(MODEL_A, "on A", &items[1]),
            read_after(MODEL_B, 1, &items[2]),
            text_after(MODEL_A, "resumed on A", &items[3]),
            ScriptedResponse::compaction("compaction", MODEL_A, "blob-after-restore"),
            read_after(MODEL_A, 3, &items[4]),
            text_after(MODEL_A, "done", &items[5]),
        ],
        FAR_FUTURE_EXP,
        true,
        Options {
            read: Some(read.clone()),
            ..Options::default()
        },
    );
    let run = live.worker("Read with reasoning");
    let mut events = Vec::new();
    live.turn(run, &mut events, None).await;
    live.reconfigure("work", MODEL_B);
    live.runtime
        .send_message(run, "Continue on B".into())
        .unwrap();
    // Collect the old process's events through B's completed reply.
    loop {
        let event = live.receiver.recv().await.unwrap();
        let replied = matches!(&event.kind,
            EventKind::Provider(event_bus::ProviderEvent::RequestCompleted { run_id: Some(id), model, .. })
                if *id == run.to_string() && model == MODEL_B);
        events.push(event);
        if replied {
            break;
        }
    }
    read.child_started.notified().await;
    let copy = storage::StorageConfig {
        db_path: live.directory.path().join("crash-copy.db"),
        ..Default::default()
    };
    rusqlite::Connection::open(&live.db.db_path)
        .unwrap()
        .execute("VACUUM INTO ?1", [copy.db_path.to_str().unwrap()])
        .unwrap();
    let crashed = live.restart(copy.clone(), super::Route::codex("work", MODEL_A));
    let saved = storage::Database::open(&copy)
        .unwrap()
        .run_context(&run.to_string())
        .unwrap()
        .unwrap();
    assert!(saved.messages_json.contains("Continue on B"));
    assert!(
        !saved.messages_json.contains("call-1"),
        "the outstanding batch is clipped"
    );
    assert!(!saved.messages_json.contains("enc:rs_3"));
    assert!(saved.config_json.contains(r#""wire_model":"mock-model-b""#));
    let saved_messages: Vec<providers::Message> =
        serde_json::from_str(&saved.messages_json).unwrap();
    let saved_config: Value = serde_json::from_str(&saved.config_json).unwrap();
    let replay: runtime::ResponsesReplayState =
        serde_json::from_value(saved_config["responses_replay"].clone()).unwrap();
    assert_eq!(replay.floor, saved_messages.len(), "safe-end bound");
    assert_eq!(replay.scope().unwrap().wire_model, MODEL_B);
    assert!(matches!(
        replay.binding,
        runtime::ResponsesReplayBinding::Bound { .. }
    ));
    live.runtime.resume_chat(run, RunConfig::default()).unwrap();
    events.extend(through_phase(&mut live.receiver, run, AgentRunPhase::Waiting).await);
    live.runtime.compact(run).unwrap();
    live.turn(run, &mut events, Some("After compaction")).await;
    live.finish(&[run]).await;
    crashed.cancel(run).unwrap();
    crashed.wait(run).await.unwrap();

    let bodies = live.requests();
    for body in &bodies {
        assert_sealed(body, &known);
    }
    let exchanges = exchanges(bodies.clone(), run, &events);
    assert_cache_chain(&exchanges, &[2, 3]);
    let requests = ordinary(&exchanges);
    assert_eq!(requests.len(), 6);
    assert_eq!(reasoning_ids(requests[1]), ["rs_1"]);
    assert!(reasoning_ids(requests[2]).is_empty(), "A's items on B");
    assert!(
        reasoning_ids(requests[3]).is_empty(),
        "old or clipped items back on A"
    );
    assert!(!input(requests[3]).iter().any(is_call("call-1")));
    let compaction = the_compaction(&exchanges);
    assert_eq!(reasoning_ids(compaction), ["rs_4"]);
    assert_precedes(compaction, &items[3], is_assistant_text("resumed on A"));
    assert_eq!(compaction_blob(requests[4]), Some("blob-after-restore"));
    assert_eq!(reasoning_ids(requests[5]), ["rs_5"]);
    assert_precedes(requests[5], &items[4], is_call("call-3"));
}

// Given: a Codex worker whose mock tokens are already expired, so every request
// refreshes first, with R1/R2 from the first turn
// When: the auth server issues a new access token for the same ChatGPT account, an
// official compaction runs and the chat continues
// Then: the compaction request (sent with the new token) still receives R1/R2 verbatim,
// the boundary is only the verified blob, R3 is reused afterwards, and every request
// names the same account
#[tokio::test]
async fn same_account_refresh_is_no_boundary_for_official_compaction() {
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
            ScriptedResponse::compaction("compaction", MODEL_A, "blob-after-refresh"),
            read_after(MODEL_A, 3, &r3),
            text_after(MODEL_A, "after compaction", &r4),
        ],
        expiring(),
        false,
    );
    live.mock
        .set_oauth_token_response(token_bundle("access-1", "refresh-1", ACCOUNT, expiring()));
    let run = live.worker("Read with reasoning");
    let mut events = Vec::new();
    live.turn(run, &mut events, None).await;
    live.mock
        .set_oauth_token_response(token_bundle("access-2", "refresh-2", ACCOUNT, expiring()));
    live.runtime.compact(run).unwrap();
    live.turn(run, &mut events, Some("Continue after the refresh"))
        .await;
    live.finish(&[run]).await;

    let recorded: Vec<_> = live
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == Wire::Codex.path())
        .collect();
    let bearer = |token: &str| Some(format!("Bearer {token}"));
    assert_eq!(
        recorded
            .iter()
            .map(|request| request.authorization.clone())
            .collect::<Vec<_>>(),
        [
            bearer("access-1"),
            bearer("access-1"),
            bearer("access-2"),
            bearer("access-2"),
            bearer("access-2")
        ]
    );
    assert!(
        recorded
            .iter()
            .all(|request| request.chatgpt_account_id.as_deref() == Some(ACCOUNT))
    );
    let refreshes = live
        .mock
        .recorded_requests()
        .iter()
        .filter(|request| request.path.ends_with("/oauth/token"))
        .count();
    assert!(refreshes >= 5, "every request refreshes first: {refreshes}");
    let exchanges = exchanges(live.requests(), run, &events);
    for exchange in &exchanges {
        assert_sealed(&exchange.body, &[&r1, &r2, &r3, &r4]);
    }
    assert_cache_chain(&exchanges, &[]);
    let compaction = the_compaction(&exchanges);
    assert_eq!(reasoning_ids(compaction), ["rs_1", "rs_2"]);
    assert_precedes(compaction, &r1, is_call("call-0"));
    let requests = ordinary(&exchanges);
    assert_eq!(compaction_blob(requests[2]), Some("blob-after-refresh"));
    assert_eq!(
        reasoning_ids(requests[3]).last().map(String::as_str),
        Some("rs_3")
    );
    assert_precedes(requests[3], &r3, is_call("call-3"));
}

/// The text after `[COMPACTION CHECKPOINT {id}]\n` in `body`'s user messages.
fn checkpoint_summary(body: &Value, id: &str) -> Option<String> {
    let marker = format!("[COMPACTION CHECKPOINT {id}]\n");
    input(body)
        .iter()
        .filter(|item| item["role"] == "user")
        .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|part| part["text"].as_str().map(str::to_owned))
        .find_map(|text| text.strip_prefix(&marker).map(str::to_owned))
}

const SENTINEL: &str = "Protected-D3-evidence-7781";

// Given: a GPT-5 Codex worker editing through apply_patch whose first turn carries R1/R2
// When: the editing interface is reloaded to edit (a protected D3 text re-summary),
// whose model response alone introduces a sentinel fact, then manual, automatic
// (large answer) and smaller-window official compactions follow, and finally
// the configuration moves to an official OpenAI Responses profile that cannot replay
// the Codex blobs
// Then: no request replays reasoning from before a boundary or leaks ciphertext; the
// non-replaying profile receives neither Codex blobs nor Codex reasoning, yet the
// text-summary HTTP input recovers the protected D3 fact without the hidden old tool
// batch, and that summary's output reaches the next ordinary request, whose prefix
// is reused with positive cache usage
#[tokio::test]
async fn protected_d3_summary_survives_stacked_compactions_and_a_non_replaying_switch() {
    const GPT5: &str = "gpt-5.5";
    let items: Vec<_> = (1..=12).map(|n| reasoning(&format!("rs_{n}"))).collect();
    let known: Vec<_> = items.iter().collect();
    let big = "fresh evidence to compact ".repeat(6_000);
    let switch_sized = "evidence the smaller window must compact ".repeat(6_000);
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let responses = mock_openai::StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            ScriptedResponse::text_stream(
                "recovered-summary",
                MODEL_B,
                [format!("{SENTINEL}: recovered from the safe D3 summary")],
            ),
            read_after(MODEL_B, 10, &items[9]),
            text_after(MODEL_B, "on the Responses profile", &items[10]),
            text_after(MODEL_B, "done", &items[11]),
        ],
        vec![MODEL_B.into()],
    );
    let mut live = Live::with(
        vec![
            read_after(GPT5, 0, &items[0]),
            text_after(GPT5, "the first turn's finding", &items[1]),
            ScriptedResponse::text_stream(
                "d3-summary",
                GPT5,
                [format!("{SENTINEL}: the protected D3 finding")],
            ),
            text_after(GPT5, "after the editing switch", &items[2]),
            ScriptedResponse::compaction("manual", GPT5, "blob-manual"),
            text_after(GPT5, &big, &items[3]),
            text_after(GPT5, "turn four", &items[4]),
            ScriptedResponse::compaction("automatic", GPT5, "blob-automatic"),
            text_after(GPT5, "after automatic compaction", &items[5]),
            text_after(GPT5, &switch_sized, &items[6]),
            ScriptedResponse::compaction("small-window", MODEL_B, "blob-small-window"),
            read_after(MODEL_B, 8, &items[7]),
            text_after(MODEL_B, "after the smaller window", &items[8]),
        ],
        FAR_FUTURE_EXP,
        false,
        Options {
            // The 156K-byte answer triggers Automatic at 256K × 0.5 after
            // reported usage catches up. The 246K-byte answer estimates 61.5K,
            // forcing a switch compaction at 96K × 0.5. The post-compaction
            // tools/read batch stays below 48K even with mock byte usage.
            overrides: vec![
                (format!("work/{GPT5}"), 256_000),
                (format!("work/{MODEL_B}"), 96_000),
                // Recovery includes both post-D3 answers: 402K bytes estimate
                // 100.5K tokens before prompts and tool evidence. Keep the Codex
                // switch at 96K, but let normal summary routing fit the recovery.
                (format!("responses/{MODEL_B}"), 256_000),
            ],
            tools: vec![
                Arc::new(tools::Edit),
                Arc::new(tools::Write),
                Arc::new(tools::ApplyPatch::new()),
            ],
            route: Some(super::Route::codex("work", GPT5)),
            // Keep the model-produced safe summary intact for the later recovery.
            max_summary_bytes: Some(16_384),
            model_summaries: true,
            ..Options::default()
        },
    );
    live.runtime.set_project_root(root).unwrap();
    let run = live.worker("Read with reasoning");
    let mut events = Vec::new();
    live.turn(run, &mut events, None).await;
    let edit = super::Route {
        edit_tool: Some("edit"),
        ..super::Route::codex("work", GPT5)
    };
    let mut config = super::super::route_config(
        live.directory.path(),
        edit,
        &live.mock.codex_base_url(),
        &live.overrides,
        live.max_summary_bytes,
    );
    // Keep one routed model for the later explicit model selection, so its
    // observed editing-interface identity survives the smaller-window switch.
    config.providers.get_mut("work").unwrap().models.push(
        serde_json::from_value(serde_json::json!({
            "id": MODEL_B, "enabled": true, "edit_tool": "edit"
        }))
        .unwrap(),
    );
    live.live
        .replace(super::routed(&config, &live.store, &live.bus, &live.mock));
    live.turn(run, &mut events, Some("Continue with edit"))
        .await;
    live.runtime.compact(run).unwrap();
    for message in [
        "Produce a large answer",
        "Turn four reports the large input",
        "Continue after automatic compaction",
        "Answer at the size the smaller window cannot hold",
    ] {
        live.turn(run, &mut events, Some(message)).await;
    }
    live.runtime
        .set_model_preference(
            run,
            Some(runtime::ModelPreference {
                profile: "work".into(),
                model: Some(MODEL_B.into()),
                reasoning_effort: None,
            }),
        )
        .unwrap();
    live.turn(run, &mut events, Some("Continue in the smaller window"))
        .await;
    live.reroute(
        super::Route {
            kind: "openai",
            profile: "responses",
            model: MODEL_B,
            edit_tool: Some("edit"),
        },
        &responses.base_url(),
    );
    // Exercise normal chain routing, including its summary window filter, with
    // the existing run pin; an explicit preference would bypass that filter.
    live.runtime.set_model_preference(run, None).unwrap();
    for message in ["Continue on Responses", "Finish"] {
        live.turn(run, &mut events, Some(message)).await;
    }
    live.finish(&[run]).await;
    assert_eq!(responses.remaining_scripts(), 0);
    for event in &events {
        if let EventKind::Diagnostic(diagnostic) = &event.kind {
            assert_ne!(
                diagnostic.code,
                event_bus::event::diagnostic_codes::COMPACTION_FAILED,
                "the model re-summary must not fall back: {diagnostic:?}"
            );
        }
    }

    let reasons: Vec<_> = checkpoints(&events).into_iter().map(|(r, _)| r).collect();
    assert_eq!(
        reasons,
        [
            CompactionReason::ModelSwitch,
            CompactionReason::Manual,
            CompactionReason::Automatic,
            CompactionReason::ModelSwitch,
            CompactionReason::ModelSwitch,
        ],
        "{reasons:?}"
    );
    let codex = live.requests();
    let openai = posted(&responses, Wire::Responses.path());
    assert_eq!(openai.len(), 4);
    for body in codex.iter().chain(&openai) {
        assert_sealed(body, &known);
    }
    let mut bodies = codex.clone();
    bodies.extend(openai.iter().cloned());
    let exchanges = exchanges(bodies, run, &events);
    // The editing switch (D3) may replace the editing tools, and the move to
    // Responses changes the provider; each segment between them is one cache chain.
    let switch = exchanges.len() - openai.len();
    assert_cache_chain(&exchanges[..2], &[]);
    assert_cache_chain(&exchanges[2..switch], &[5]);
    assert_cache_chain(&exchanges[switch..], &[]);
    let d3 = &checkpoints(&events)[0].1;
    let d3_request = &exchanges[3].body;
    assert_eq!(
        exchanges[3].checkpoint.as_ref().map(|(reason, _)| *reason),
        Some(CompactionReason::ModelSwitch)
    );
    let mut before_d3 = exchanges[1].body.clone();
    before_d3["tools"] = d3_request["tools"].clone();
    mock_openai::cache_contract::assert_compaction_transition(
        mock_openai::cache_contract::CacheProtocol::Codex,
        &before_d3,
        d3_request,
        d3,
    )
    .unwrap();
    assert!(exchanges[3].usage.unwrap().1 < exchanges[1].usage.unwrap().0);
    assert_eq!(
        exchanges[switch + 1]
            .checkpoint
            .as_ref()
            .map(|(reason, _)| *reason),
        Some(CompactionReason::ModelSwitch),
        "the first Responses request consumes the switch's replacement"
    );
    let summary = checkpoint_summary(d3_request, d3).expect("the D3 boundary checkpoint");
    assert!(summary.contains(SENTINEL), "{summary}");
    assert!(reasoning_ids(d3_request).is_empty());
    let text_summaries: Vec<_> = exchanges
        .iter()
        .filter(|exchange| {
            exchange.compaction && !super::super::is_compaction_request(&exchange.body)
        })
        .collect();
    assert_eq!(text_summaries.len(), 2);
    assert!(
        text_summaries[0].body.to_string().contains("call-0"),
        "D3 must actually replace the original tool batch"
    );
    assert!(
        !text_summaries[0].body.to_string().contains(SENTINEL),
        "the sentinel must originate only in the protected D3 summary response"
    );
    let recovered = &text_summaries[1].body;
    let recovered_wire = recovered.to_string();
    assert!(
        recovered_wire.contains(SENTINEL),
        "lost D3 fact: {recovered}"
    );
    assert!(
        recovered_wire.contains(&big) && recovered_wire.contains(&switch_sized),
        "the recovery must include both post-D3 answers, not only the latest native blob"
    );
    assert!(
        !recovered_wire.contains("call-0"),
        "the hidden pre-D3 tool batch must not return: {recovered}"
    );
    assert!(reasoning_ids(recovered).is_empty());
    for body in &codex[3..] {
        assert!(
            !body.to_string().contains("call-0"),
            "the protected batch returned after D3"
        );
    }
    let compacted = compaction_requests(&codex);
    assert_eq!(compacted.len(), 3);
    assert_eq!(compacted[2]["model"], MODEL_B);
    assert!(reasoning_ids(compacted[2]).is_empty());
    assert_eq!(
        compaction_blob(&exchanges[switch - 2].body),
        Some("blob-small-window")
    );
    for compaction in compacted {
        let ids = reasoning_ids(compaction);
        assert!(
            !ids.iter().any(|id| id == "rs_1" || id == "rs_2"),
            "{ids:?}"
        );
    }
    for body in &openai {
        assert!(
            !body.to_string().contains("blob-"),
            "a Codex blob reached Responses"
        );
        assert!(!body.to_string().contains("call-0"));
        assert!(
            reasoning_ids(body)
                .iter()
                .all(|id| ["rs_10", "rs_11", "rs_12"].contains(&id.as_str())),
            "Codex reasoning reached Responses: {:?}",
            reasoning_ids(body)
        );
    }
    assert!(
        openai[1].to_string().contains(SENTINEL),
        "the protected D3 summary was lost: {}",
        openai[1]
    );
    assert!(
        !reasoning_ids(openai.last().unwrap()).is_empty(),
        "new Responses reasoning is reused"
    );
    assert!(
        exchanges.last().unwrap().checkpoint.is_none(),
        "the last request reuses the prefix after every boundary"
    );
}
