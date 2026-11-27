//! A file-touch notice is an ordinary append-only provider input: raised by a real
//! `read` in one thread and real `edit`s in another, it reaches the reader once, at
//! its next loop head, behind the history already sent, and the request after it
//! keeps the prefix that includes it. Instructions, model, tools and cache affinity
//! never change. The scenario runs on every OpenAI-shaped wire.
use super::*;
use config::EditToolConfig;
use std::path::Path;

const READER_GOAL: &str = "FILE-TOUCH-READER-GOAL";
const EDITOR_GOAL: &str = "FILE-TOUCH-EDITOR-GOAL";
const NOTICE: &str = "id=file-touch-";

struct ReleaseOnDrop(mock_openai::ResponseGate);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn call(id: &str, name: &str, input: Value) -> ScriptedResponse {
    ScriptedResponse::tool_call(id, MODEL, 0, id, name, [input.to_string()])
}

fn read_call(id: &str, path: &Path) -> ScriptedResponse {
    call(id, "read", json!({"path": path}))
}

fn edit_call(id: &str, path: &Path, old: &str, new: &str) -> ScriptedResponse {
    call(
        id,
        "edit",
        json!({"path": path, "old_string": old, "new_string": new}),
    )
}

fn rules(root: &Path) -> Arc<runtime::RulesSource> {
    Arc::new(runtime::RulesSource::new(
        runtime::ProjectTrust::Approved,
        runtime::RulesSettings {
            context_window_tokens: 1_000_000,
            response_headroom_tokens: 16_384,
            max_injection_bytes: 65_536,
        },
        None,
        Some(root.to_path_buf()),
        None,
    ))
}

fn history(request: &Value) -> String {
    request
        .get("messages")
        .or_else(|| request.get("input"))
        .unwrap()
        .to_string()
}

/// The wire requests whose conversation carries `goal`, in the order they were sent.
fn requests_of(harness: &Harness, goal: &str) -> Vec<Value> {
    harness
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == harness.wire.path())
        .map(|request| request.body)
        .filter(|body| history(body).contains(goal))
        .collect()
}

fn notices(request: &Value) -> usize {
    history(request).matches(NOTICE).count()
}

/// `(input_tokens, cache_read_tokens)` the provider reported for each request of `run`.
fn usage_of(events: &[Event], run: RunId) -> Vec<(u64, u64)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                run_id: Some(id),
                input_tokens,
                cache_read_tokens,
                ..
            }) if id == &run.to_string() => Some((*input_tokens, *cache_read_tokens)),
            EventKind::Diagnostic(diagnostic) if diagnostic.code == "CacheRegression" => {
                panic!("{diagnostic:?}")
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn file_touch_cache_notice_from_a_real_edit_is_append_only_on_every_wire() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let project = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        let (x, y, z) = (root.join("x.txt"), root.join("y.txt"), root.join("z.txt"));
        std::fs::write(&x, "alpha\nbeta\n").unwrap();
        std::fs::write(&y, "one\n").unwrap();
        std::fs::write(&z, "two\n").unwrap();

        let (gate, arrived) = mock_openai::ResponseGate::new();
        let mut harness = harness_with_wire(
            wire,
            vec![
                read_call("read-x", &x),
                read_call("read-y", &y).with_gate(gate.clone()),
            ],
            1_000_000,
            Arc::new(tools::Read),
            vec![Arc::new(tools::Edit)],
            None,
            0.9,
            |config| {
                config.providers.get_mut("local").unwrap().models[0].edit_tool =
                    Some(EditToolConfig::Edit);
            },
        );
        let _release = ReleaseOnDrop(gate.clone());
        harness.runtime = harness.runtime.with_project_rules(rules(&root));
        harness.runtime.set_project_root(root.clone()).unwrap();

        let spawn = |thread: &str, goal: &str| {
            let run = harness.runtime.reserve_run_id();
            harness.runtime.bind_thread_root(thread, run).unwrap();
            (run, goal.to_owned())
        };
        let (reader, reader_goal) = spawn("file-touch-reader", READER_GOAL);
        harness
            .runtime
            .spawn_reserved(
                reader,
                None,
                Role::Worker,
                reader_goal,
                RunConfig::default(),
            )
            .unwrap();
        // The reader's second response waits at the gate; its first has been answered, so
        // `x.txt` is read and the mock's script is empty.
        tokio::task::spawn_blocking(move || arrived.recv().unwrap())
            .await
            .unwrap();

        harness.mock.enqueue([
            edit_call("edit-1", &x, "alpha", "alpha-a"),
            edit_call("edit-2", &x, "alpha-a", "alpha-aa"),
            ScriptedResponse::text_stream("editor-reply", MODEL, ["editor done"]),
        ]);
        let (editor, editor_goal) = spawn("file-touch-editor", EDITOR_GOAL);
        harness
            .runtime
            .spawn_reserved(
                editor,
                None,
                Role::Worker,
                editor_goal,
                RunConfig::default(),
            )
            .unwrap();
        let mut events = through_phase(&mut harness.receiver, editor, AgentRunPhase::Done).await;
        assert_eq!(std::fs::read_to_string(&x).unwrap(), "alpha-aa\nbeta\n");

        harness.mock.enqueue([
            read_call("read-z", &z),
            ScriptedResponse::text_stream("reader-reply", MODEL, ["reader done"]),
        ]);
        gate.release();
        events.extend(through_phase(&mut harness.receiver, reader, AgentRunPhase::Done).await);
        harness.runtime.wait(reader).await.unwrap();
        assert_eq!(harness.mock.remaining_scripts(), 0);

        let requests = requests_of(&harness, READER_GOAL);
        assert_eq!(requests.len(), 4, "{}", wire.path());
        assert_eq!(
            requests.iter().map(notices).collect::<Vec<_>>(),
            [0, 0, 1, 1],
            "the notice reaches the reader once and stays in the history afterwards"
        );
        let notice = history(&requests[2]);
        assert!(
            notice.contains("x.txt") && notice.contains("2 changes"),
            "{notice}"
        );
        assert!(!notice.contains("y.txt\\\":") && !notice.contains("z.txt\\\":"));
        let last = |request: &Value| {
            request
                .get("messages")
                .or_else(|| request.get("input"))
                .and_then(Value::as_array)
                .and_then(|items| items.last())
                .unwrap()
                .to_string()
        };
        assert!(
            last(&requests[2]).contains(NOTICE),
            "the notice is appended last"
        );
        for request in requests_of(&harness, EDITOR_GOAL) {
            assert_eq!(notices(&request), 0, "the writer is never told");
        }

        // Every request keeps the whole previous request as its prefix, with unchanged
        // instructions, model, tools and affinity (settings are compared too), across the
        // notice and in the request after it.
        for pair in requests.windows(2) {
            assert_append_only(wire.protocol(), &pair[0], &pair[1])
                .unwrap_or_else(|error| panic!("{}: {error}", wire.path()));
            assert_eq!(pair[0]["tools"], pair[1]["tools"]);
        }
        let items = |request: &Value| {
            request
                .get("messages")
                .or_else(|| request.get("input"))
                .and_then(Value::as_array)
                .unwrap()
                .len()
        };
        assert!(items(&requests[2]) > items(&requests[1]));
        assert!(items(&requests[3]) > items(&requests[2]));

        // The mock derives cached tokens from the bytes it received: each request reuses
        // everything the previous one processed.
        let usage = usage_of(&events, reader);
        assert_eq!(usage.len(), 4, "{usage:?}");
        assert_eq!(usage[0].1, 0, "the first request is cold");
        for pair in usage.windows(2) {
            assert!(pair[0].0 > 0 && pair[1].1 >= pair[0].0, "{usage:?}");
        }
    }
}
