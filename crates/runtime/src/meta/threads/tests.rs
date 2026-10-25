//! ADR 0041 acceptance tests for the thread tools: visibility, safe output,
//! stable refs, bounded output with cursors, and the dispatch path from a run.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolResultContent,
    Usage,
};
use secret_guard::SecretRedactor;
use serde_json::{Value, json};
use storage::RunContextRecord;
use storage::threads::{ThreadLineageKind, ThreadUpsert};

use super::normalize::UNTRUSTED_NOTE;
use super::visibility::Caller;
use super::{MAX_OUTPUT_BYTES, Scope, grep, read, search};
use crate::{AgentModel, AgentRuntime, Role, RunConfig, RunStore, RuntimeError};

const SECRET: &str = "known-secret-value-1234";

fn denial(id: &str) -> String {
    format!(
        "thread_denied: {id} is not visible from this run; a thread outside your project becomes readable only when the user names its ID in this conversation"
    )
}

struct ScriptModel(Mutex<VecDeque<ChatResponse>>);

#[async_trait::async_trait]
impl AgentModel for ScriptModel {
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[providers::ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| reply(vec![text("done")], FinishReason::Stop)))
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
}

fn reply(content: Vec<ContentBlock>, finish_reason: FinishReason) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: MessageRole::Assistant,
            content,
        },
        finish_reason,
        usage: Usage::default(),
    }
}

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text { text: text.into() }
}

fn user(content: Vec<ContentBlock>) -> Message {
    Message {
        role: MessageRole::User,
        content,
    }
}

fn assistant(content: Vec<ContentBlock>) -> Message {
    Message {
        role: MessageRole::Assistant,
        content,
    }
}

struct Fixture {
    runtime: AgentRuntime,
    _storage: storage::Storage,
    _db: tempfile::TempDir,
    project: tempfile::TempDir,
    other: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self::with_script(Vec::new())
    }

    fn with_script(script: Vec<ChatResponse>) -> Self {
        let db = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: db.path().join("threads.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(event_bus::EventBus::new(4096));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            Arc::new(ScriptModel(Mutex::new(script.into()))),
        )
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        Self {
            runtime,
            _storage: storage,
            _db: db,
            project: tempfile::tempdir().unwrap(),
            other: tempfile::tempdir().unwrap(),
        }
    }

    fn store(&self) -> &RunStore {
        self.runtime.shared.run_store.get().unwrap()
    }

    fn canonical(path: &Path) -> String {
        path.canonicalize().unwrap().to_string_lossy().into_owned()
    }

    fn thread(
        &self,
        id: &str,
        project: Option<&Path>,
        root: &str,
        edit: impl FnOnce(&mut ThreadUpsert),
    ) {
        let mut row = ThreadUpsert {
            thread_id: id.into(),
            project_root: project.map(Self::canonical),
            root_run_id: Some(root.into()),
            title: Some(format!("title of {id}")),
            created_at_ns: 1,
            updated_at_ns: 1,
            ..Default::default()
        };
        edit(&mut row);
        self.store().handle.upsert_thread_runtime(&row).unwrap();
    }

    /// Save a terminal run snapshot; `thread` binds a root, `parent` a child.
    fn run(&self, run: &str, thread: Option<&str>, parent: Option<&str>, messages: Vec<Message>) {
        let config = match thread {
            Some(thread) => json!({"thread_id": thread}),
            None => json!({}),
        };
        self.store()
            .handle
            .upsert_run_context(&RunContextRecord {
                run_id: run.into(),
                role: "Worker".into(),
                name: format!("name of {run}"),
                parent_run_id: parent.map(Into::into),
                config_json: config.to_string(),
                messages_json: serde_json::to_string(&messages).unwrap(),
                checkpoints_json: "[]".into(),
                terminal_phase: "Done".into(),
                restorable: false,
                updated_at_ns: crate::run_store::review_time_ns(),
            })
            .unwrap();
    }

    fn human(&self, run: &str, kind: &str, said: &str) {
        let row = crate::run_store::evidence_row(run, run, kind, said);
        self.store()
            .handle
            .append_review_evidence_with_reference_consumption(&[row], &[], None)
            .unwrap();
    }

    fn call(
        &self,
        who: (&str, Option<&str>, Option<&Path>),
        tool: &str,
        input: Value,
    ) -> Result<Value, String> {
        let caller = Caller::new(who.0.parse().unwrap(), who.1.map(Into::into), who.2);
        let scope = Scope::new(
            self.store(),
            &self.runtime,
            caller,
            SecretRedactor::with_known_values([SECRET.into()]),
        );
        let result = match tool {
            "thread_search" => search::run(&scope, serde_json::from_value(input).unwrap()),
            "thread_grep" => grep::run(&scope, serde_json::from_value(input).unwrap()),
            "thread_read" => read::run(&scope, serde_json::from_value(input).unwrap()),
            _ => unreachable!(),
        };
        if let Ok(value) = &result {
            assert_eq!(value["note"], UNTRUSTED_NOTE, "{tool}: {value}");
            assert!(value.to_string().len() <= MAX_OUTPUT_BYTES, "{tool}");
        }
        result
    }

    /// A caller in its own thread of `self.project`.
    fn own(&self) -> (&'static str, Option<&'static str>, Option<&Path>) {
        self.thread("own", Some(self.project.path()), "run-100", |_| {});
        self.run(
            "run-100",
            Some("own"),
            None,
            vec![user(vec![text("hello")])],
        );
        ("run-100", Some("own"), Some(self.project.path()))
    }
}

fn all_tools(thread: &str) -> [(&'static str, Value); 4] {
    [
        ("thread_grep", json!({"thread": thread, "pattern": "a"})),
        ("thread_read", json!({"thread": thread})),
        ("thread_read", json!({"thread": thread, "view": "messages"})),
        (
            "thread_read",
            json!({"thread": thread, "view": "messages", "around": "run-9#0.0"}),
        ),
    ]
}

#[test]
fn other_project_threads_are_denied_exactly_like_missing_ids() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("foreign", Some(f.other.path()), "run-200", |_| {});
    f.run(
        "run-200",
        Some("foreign"),
        None,
        vec![user(vec![text("foreign secret plan")])],
    );
    for id in ["foreign", "missing-thread", "run-200", "run-999"] {
        for (tool, input) in all_tools(id) {
            assert_eq!(
                f.call(own, tool, input).unwrap_err(),
                denial(id),
                "{tool} {id}"
            );
        }
    }
    let listed = f.call(own, "thread_search", json!({})).unwrap();
    assert!(!listed.to_string().contains("foreign"), "{listed}");
    let searched = f
        .call(own, "thread_search", json!({"query": "plan"}))
        .unwrap();
    assert_eq!(searched["threads"], json!([]));
}

#[test]
fn same_project_threads_are_readable_including_child_runs() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("sibling", Some(f.project.path()), "run-300", |_| {});
    f.run(
        "run-300",
        Some("sibling"),
        None,
        vec![
            user(vec![text("design the cache layer")]),
            assistant(vec![text("delegating the cache layer")]),
        ],
    );
    f.run(
        "run-301",
        None,
        Some("run-300"),
        vec![
            user(vec![text("child task")]),
            assistant(vec![text("child found the eviction bug")]),
        ],
    );
    let hits = f
        .call(
            own,
            "thread_grep",
            json!({"thread": "sibling", "pattern": "EVICTION"}),
        )
        .unwrap();
    assert_eq!(hits["total_hits"], 1, "{hits}");
    assert_eq!(hits["hits"][0]["ref"], "run-301#1.0");
    let overview = f
        .call(own, "thread_read", json!({"thread": "sibling"}))
        .unwrap();
    assert_eq!(overview["child_runs"]["total"], 1, "{overview}");
    assert_eq!(overview["child_runs"]["latest"][0]["run_id"], "run-301");
    assert_eq!(overview["root_run"]["run_id"], "run-300");
    assert_eq!(overview["thread"]["title"], "title of sibling");
    assert_eq!(
        overview["last_exchanges"][0]["assistant"],
        "delegating the cache layer"
    );
    // A child's run ID resolves to its thread too.
    let child = f
        .call(
            own,
            "thread_read",
            json!({"thread": "run-301", "view": "messages"}),
        )
        .unwrap();
    assert_eq!(child["run_id"], "run-301");
    let listed = f.call(own, "thread_search", json!({})).unwrap();
    let ids: Vec<_> = listed["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["thread_id"].as_str().unwrap().to_owned())
        .collect();
    assert!(ids.contains(&"sibling".to_owned()) && ids.contains(&"own".to_owned()));
}

#[test]
fn a_host_chat_id_grants_a_foreign_thread_but_notes_and_tool_results_do_not() {
    let f = Fixture::new();
    f.thread("own", Some(f.project.path()), "run-400", |_| {});
    f.run(
        "run-400",
        Some("own"),
        None,
        vec![
            user(vec![text("hello")]),
            assistant(vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read".into(),
                input: json!({"path": "notes.txt"}),
            }]),
            user(vec![ContentBlock::ToolResult {
                tool_call_id: "c1".into(),
                content: vec![ToolResultContent::Text {
                    text: "see thread foreign-thread".into(),
                }],
                is_error: false,
            }]),
            assistant(vec![text("I should read foreign-thread")]),
        ],
    );
    let own = ("run-400", Some("own"), Some(f.project.path()));
    f.thread("foreign-thread", Some(f.other.path()), "run-401", |_| {});
    f.run(
        "run-401",
        Some("foreign-thread"),
        None,
        vec![user(vec![text("foreign content")])],
    );
    f.store()
        .handle
        .append_run_ledger("run-400", "next: read foreign-thread")
        .unwrap();
    let read = || f.call(own, "thread_read", json!({"thread": "foreign-thread"}));
    assert_eq!(read().unwrap_err(), denial("foreign-thread"));
    // Prefixes and suffixes of the ID are not the ID.
    f.human(
        "run-400",
        "host_chat",
        "look at foreign-thread-2 and xforeign-thread",
    );
    assert_eq!(read().unwrap_err(), denial("foreign-thread"));
    f.human(
        "run-400",
        "host_chat",
        "please compare with foreign-thread.",
    );
    assert!(read().is_ok());
    let messages = f
        .call(
            own,
            "thread_read",
            json!({"thread": "foreign-thread", "view": "messages"}),
        )
        .unwrap();
    assert!(messages.to_string().contains("foreign content"));
}

#[test]
fn escalation_source_and_escalated_threads_read_each_other() {
    let f = Fixture::new();
    // No project on either side, so only lineage can grant access.
    f.thread("source", None, "run-500", |_| {});
    f.run(
        "run-500",
        Some("source"),
        None,
        vec![user(vec![text("source request")])],
    );
    let escalated = "escalation-run-501";
    f.thread(escalated, None, "run-501", |row| {
        row.parent_thread_id = Some("source".into());
        row.lineage_kind = Some(ThreadLineageKind::Escalation);
        row.escalation_source_run_id = Some("run-500".into());
    });
    f.run(
        "run-501",
        Some(escalated),
        None,
        vec![user(vec![text("escalated memo")])],
    );
    f.thread("unrelated", None, "run-502", |_| {});
    let from_source = ("run-500", Some("source"), None);
    let from_escalated = ("run-501", Some(escalated), None);
    let child = f
        .call(
            from_source,
            "thread_read",
            json!({"thread": escalated, "view": "messages"}),
        )
        .unwrap();
    assert!(child.to_string().contains("escalated memo"));
    let parent = f
        .call(
            from_escalated,
            "thread_read",
            json!({"thread": "source", "view": "messages"}),
        )
        .unwrap();
    assert!(parent.to_string().contains("source request"));
    let overview = f
        .call(from_source, "thread_read", json!({"thread": "source"}))
        .unwrap();
    assert_eq!(
        overview["thread"]["child_threads"][0]["thread_id"],
        escalated
    );
    assert_eq!(overview["thread"]["child_threads"][0]["kind"], "escalation");
    for caller in [from_source, from_escalated] {
        assert_eq!(
            f.call(caller, "thread_read", json!({"thread": "unrelated"}))
                .unwrap_err(),
            denial("unrelated")
        );
    }
    let listed = f.call(from_escalated, "thread_search", json!({})).unwrap();
    assert!(listed.to_string().contains("\"source\""), "{listed}");
}

#[test]
fn hidden_blocks_images_and_secrets_never_reach_any_output() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("sibling", Some(f.project.path()), "run-600", |row| {
        row.title = Some(format!("title {SECRET}"));
    });
    f.run(
        "run-600",
        Some("sibling"),
        None,
        vec![
            Message {
                role: MessageRole::System,
                content: vec![text("system-prompt-hidden")],
            },
            user(vec![
                text(&format!("token {SECRET} please")),
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "aGlkZGVuLWltYWdlLWJ5dGVz".into(),
                },
            ]),
            assistant(vec![
                ContentBlock::Reasoning {
                    text: "reasoning-hidden".into(),
                },
                ContentBlock::Anthropic {
                    block: json!({"type":"thinking","thinking":"thinking-hidden","signature":"sig-hidden"}),
                    prefix_hash: None,
                },
                ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "shell".into(),
                    input: json!({"command": format!("echo {SECRET}")}),
                },
            ]),
            user(vec![ContentBlock::ToolResult {
                tool_call_id: "c1".into(),
                content: vec![
                    ToolResultContent::Text {
                        text: format!("{SECRET} printed"),
                    },
                    ToolResultContent::Image {
                        media_type: "image/jpeg".into(),
                        data: "cmVzdWx0LWltYWdlLWJ5dGVz".into(),
                        source: None,
                    },
                ],
                is_error: false,
            }]),
            assistant(vec![text(&format!("final answer without {SECRET}"))]),
        ],
    );
    f.store()
        .handle
        .append_run_ledger("run-600", &format!("note {SECRET}"))
        .unwrap();
    f.human(
        "run-600",
        "initial_prompt",
        &format!("token {SECRET} please"),
    );
    let outputs = [
        f.call(own, "thread_read", json!({"thread": "sibling"})),
        f.call(
            own,
            "thread_read",
            json!({"thread": "sibling", "view": "messages", "max_bytes": 16384}),
        ),
        f.call(
            own,
            "thread_grep",
            json!({"thread": "sibling", "pattern": ".", "regex": true, "max_hits": 50}),
        ),
        f.call(own, "thread_search", json!({"query": "token"})),
        f.call(own, "thread_search", json!({"query": "title"})),
    ];
    for output in outputs {
        let output = output.unwrap().to_string();
        for hidden in [
            SECRET,
            "system-prompt-hidden",
            "reasoning-hidden",
            "thinking-hidden",
            "sig-hidden",
            "aGlkZGVuLWltYWdlLWJ5dGVz",
            "cmVzdWx0LWltYWdlLWJ5dGVz",
        ] {
            assert!(!output.contains(hidden), "{hidden} leaked: {output}");
        }
    }
    let messages = f
        .call(
            own,
            "thread_read",
            json!({"thread": "sibling", "view": "messages", "max_bytes": 16384}),
        )
        .unwrap();
    let text = messages.to_string();
    assert!(text.contains("[image image/png]") && text.contains("[image image/jpeg]"));
    assert!(text.contains("REDACTED"));
    // Tool input stays a nested value instead of an escaped JSON string.
    assert!(
        messages["messages"][1]["blocks"][0]["input"]["command"].is_string(),
        "{messages}"
    );
    assert_eq!(messages["messages"][1]["blocks"][0]["ref"], "run-600#1.2");
    // Redaction happens before matching, so a pattern cannot probe a secret.
    let probe = f
        .call(
            own,
            "thread_grep",
            json!({"thread": "sibling", "pattern": SECRET}),
        )
        .unwrap();
    assert_eq!(probe["total_hits"], 0, "{probe}");
    let probe = f
        .call(
            own,
            "thread_grep",
            json!({"thread": "sibling", "pattern": "hidden"}),
        )
        .unwrap();
    assert_eq!(probe["total_hits"], 0, "{probe}");
}

#[test]
fn metadata_is_redacted_without_changing_authorization_or_search_cursors() {
    let f = Fixture::new();
    let secret = format!("ghp_{}", "A".repeat(40));
    assert!(
        SecretRedactor::from_env()
            .redact(&secret)
            .text
            .contains("REDACTED")
    );
    let project = f.project.path().join(&secret);
    std::fs::create_dir(&project).unwrap();
    for (id, time) in [(secret.as_str(), 2), ("older", 1)] {
        f.thread(id, Some(&project), "run-800", |row| {
            row.project_id = Some(secret.clone());
            row.updated_at_ns = time;
        });
    }
    f.run(
        "run-800",
        Some(&secret),
        None,
        vec![user(vec![text("needle")])],
    );
    let caller = ("run-100", Some("own"), Some(project.as_path()));
    for (tool, args) in [
        ("thread_read", json!({"thread": secret})),
        ("thread_read", json!({"thread": secret, "view": "messages"})),
        (
            "thread_grep",
            json!({"thread": secret, "pattern": "needle"}),
        ),
        ("thread_search", json!({"limit": 1})),
    ] {
        let value = f.call(caller, tool, args).unwrap();
        assert!(!value.to_string().contains(&secret), "{tool}: {value}");
        assert!(value.to_string().contains("REDACTED"), "{tool}: {value}");
        if tool == "thread_search" {
            let next = value["next_cursor"].as_str().unwrap();
            let page = f
                .call(caller, tool, json!({"limit": 1, "cursor": next}))
                .unwrap();
            assert_eq!(page["threads"][0]["thread_id"], "older");
        }
    }
    // Redacting the canonical path in output must not broaden project visibility.
    assert!(
        f.call(
            ("run-100", Some("own"), Some(f.other.path())),
            "thread_read",
            json!({"thread": secret})
        )
        .is_err()
    );
}

#[test]
fn long_tool_identifiers_do_not_stall_read_or_grep_pagination() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("long-tools", Some(f.project.path()), "run-810", |_| {});
    let id = "識".repeat(6000);
    let name = "名".repeat(6000);
    f.run(
        "run-810",
        Some("long-tools"),
        None,
        vec![
            assistant(vec![ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: json!({"needle": "x"}),
            }]),
            user(vec![ContentBlock::ToolResult {
                tool_call_id: id.clone(),
                content: vec![ToolResultContent::Text {
                    text: "needle result".into(),
                }],
                is_error: false,
            }]),
            assistant(vec![text("needle later")]),
        ],
    );
    for tool in ["thread_read", "thread_grep"] {
        let mut args = if tool == "thread_read" {
            json!({"thread": "long-tools", "view": "messages", "max_bytes": 1024})
        } else {
            json!({"thread": "long-tools", "pattern": "needle", "max_hits": 1})
        };
        let mut saw_later = false;
        let mut finished = false;
        for _ in 0..20 {
            let page = f.call(own, tool, args.clone()).unwrap();
            assert!(!page.to_string().contains(&id));
            assert!(!page.to_string().contains(&name));
            saw_later |= page.to_string().contains("needle later");
            let next = &page["next_cursor"];
            if next.is_null() {
                finished = true;
                break;
            }
            assert_ne!(next, &args["cursor"], "cursor must progress");
            args["cursor"] = next.clone();
        }
        assert!(finished && saw_later, "{tool} must reach the later message");
    }
    assert!(super::fit(10, 1, |_| json!({"too_large": "x".repeat(100)})).is_err());
}

#[test]
fn around_refs_stay_stable_and_max_bytes_pages_with_a_cursor() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("long", Some(f.project.path()), "run-700", |_| {});
    let mut messages = vec![Message {
        role: MessageRole::System,
        content: vec![text("system")],
    }];
    for turn in 0..6 {
        messages.push(user(vec![text(&format!("question {turn}"))]));
        messages.push(assistant(vec![text(&format!("answer {turn}"))]));
    }
    let long_text = "長い本文 with \"quotes\"\n".repeat(400);
    messages.push(user(vec![text("needle-marker"), text(&long_text)]));
    f.run("run-700", Some("long"), None, messages.clone());
    let hits = f
        .call(
            own,
            "thread_grep",
            json!({"thread": "long", "pattern": "needle-marker"}),
        )
        .unwrap();
    let reference = hits["hits"][0]["ref"].as_str().unwrap().to_owned();
    assert_eq!(reference, "run-700#12.0");
    let around = json!({"thread": "long", "view": "messages", "around": reference, "before": 2, "after": 0, "max_bytes": 1024});
    let first = f.call(own, "thread_read", around.clone()).unwrap();
    assert_eq!(f.call(own, "thread_read", around.clone()).unwrap(), first);
    assert_eq!(first["focus"], reference);
    assert_eq!(first["first_ordinal"], 10);
    assert_eq!(first["last_ordinal"], 12);
    assert!(first.to_string().len() <= 1024);
    // Later turns do not move an existing ref.
    messages.push(assistant(vec![text("a later reply")]));
    f.run("run-700", Some("long"), None, messages);
    let again = f.call(own, "thread_read", around.clone()).unwrap();
    for key in [
        "focus",
        "first_ordinal",
        "last_ordinal",
        "messages",
        "next_cursor",
    ] {
        assert_eq!(again[key], first[key], "{key}");
    }
    assert_eq!(again["message_count"], 14);
    // Follow cursors until the window is complete and reassemble the long block.
    let mut cursor = first["next_cursor"].as_str().map(str::to_owned);
    assert!(
        cursor.is_some(),
        "1024 bytes cannot hold the long block: {first}"
    );
    let mut pages = vec![first];
    while let Some(next) = cursor {
        let mut input = around.clone();
        input["cursor"] = json!(next);
        let page = f.call(own, "thread_read", input).unwrap();
        assert!(page.to_string().len() <= 1024, "{page}");
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        pages.push(page);
        assert!(pages.len() < 200, "cursor must make progress");
    }
    let mut assembled = String::new();
    for page in &pages {
        for message in page["messages"].as_array().unwrap() {
            for block in message["blocks"].as_array().unwrap() {
                if block["ref"] == "run-700#12.1" {
                    assert_eq!(block["text_offset"], assembled.len());
                    assembled.push_str(block["text"].as_str().unwrap());
                }
            }
        }
    }
    assert_eq!(assembled, long_text);
    assert_eq!(
        f.call(
            own,
            "thread_read",
            json!({"thread": "long", "view": "messages", "around": "run-700#99.0"})
        )
        .unwrap_err(),
        "thread_ref_out_of_range: run-700#99.0 is beyond the 14 messages of run-700"
    );
    let mut bad = around;
    bad["cursor"] = json!("run-700#3.0@0");
    assert!(
        f.call(own, "thread_read", bad)
            .unwrap_err()
            .starts_with("thread_cursor_invalid")
    );
}

#[test]
fn grep_ranks_tool_results_last_collapses_repeats_and_pages() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("tools", Some(f.project.path()), "run-800", |_| {});
    let call = |id: &str| ContentBlock::ToolUse {
        id: id.into(),
        name: "read".into(),
        input: json!({"path": "a.rs"}),
    };
    let result = |id: &str| ContentBlock::ToolResult {
        tool_call_id: id.into(),
        content: vec![ToolResultContent::Text {
            text: "fn target() {}".into(),
        }],
        is_error: false,
    };
    f.run(
        "run-800",
        Some("tools"),
        None,
        vec![
            user(vec![text("find target")]),
            assistant(vec![call("a"), call("b"), call("c")]),
            user(vec![result("a"), result("b"), result("c")]),
            assistant(vec![text("target found")]),
        ],
    );
    let hits = f
        .call(
            own,
            "thread_grep",
            json!({"thread": "tools", "pattern": "target"}),
        )
        .unwrap();
    let kinds: Vec<_> = hits["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["text", "text", "tool_result"], "{hits}");
    assert_eq!(hits["hits"][2]["repeat_count"], 3);
    assert_eq!(hits["hits"][2]["ref"], "run-800#2.0");
    let all = f
        .call(
            own,
            "thread_grep",
            json!({"thread": "tools", "pattern": "target", "collapse_repeats": false, "kinds": ["tool_result"], "max_hits": 2}),
        )
        .unwrap();
    assert_eq!(all["total_hits"], 3);
    assert_eq!(all["next_cursor"], "2");
    let rest = f
        .call(
            own,
            "thread_grep",
            json!({"thread": "tools", "pattern": "target", "collapse_repeats": false, "kinds": ["tool_result"], "cursor": "2"}),
        )
        .unwrap();
    assert_eq!(rest["hits"][0]["ref"], "run-800#2.2");
    assert_eq!(rest["next_cursor"], Value::Null);
    assert_eq!(
        f.call(
            own,
            "thread_grep",
            json!({"thread": "tools", "pattern": "x", "runs": ["run-100"]})
        )
        .unwrap_err(),
        "thread_run_not_found: run-100 is not a saved run of thread tools"
    );
}

#[test]
fn search_matches_metadata_only_and_pages_by_cursor() {
    let f = Fixture::new();
    let own = f.own();
    for (index, id) in ["alpha", "beta", "gamma"].iter().enumerate() {
        let run = format!("run-90{index}");
        f.thread(id, Some(f.project.path()), &run, |row| {
            row.updated_at_ns = 10 + index as i64;
        });
        f.run(
            &run,
            Some(id),
            None,
            vec![
                user(vec![text("start")]),
                assistant(vec![text(&format!("assistant-only-{id} report {id}"))]),
            ],
        );
    }
    f.human(
        "run-900",
        "initial_prompt",
        "please migrate the billing schema",
    );
    f.store()
        .handle
        .append_run_ledger("run-901", "decided: keep the ledger format")
        .unwrap();
    let ids = |value: &Value| -> Vec<String> {
        value["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|thread| thread["thread_id"].as_str().unwrap().to_owned())
            .collect()
    };
    let human = f
        .call(own, "thread_search", json!({"query": "billing migrate"}))
        .unwrap();
    assert_eq!(ids(&human), ["alpha"]);
    assert_eq!(human["threads"][0]["snippets"][0]["kind"], "user");
    assert_eq!(human["threads"][0]["matched_runs"], json!(["run-900"]));
    let note = f
        .call(
            own,
            "thread_search",
            json!({"query": ["nothing", "ledger format"], "match": "phrase"}),
        )
        .unwrap();
    assert_eq!(ids(&note), ["beta"]);
    let report = f
        .call(
            own,
            "thread_search",
            json!({"query": "report gamma", "kinds": ["final_report"]}),
        )
        .unwrap();
    assert_eq!(ids(&report), ["gamma"]);
    assert_eq!(
        f.call(
            own,
            "thread_search",
            json!({"query": "report gamma", "kinds": ["title", "user"]})
        )
        .unwrap()["threads"],
        json!([])
    );
    // Assistant text that is not a final report is not metadata.
    let assistant_only = f
        .call(own, "thread_search", json!({"query": "start"}))
        .unwrap();
    assert_eq!(assistant_only["threads"], json!([]));
    let title = f
        .call(
            own,
            "thread_search",
            json!({"query": "^title of (alpha|gamma)$", "match": "regex"}),
        )
        .unwrap();
    assert_eq!(ids(&title), ["gamma", "alpha"]);
    let first = f.call(own, "thread_search", json!({"limit": 2})).unwrap();
    assert_eq!(ids(&first), ["gamma", "beta"]);
    let cursor = first["next_cursor"].as_str().unwrap();
    let second = f
        .call(own, "thread_search", json!({"limit": 2, "cursor": cursor}))
        .unwrap();
    assert_eq!(ids(&second), ["alpha", "own"]);
    assert_eq!(second["next_cursor"], Value::Null);
    let recent = f
        .call(own, "thread_search", json!({"updated_after": 10}))
        .unwrap();
    assert_eq!(ids(&recent), ["gamma", "beta"]);
}

#[test]
fn search_cursors_reach_every_thread_beyond_two_thousand_candidates() {
    let f = Fixture::new();
    let own = f.own();
    let rows: Vec<_> = (0..2105)
        .map(|n| ThreadUpsert {
            thread_id: format!("page-{n:04}"),
            project_root: Some(Fixture::canonical(f.project.path())),
            title: Some(if n == 0 {
                "oldest needle".into()
            } else {
                "ordinary".into()
            }),
            // Equal timestamps also exercise the thread ID tie-breaker.
            created_at_ns: 2,
            updated_at_ns: 2,
            ..Default::default()
        })
        .collect();
    f.store().handle.upsert_threads_host(&rows).unwrap();
    let mut args = json!({"limit": 20});
    let mut seen = std::collections::HashSet::new();
    loop {
        let page = f.call(own, "thread_search", args.clone()).unwrap();
        for row in page["threads"].as_array().unwrap() {
            assert!(seen.insert(row["thread_id"].as_str().unwrap().to_owned()));
        }
        if page["next_cursor"].is_null() {
            break;
        }
        assert_ne!(page["next_cursor"], args["cursor"]);
        args["cursor"] = page["next_cursor"].clone();
        assert!(seen.len() <= 2106);
    }
    assert_eq!(seen.len(), 2106);
    assert!(seen.contains("page-0000") && seen.contains("own"));
    let mut args = json!({"query": "oldest needle", "match": "phrase", "kinds": ["title"]});
    let mut found = false;
    let mut exhausted = false;
    for _ in 0..30 {
        let page = f.call(own, "thread_search", args.clone()).unwrap();
        found |= page["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["thread_id"] == "page-0000");
        if page["next_cursor"].is_null() {
            exhausted = true;
            break;
        }
        assert_ne!(page["next_cursor"], args["cursor"]);
        args["cursor"] = page["next_cursor"].clone();
    }
    assert!(
        found && exhausted,
        "bounded scans must cover the whole project"
    );
}

#[test]
fn cursors_cannot_probe_invisible_or_missing_threads_or_runs() {
    let f = Fixture::new();
    let own = f.own();
    f.thread("foreign", Some(f.other.path()), "run-200", |_| {});
    f.run(
        "run-200",
        Some("foreign"),
        None,
        vec![user(vec![text("foreign")])],
    );
    f.thread("foreign-older", Some(f.other.path()), "run-201", |row| {
        row.updated_at_ns = 2;
    });
    f.run(
        "run-201",
        Some("foreign-older"),
        None,
        vec![user(vec![text("older")])],
    );
    let foreign = ("run-200", Some("foreign"), Some(f.other.path()));
    let first = f
        .call(foreign, "thread_search", json!({"limit": 1}))
        .unwrap();
    let token = first["next_cursor"].as_str().unwrap();
    let missing = format!("v1:0:{}", i64::MAX);
    // Neither an authentic foreign cursor nor an old/missing position grants access.
    for who in [("run-100", None, None), own] {
        let hidden_error = f
            .call(who, "thread_search", json!({"cursor": token}))
            .unwrap_err();
        let missing_error = f
            .call(who, "thread_search", json!({"cursor": missing}))
            .unwrap_err();
        assert_eq!(hidden_error, missing_error);
        assert!(hidden_error.starts_with("thread_cursor_invalid: pass next_cursor unchanged"));
    }
    assert!(
        f.call(foreign, "thread_search", json!({"cursor": token}))
            .is_ok()
    );
    // Keep the caller identity but revoke the position's project visibility.
    f.thread("foreign-older", Some(f.project.path()), "run-201", |_| {});
    assert!(
        f.call(foreign, "thread_search", json!({"cursor": token}))
            .is_err()
    );
    for cursor in ["run-200#0.0@0", "run-999#0.0@0"] {
        assert_eq!(
            f.call(
                own,
                "thread_read",
                json!({"thread": "own", "view": "messages", "cursor": cursor})
            )
            .unwrap_err(),
            "thread_cursor_invalid: pass next_cursor with the same thread, run and around"
        );
    }
    // grep's cursor is only a numeric offset, never a database/run reference;
    // the explicit thread target still passes the common visibility gate.
    for cursor in ["run-200#0.0@0", "run-999#0.0@0"] {
        assert_eq!(
            f.call(
                own,
                "thread_grep",
                json!({"thread": "own", "pattern": "x", "cursor": cursor})
            )
            .unwrap_err(),
            "thread_cursor_invalid: pass next_cursor unchanged"
        );
    }
    for tool in ["thread_read", "thread_grep"] {
        for id in ["foreign", "missing"] {
            let args = if tool == "thread_read" {
                json!({"thread": id, "view": "messages", "cursor": "run-200#0.0@0"})
            } else {
                json!({"thread": id, "pattern": "x", "cursor": "0"})
            };
            assert_eq!(f.call(own, tool, args).unwrap_err(), denial(id));
        }
    }
}

#[test]
fn invisible_lineage_candidates_never_become_search_cursor_positions() {
    let f = Fixture::new();
    let own = f.own();
    let rows: Vec<_> = (0..110)
        .map(|n| ThreadUpsert {
            thread_id: format!("hidden-fork-{n}"),
            parent_thread_id: Some("own".into()),
            lineage_kind: Some(ThreadLineageKind::Fork),
            project_root: Some(Fixture::canonical(f.other.path())),
            updated_at_ns: 2,
            ..Default::default()
        })
        .collect();
    f.store().handle.upsert_threads_host(&rows).unwrap();
    let page = f
        .call(own, "thread_search", json!({"query": "no match"}))
        .unwrap();
    assert_eq!(page["threads"], json!([]));
    assert!(
        page["next_cursor"].is_null(),
        "invisible candidates must not emit a position: {page}"
    );
}

#[test]
fn a_thread_less_cli_caller_sees_only_its_project() {
    let f = Fixture::new();
    f.thread("inside", Some(f.project.path()), "run-1000", |_| {});
    f.run(
        "run-1000",
        Some("inside"),
        None,
        vec![user(vec![text("in")])],
    );
    f.thread("outside", Some(f.other.path()), "run-1001", |_| {});
    f.run(
        "run-1002",
        None,
        None,
        vec![user(vec![text("cli asks about outside")])],
    );
    f.human("run-1002", "initial_prompt", "read outside");
    let cli = ("run-1002", None, Some(f.project.path()));
    assert!(
        f.call(cli, "thread_read", json!({"thread": "inside"}))
            .is_ok()
    );
    assert_eq!(
        f.call(cli, "thread_read", json!({"thread": "outside"}))
            .unwrap_err(),
        denial("outside")
    );
    let unknown_project = ("run-1002", None, None);
    assert_eq!(
        f.call(unknown_project, "thread_read", json!({"thread": "inside"}))
            .unwrap_err(),
        denial("inside")
    );
}

#[tokio::test]
async fn thread_argument_errors_are_redacted_and_bounded_through_the_agent_loop() {
    let secret = format!("ghp_{}", "B".repeat(40));
    let huge = format!("{secret} {}", "界".repeat(10_000));
    let inputs = [
        ("thread_read", json!({"thread": "own", huge.clone(): 1})),
        ("thread_read", json!({"thread": "own", "view": huge})),
        ("thread_search", json!({"match": huge})),
        (
            "thread_grep",
            json!({"thread": "own", "pattern": "x", "kinds": [huge]}),
        ),
    ];
    let script = inputs
        .iter()
        .enumerate()
        .map(|(n, (name, input))| {
            reply(
                vec![ContentBlock::ToolUse {
                    id: format!("bad-{n}"),
                    name: (*name).into(),
                    input: input.clone(),
                }],
                FinishReason::ToolUse,
            )
        })
        .collect();
    let f = Fixture::with_script(script);
    let mut events = f.runtime.shared.bus.subscribe();
    let run = f
        .runtime
        .delegate_chat(
            "caller",
            Role::Worker,
            "test invalid arguments".into(),
            RunConfig {
                project_root: Some(f.project.path().into()),
                ..Default::default()
            },
        )
        .unwrap();
    f.runtime.wait(run).await.unwrap();
    let record = f.store().restore_record(run).unwrap().unwrap();
    let messages: Vec<Message> = serde_json::from_str(&record.messages_json).unwrap();
    let mut count = 0;
    for block in messages.iter().flat_map(|m| &m.content) {
        if let ContentBlock::ToolResult {
            content, is_error, ..
        } = block
        {
            assert!(is_error);
            let text: String = content
                .iter()
                .map(|p| p.text_or_image_note().into_owned())
                .collect();
            assert!(text.len() <= MAX_OUTPUT_BYTES, "{}", text.len());
            assert!(!text.contains(&secret));
            count += 1;
        }
    }
    assert_eq!(count, inputs.len());
    let mut completed = 0;
    while let Ok(Ok(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
    {
        if let event_bus::EventKind::Tool(event_bus::ToolEvent::ToolCompleted {
            output: Some(output),
            ..
        }) = event.kind
        {
            assert!(output.len() <= MAX_OUTPUT_BYTES);
            assert!(!output.contains(&secret));
            completed += 1;
        }
    }
    assert_eq!(completed, inputs.len());
    // The handler's serde path is bounded too, even if no preflight ran.
    let parse_error = super::parse::<read::ReadArgs>(json!({"thread": "own", "view": huge}))
        .err()
        .unwrap();
    assert!(super::error(parse_error).result.content.len() <= MAX_OUTPUT_BYTES);
    let other = tools::ToolResult::error(huge.clone());
    assert_eq!(super::bound_result("read", other.clone()), other);
}

#[tokio::test]
async fn a_chat_run_reads_a_sibling_thread_through_the_registered_tools() {
    let tool = |id: &str, name: &str, input: Value| {
        reply(
            vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            }],
            FinishReason::ToolUse,
        )
    };
    let f = Fixture::with_script(vec![
        tool(
            "t1",
            "thread_read",
            json!({"thread": "sibling", "view": "messages"}),
        ),
        tool(
            "t2",
            "thread_grep",
            json!({"thread": "foreign", "pattern": "x"}),
        ),
        tool("t3", "thread_search", json!({"query": "sibling"})),
        reply(vec![text("done")], FinishReason::Stop),
    ]);
    f.thread("sibling", Some(f.project.path()), "run-1100", |_| {});
    f.run(
        "run-1100",
        Some("sibling"),
        None,
        vec![user(vec![text("sibling-content")])],
    );
    f.thread("foreign", Some(f.other.path()), "run-1101", |_| {});
    let mut events = f.runtime.shared.bus.subscribe();
    let run = f
        .runtime
        .delegate_chat(
            "caller",
            Role::Worker,
            "compare with the sibling thread".into(),
            RunConfig {
                project_root: Some(f.project.path().into()),
                ..Default::default()
            },
        )
        .unwrap();
    f.runtime.wait(run).await.unwrap();
    let record = f.store().restore_record(run).unwrap().unwrap();
    let messages: Vec<Message> = serde_json::from_str(&record.messages_json).unwrap();
    let results: Vec<(String, bool)> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((
                content
                    .iter()
                    .map(|part| part.text_or_image_note().into_owned())
                    .collect::<String>(),
                *is_error,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 3, "{results:?}");
    assert!(
        !results[0].1 && results[0].0.contains("sibling-content"),
        "{results:?}"
    );
    assert!(results[0].0.contains(UNTRUSTED_NOTE));
    assert!(
        results[1].1 && results[1].0.contains(&denial("foreign")),
        "{results:?}"
    );
    assert!(
        !results[2].1 && results[2].0.contains("\"sibling\""),
        "{results:?}"
    );
    // Reads of other threads are shown to the user as tool calls.
    let mut started = Vec::new();
    let mut completed = Vec::new();
    while let Ok(Ok(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
    {
        match event.kind {
            event_bus::EventKind::Tool(event_bus::ToolEvent::ToolStarted { tool_name, .. }) => {
                started.push(tool_name);
            }
            event_bus::EventKind::Tool(event_bus::ToolEvent::ToolCompleted {
                tool_name,
                is_error,
                ..
            }) => completed.push((tool_name, is_error)),
            _ => {}
        }
    }
    assert_eq!(started, ["thread_read", "thread_grep", "thread_search"]);
    assert_eq!(
        completed,
        [
            ("thread_read".to_owned(), false),
            ("thread_grep".to_owned(), true),
            ("thread_search".to_owned(), false)
        ]
    );
}

#[tokio::test]
async fn escalation_lineage_recorded_by_the_runtime_is_readable_both_ways() {
    let f = Fixture::new();
    let source = f
        .runtime
        .delegate_chat(
            "source",
            Role::Worker,
            "source-request".into(),
            RunConfig::default(),
        )
        .unwrap();
    f.runtime.wait(source).await.unwrap();
    let escalated = f
        .runtime
        .spawn_escalated_root(
            crate::EscalationMemo {
                source_run_id: source,
                original_request: "source-request".into(),
                findings: vec![],
                files_touched: vec![],
                blockers: vec![],
                workspace_state: String::new(),
                escalation_reason: "escalation-reason".into(),
                suggested_next: "continue".into(),
            },
            &RunConfig::default(),
            Default::default(),
            || {},
        )
        .unwrap();
    f.runtime.wait(escalated).await.unwrap();
    let child = event_bus::escalation_thread_id(&escalated.to_string());
    // Neither side has a project, so only the recorded lineage grants access.
    let from_source = (source.to_string(), Some("source"), None);
    let from_child = (escalated.to_string(), Some(child.as_str()), None);
    let read = |who: &(String, Option<&str>, Option<&Path>), thread: &str| {
        f.call(
            (who.0.as_str(), who.1, who.2),
            "thread_read",
            json!({"thread": thread, "view": "messages", "max_bytes": 16384}),
        )
    };
    let down = read(&from_source, &child).unwrap();
    assert_eq!(down["run_id"], escalated.to_string(), "{down}");
    assert!(down.to_string().contains("escalation-reason"), "{down}");
    let up = read(&from_child, "source").unwrap();
    assert_eq!(up["run_id"], source.to_string(), "{up}");
    assert!(up.to_string().contains("source-request"), "{up}");
    f.thread("unrelated", None, "run-1200", |_| {});
    for who in [&from_source, &from_child] {
        assert_eq!(read(who, "unrelated").unwrap_err(), denial("unrelated"));
    }
}

#[test]
fn overlong_thread_arguments_fail_without_echoing_or_probing() {
    let f = Fixture::new();
    let own = f.own();
    let long = "x".repeat(super::MAX_THREAD_ARG_BYTES + 1);
    for (tool, input) in [
        ("thread_read", json!({"thread": long})),
        ("thread_grep", json!({"thread": long, "pattern": "a"})),
    ] {
        let error = f.call(own, tool, input).unwrap_err();
        assert!(error.starts_with("thread_args_invalid:"), "{error}");
        assert!(!error.contains(&long), "{tool}");
    }
    let exact = "x".repeat(super::MAX_THREAD_ARG_BYTES);
    assert_eq!(
        f.call(own, "thread_read", json!({"thread": exact}))
            .unwrap_err(),
        denial(&exact)
    );
}

#[path = "search_tests.rs"]
mod search_phase_b;
