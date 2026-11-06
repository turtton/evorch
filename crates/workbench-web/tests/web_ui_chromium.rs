//! Browser end-to-end test of the embedded web workbench (ADR 0045 P6).
//!
//! A scripted backend shaped like the host serves one project with two
//! threads, a long committed history, questions (one routed to a lead) and a
//! tool approval. Like the host it answers a subscription with one cut-short
//! snapshot page, uses opaque revisions that can go down and forgets a list
//! cursor. Headless Chromium then drives the real front end through the real
//! adapter: list (snapshot completed by queries, expired cursor) -> thread
//! (history paging, Markdown, a long body, read-only lead question) -> send
//! (IME guard, receipts) -> answer -> deny (approving needs the full input,
//! which the protocol does not carry yet) -> core restart prompt.
//! The JavaScript unit tests (tests/web/unit.mjs) run in the same browser.
//!
//! Ignored by default because it needs Chrome or Chromium (`CHROME` names the
//! executable; otherwise the usual locations are searched). The CI
//! `browser-e2e` job runs it with `--run-ignored only`. Every wait is on a
//! DOM mutation (MutationObserver) or on a command the backend recorded;
//! nothing sleeps.
//!
//! Set `WORKBENCH_WEB_SCREENSHOT_DIR` to keep PNG screenshots of the main
//! screens (mobile light, mobile dark, desktop).

mod common;

use chromiumoxide::cdp::browser_protocol::emulation::{
    MediaFeature, SetDeviceMetricsOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::handler::viewport::Viewport;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use workbench_protocol::envelope::{Provenance, ReplyEnvelope, RequestEnvelope};
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::events::{EventEnvelope, Subscribe};
use workbench_protocol::handshake::ServerHello;
use workbench_protocol::ids::{BodyId, ClientId};
use workbench_protocol::views::BodyChunk;
use workbench_protocol::wire::{ByteOffset, InstanceEpoch};
use workbench_web::{
    BackendSubscription, StaticAsset, UI_ASSETS, WebServer, WebServerConfig, WebSession,
    WorkbenchBackend,
};

const JS: &str = "text/javascript; charset=utf-8";
const HISTORY_LEN: usize = 230;
const QUESTION_TITLE: &str = "本番 DB のマイグレーションも実行してよいですか？";
const LANE_QUESTION_TITLE: &str = "テストのタイムアウト値はいくつにしますか？";
const FREE_QUESTION_TITLE: &str = "回帰テストの対象ブラウザを教えてください。";
const SENT_TEXT: &str = "修正案 A で進めてください。テストも追加して。";

fn body_text() -> String {
    format!(
        "長い本文の先頭です。\n\n{}\n\n本文の終わり。",
        "lorem ipsum ".repeat(300)
    )
}

fn now_millis() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .to_string()
}

fn history() -> Vec<Value> {
    let markdown = "## 調査結果\n\n`login.rs` の **redirect** 処理が原因です。\n\n- 修正案 A: 戻り先を検証する\n- 修正案 B: 既定の戻り先を使う\n\n詳細: https://example.com/docs と [設計書](https://example.com/design)。\n\n<img src=x onerror=alert(1)> は表示されません。[危険](javascript:alert(1))\n\n```rust\nfn redirect_target() -> &'static str { \"/home\" }\n```";
    (0..HISTORY_LEN)
        .map(|index| {
            let (role, content) = match index {
                226 => ("user", json!({"type": "inline", "payload": {"text": "ログイン後のリダイレクト先が壊れています。直してください。"}})),
                227 => ("assistant", json!({"type": "inline", "payload": {"text": markdown}})),
                228 => ("tool", json!({"type": "inline", "payload": {"text": "running 12 tests\ntest result: ok. 12 passed; 0 failed"}})),
                229 => ("assistant", json!({"type": "body", "payload": {"body_id": "body-1", "total_bytes": body_text().len().to_string()}})),
                i if i % 3 == 0 => ("user", json!({"type": "inline", "payload": {"text": format!("メッセージ {i}")}})),
                i => ("assistant", json!({"type": "inline", "payload": {"text": format!("メッセージ {i}")}})),
            };
            json!({"thread": "thr-1", "run": "run-root", "index": index.to_string(), "role": role, "content": content})
        })
        .collect()
}

struct World {
    epoch: u64,
    sequence: u64,
    projects: Vec<Value>,
    threads: Vec<Value>,
    runs: Vec<Value>,
    questions: Vec<Value>,
    approvals: Vec<Value>,
    history: Vec<Value>,
    subscribers: Vec<(String, mpsc::Sender<EventEnvelope>)>,
    subscriptions: u64,
    commands: Vec<Value>,
    /// Thread-list cursors the core has "forgotten": the first use of each
    /// is refused with `conflict`, as an expired host cursor is.
    expired_cursors: Vec<String>,
    conflicts: usize,
}

impl World {
    fn new() -> Self {
        let now = now_millis();
        let agent = |run: &str| json!({"actor": "agent", "surface": "none", "agent_run_id": run});
        Self {
            epoch: 7,
            sequence: 40,
            projects: vec![
                json!({"id": "proj-1", "display_name": "evorch", "thread_count": 2, "revision": "1"}),
                json!({"id": "proj-2", "display_name": "intent-cli", "thread_count": 0, "revision": "1"}),
            ],
            threads: vec![
                json!({"id": "thr-1", "project": "proj-1", "title": "ログイン画面のリダイレクトを直す", "state": "waiting_for_user", "revision": "15802293917013211136", "updated_at": now}),
                json!({"id": "thr-2", "project": "proj-1", "title": "レーン: 回帰テストを追加", "state": "running", "lead_thread": "thr-1", "revision": "77", "updated_at": now}),
            ],
            runs: vec![
                json!({"id": "run-root", "thread": "thr-1", "role": "orchestrator", "phase": "waiting_for_user", "revision": "1"}),
                json!({"id": "run-w1", "thread": "thr-1", "parent_run": "run-root", "role": "worker", "phase": "waiting_for_approval", "revision": "1"}),
                json!({"id": "run-l1", "thread": "thr-2", "role": "worker", "phase": "running", "revision": "1"}),
            ],
            questions: vec![
                json!({
                    "id": "q-1",
                    "requester": {"run": "run-root", "thread": "thr-1", "root_run": "run-root"},
                    "target": {"type": "user"},
                    "management_revision": "1", "routing_revision": "1",
                    "blocking": true, "title": QUESTION_TITLE, "options": ["はい", "いいえ"],
                    "revision": "9000", "raised_by": agent("run-root"),
                }),
                json!({
                    "id": "q-2",
                    "requester": {"run": "run-l1", "thread": "thr-2", "root_run": "run-l1"},
                    "target": {"type": "lead", "payload": {"lead_thread": "thr-1", "lead_run": "run-root"}},
                    "management_revision": "1", "routing_revision": "1",
                    "blocking": false, "title": LANE_QUESTION_TITLE, "options": [],
                    "revision": "1", "raised_by": agent("run-l1"),
                }),
                json!({
                    "id": "q-3",
                    "requester": {"run": "run-l1", "thread": "thr-2", "root_run": "run-l1"},
                    "target": {"type": "user"},
                    "management_revision": "1", "routing_revision": "1",
                    "blocking": true, "title": FREE_QUESTION_TITLE, "options": [],
                    "revision": "31337", "raised_by": agent("run-l1"),
                }),
            ],
            approvals: vec![json!({
                "id": "ap-1", "core_epoch": "7", "revision": "77777", "thread": "thr-1", "run": "run-w1",
                "tool_name": "shell",
                "summary": "$ cargo nextest run -p workbench-web\n$ git add crates/auth/src/login.rs\n$ git commit -S -m 'fix(auth): validate redirect target'\n$ git push origin fix/login-redirect\n# 4 commands in /home/user/src/evorch\n# network: github.com:443\n# writes: crates/auth/src/login.rs\n# timeout: 10 minutes",
                "state": "pending",
            })],
            history: history(),
            subscribers: Vec::new(),
            subscriptions: 0,
            commands: Vec::new(),
            expired_cursors: vec!["t1".to_owned()],
            conflicts: 0,
        }
    }

    fn publish(&mut self, event: Value) {
        self.sequence += 1;
        let (epoch, sequence) = (self.epoch, self.sequence);
        self.subscribers.retain(|(id, tx)| {
            let envelope: EventEnvelope = serde_json::from_value(json!({
                "subscription_id": id,
                "cursor": {"core_epoch": epoch.to_string(), "sequence": sequence.to_string()},
                "event": event,
            }))
            .unwrap();
            tx.try_send(envelope).is_ok()
        });
    }

    fn find<'a>(items: &'a mut [Value], id: &str) -> Option<&'a mut Value> {
        items.iter_mut().find(|item| item["id"] == id)
    }
}

fn page(items: Vec<Value>, cursor: Option<String>) -> Value {
    let mut page = json!({"items": items, "snapshot_revision": "1"});
    if let Some(cursor) = cursor {
        page["next_cursor"] = json!(cursor);
    }
    page
}

fn query_reply(kind: &str, payload: Value) -> Value {
    json!({"status": "success", "payload": {"type": "query", "payload": {"type": kind, "payload": payload}}})
}

fn accepted() -> Value {
    json!({"status": "success", "payload": {"type": "command", "payload": {"disposition": "accepted"}}})
}

fn refused(code: &str, retry: &str) -> Value {
    json!({"status": "error", "payload": {"code": code, "message": "scripted refusal", "retry": retry}})
}

/// A backend with just enough state to serve the screens and apply commands.
struct UiBackend {
    world: Mutex<World>,
    commands: watch::Sender<usize>,
}

impl UiBackend {
    fn new() -> Self {
        Self {
            world: Mutex::new(World::new()),
            commands: watch::channel(0).0,
        }
    }

    /// The first recorded command of `kind`, waiting until one arrives.
    async fn command(&self, kind: &str) -> Value {
        let mut changes = self.commands.subscribe();
        loop {
            if let Some(found) = self
                .world
                .lock()
                .unwrap()
                .commands
                .iter()
                .find(|c| c["type"] == kind)
                .cloned()
            {
                return found;
            }
            changes.changed().await.unwrap();
        }
    }

    fn count(&self, kind: &str) -> usize {
        let world = self.world.lock().unwrap();
        world.commands.iter().filter(|c| c["type"] == kind).count()
    }

    /// The core restarts: a new epoch, and open subscriptions are closed.
    fn restart(&self) {
        let mut world = self.world.lock().unwrap();
        world.publish(
            json!({"type": "subscription_closed", "payload": {"reason": "core_shutting_down"}}),
        );
        world.subscribers.clear();
        world.epoch += 1;
        world.sequence = 0;
        for approval in &mut world.approvals {
            approval["state"] = json!("expired");
        }
    }

    fn answer(&self, request: &Value, provenance: &Provenance) -> Value {
        let mut world = self.world.lock().unwrap();
        let p = &request["payload"]["payload"];
        let command = p.clone();
        let kind = request["payload"]["type"].as_str().unwrap().to_owned();
        let reply = match kind.as_str() {
            "send_chat" => {
                let index = world.history.len().to_string();
                let receipt = |state: &str| json!({"submission_id": p["submission_id"], "thread": p["thread"], "run": "run-root", "state": state, "revision": if state == "accepted" { "1" } else { "2" }, "sent_by": serde_json::to_value(provenance).unwrap()});
                world.publish(json!({"type": "input_receipt_changed", "payload": {"receipt": receipt("accepted")}}));
                let message = json!({"thread": p["thread"], "run": "run-root", "index": index, "role": "user", "content": {"type": "inline", "payload": {"text": p["text"]}}});
                world.history.push(message.clone());
                world
                    .publish(json!({"type": "message_committed", "payload": {"message": message}}));
                world.publish(json!({"type": "input_receipt_changed", "payload": {"receipt": receipt("delivered")}}));
                let mut reply = accepted();
                reply["payload"]["payload"]["submission_id"] = p["submission_id"].clone();
                reply
            }
            "answer_user_question" => {
                let id = p["question_id"].as_str().unwrap().to_owned();
                let answered_by = serde_json::to_value(provenance).unwrap();
                let question = World::find(&mut world.questions, &id).unwrap();
                if question["revision"] != p["question_revision"] {
                    refused("stale_revision", "after_resync")
                } else if !question["answer"].is_null() {
                    refused("already_resolved", "never")
                } else {
                    question["answer"] = json!({"text": p["answer"], "answered_by": answered_by});
                    // Revisions are content hashes in the host: the new one
                    // may well be smaller, as here.
                    question["revision"] = json!("12");
                    let question = question.clone();
                    world.publish(
                        json!({"type": "question_changed", "payload": {"question": question}}),
                    );
                    accepted()
                }
            }
            "decide_tool_approval" => {
                let epoch = world.epoch.to_string();
                let id = p["approval_id"].as_str().unwrap().to_owned();
                let decided_by = serde_json::to_value(provenance).unwrap();
                let approval = World::find(&mut world.approvals, &id).unwrap();
                if p["core_epoch"] != epoch.as_str() {
                    refused("stale_epoch", "after_reconnect")
                } else if approval["revision"] != p["approval_revision"] {
                    refused("stale_revision", "after_resync")
                } else {
                    approval["state"] = json!(if p["decision"] == "approve" {
                        "approved"
                    } else {
                        "denied"
                    });
                    approval["revision"] = json!("5");
                    approval["decided_by"] = decided_by;
                    let approval = approval.clone();
                    world.publish(
                        json!({"type": "approval_changed", "payload": {"approval": approval}}),
                    );
                    accepted()
                }
            }
            _ => accepted(),
        };
        world
            .commands
            .push(json!({"type": kind, "payload": command, "request_id": request["request_id"]}));
        let count = world.commands.len();
        drop(world);
        self.commands.send_replace(count);
        reply
    }

    fn query(&self, request: &Value) -> Value {
        let mut world = self.world.lock().unwrap();
        let kind = request["payload"]["type"].as_str().unwrap();
        let p = &request["payload"]["payload"];
        let thread_filter = |item: &Value, key: &str| {
            p["thread"].is_null() || item.pointer(key) == Some(&p["thread"])
        };
        match kind {
            "list_projects" => query_reply("projects", page(world.projects.clone(), None)),
            "list_threads" => {
                // One thread per page, so listings need their cursors.
                let threads: Vec<Value> = world
                    .threads
                    .iter()
                    .filter(|t| t["project"] == p["project"])
                    .cloned()
                    .collect();
                let cursor = p["page"]["cursor"].as_str().map(str::to_owned);
                if let Some(cursor) = &cursor
                    && let Some(at) = world.expired_cursors.iter().position(|c| c == cursor)
                {
                    world.expired_cursors.remove(at);
                    world.conflicts += 1;
                    return refused("conflict", "after_resync");
                }
                let start =
                    cursor.map_or(0, |c| c.trim_start_matches('t').parse::<usize>().unwrap());
                let end = (start + 1).min(threads.len());
                let next = (end < threads.len()).then(|| format!("t{end}"));
                query_reply("threads", page(threads[start..end].to_vec(), next))
            }
            "get_thread" => match world.threads.iter().find(|t| t["id"] == p["thread"]) {
                Some(summary) => query_reply(
                    "thread",
                    json!({"summary": summary, "root_run": "run-root", "active_run": "run-root", "pending_questions": 1, "pending_approvals": 1, "queued_follow_ups": 0}),
                ),
                None => refused("not_found", "never"),
            },
            "list_runs" => query_reply(
                "runs",
                page(
                    world
                        .runs
                        .iter()
                        .filter(|r| r["thread"] == p["thread"])
                        .cloned()
                        .collect(),
                    None,
                ),
            ),
            "list_questions" => query_reply(
                "questions",
                page(
                    world
                        .questions
                        .iter()
                        .filter(|q| thread_filter(q, "/requester/thread"))
                        .filter(|q| !(p["pending_only"] == true && !q["answer"].is_null()))
                        .cloned()
                        .collect(),
                    None,
                ),
            ),
            "list_approvals" => query_reply(
                "approvals",
                page(
                    world
                        .approvals
                        .iter()
                        .filter(|a| thread_filter(a, "/thread"))
                        .filter(|a| !(p["pending_only"] == true && a["state"] != "pending"))
                        .cloned()
                        .collect(),
                    None,
                ),
            ),
            "read_committed_history" => {
                let start = p["page"]["cursor"]
                    .as_str()
                    .map_or(0, |c| c.trim_start_matches('h').parse::<usize>().unwrap());
                let limit = p["page"]["limit"].as_u64().unwrap() as usize;
                let end = (start + limit).min(world.history.len());
                let next = (end < world.history.len()).then(|| format!("h{end}"));
                query_reply(
                    "committed_history",
                    page(world.history[start..end].to_vec(), next),
                )
            }
            "read_body" => {
                let offset = p["offset"].as_str().unwrap().parse().unwrap();
                let chunk = BodyChunk::slice(
                    BodyId::new("body-1").unwrap(),
                    &body_text(),
                    ByteOffset(offset),
                    p["max_bytes"].as_u64().unwrap() as u32,
                )
                .unwrap();
                query_reply("body", serde_json::to_value(chunk).unwrap())
            }
            "get_command_receipt" => query_reply(
                "command_receipt",
                json!({"request_id": p["request_id"], "state": {"type": "unknown"}}),
            ),
            _ => refused("unsupported_capability", "never"),
        }
    }

    /// The first snapshot page. Like the host's (one default-size page), it
    /// does not hold everything: the lane thread, the questions and the
    /// approvals are only reachable with list queries.
    fn snapshot_items(world: &World) -> Vec<Value> {
        let mut items = vec![
            json!({"type": "core_status", "payload": serde_json::to_value(common::core_status()).unwrap()}),
        ];
        items.extend(
            world
                .projects
                .iter()
                .map(|p| json!({"type": "project", "payload": p})),
        );
        items.extend(
            world
                .threads
                .iter()
                .filter(|t| t["lead_thread"].is_null())
                .map(|t| json!({"type": "thread", "payload": t})),
        );
        items.extend(
            world
                .runs
                .iter()
                .map(|r| json!({"type": "run", "payload": r})),
        );
        items
    }
}

impl WorkbenchBackend for UiBackend {
    async fn handle_request(
        &self,
        _session: &WebSession,
        request: RequestEnvelope,
        provenance: Provenance,
    ) -> ReplyEnvelope {
        let value = serde_json::to_value(&request).unwrap();
        let mut reply = match value["kind"].as_str() {
            Some("command") => self.answer(&value, &provenance),
            _ => self.query(&value),
        };
        reply["request_id"] = value["request_id"].clone();
        serde_json::from_value(reply).unwrap()
    }

    async fn subscribe(
        &self,
        _client: ClientId,
        _subscribe: Subscribe,
    ) -> Result<BackendSubscription, WorkbenchError> {
        let mut world = self.world.lock().unwrap();
        world.subscriptions += 1;
        let id = world.subscriptions.to_string();
        let (tx, rx) = mpsc::channel(256);
        world.subscribers.push((id.clone(), tx));
        // Always a fresh snapshot: a core may answer any resume this way.
        let started = json!({
            "subscription_id": id,
            "snapshot_id": format!("snap-{id}"),
            "high_water": {"core_epoch": world.epoch.to_string(), "sequence": world.sequence.to_string()},
            "initial_page": {
                "items": Self::snapshot_items(&world),
                "next_cursor": format!("snap-{id}-2"),
                "snapshot_revision": "1",
            },
        });
        Ok(BackendSubscription {
            started: serde_json::from_value(started).unwrap(),
            events: rx,
        })
    }

    async fn server_hello(&self) -> Result<ServerHello, WorkbenchError> {
        let mut hello = common::server_hello();
        hello.instance_epoch = InstanceEpoch(self.world.lock().unwrap().epoch);
        Ok(hello)
    }
}

/// The UI plus the JavaScript unit tests, laid out so that the relative
/// imports of tests/web/unit.mjs (`../../assets/js/*.js`) resolve.
fn assets_with_unit_tests() -> &'static [StaticAsset] {
    let mut assets: Vec<StaticAsset> = UI_ASSETS.to_vec();
    for asset in UI_ASSETS.iter().filter(|a| a.path.starts_with("js/")) {
        assets.push(StaticAsset {
            path: Box::leak(format!("assets/{}", asset.path).into_boxed_str()),
            ..*asset
        });
    }
    assets.extend([
        StaticAsset {
            path: "tests/web/unit.html",
            content_type: "text/html; charset=utf-8",
            body: include_bytes!("web/unit.html"),
        },
        StaticAsset {
            path: "tests/web/unit.mjs",
            content_type: JS,
            body: include_bytes!("web/unit.mjs"),
        },
        StaticAsset {
            path: "tests/web/unit-browser.mjs",
            content_type: JS,
            body: include_bytes!("web/unit-browser.mjs"),
        },
    ]);
    Box::leak(assets.into_boxed_slice())
}

// ---- browser helpers -----------------------------------------------------------

struct Driver {
    page: Page,
    screenshots: Option<PathBuf>,
}

impl Driver {
    async fn eval(&self, expression: &str) -> Value {
        let params = EvaluateParams::builder()
            .expression(expression)
            .await_promise(true)
            .return_by_value(true)
            .build()
            .unwrap();
        let result = self.page.evaluate_expression(params).await.unwrap();
        result.value().cloned().unwrap_or(Value::Null)
    }

    /// Wait until `predicate` (a JS function source) returns something truthy,
    /// re-checking on every DOM mutation.
    async fn wait(&self, predicate: &str) -> Value {
        self.eval(&format!(
            r##"new Promise((resolve) => {{
                const test = {predicate};
                let observer = null;
                const check = () => {{
                    let value;
                    try {{ value = test(); }} catch (error) {{ value = undefined; }}
                    if (!value) return false;
                    if (observer) observer.disconnect();
                    resolve(value instanceof Node ? true : value);
                    return true;
                }};
                if (!check()) {{
                    observer = new MutationObserver(check);
                    observer.observe(document, {{ subtree: true, childList: true, attributes: true, characterData: true }});
                }}
            }})"##
        ))
        .await
    }

    async fn click(&self, selector: &str) {
        // Center the target first: chromiumoxide only scrolls it into view,
        // which can leave it under the sticky top bar or composer.
        self.eval(&format!(
            "document.querySelector({selector:?}).scrollIntoView({{block: 'center'}}); true"
        ))
        .await;
        self.page
            .find_element(selector)
            .await
            .unwrap_or_else(|error| panic!("{selector}: {error}"))
            .click()
            .await
            .unwrap_or_else(|error| panic!("click {selector}: {error}"));
    }

    /// Type like an IME commit: focus, then insert the composed text at once.
    async fn insert_text(&self, selector: &str, text: &str) {
        self.eval(&format!(
            "document.querySelector({selector:?}).focus(); true"
        ))
        .await;
        self.page
            .execute(InsertTextParams::new(text))
            .await
            .unwrap();
    }

    async fn go(&self, hash: &str) {
        self.eval(&format!("location.hash = {hash:?}; true")).await;
    }

    async fn screenshot(&self, name: &str) {
        let Some(dir) = &self.screenshots else {
            return;
        };
        let png = self
            .page
            .screenshot(
                ScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Png)
                    .build(),
            )
            .await
            .unwrap();
        std::fs::write(dir.join(format!("{name}.png")), png).unwrap();
    }

    async fn color_scheme(&self, scheme: &str) {
        self.page
            .emulate_media_features(vec![MediaFeature::new("prefers-color-scheme", scheme)])
            .await
            .unwrap();
    }
}

const PROBE: &str = r##"
window.__problems = [];
document.addEventListener("securitypolicyviolation", (e) => window.__problems.push(`csp ${e.violatedDirective} ${e.blockedURI}`));
window.addEventListener("error", (e) => window.__problems.push(`error ${e.message}`));
window.addEventListener("unhandledrejection", (e) => window.__problems.push(`rejection ${e.reason}`));
"##;

async fn launch_browser(profile: &std::path::Path) -> Browser {
    let mut config = BrowserConfig::builder()
        .new_headless_mode()
        .user_data_dir(profile)
        .viewport(Viewport {
            width: 390,
            height: 844,
            device_scale_factor: Some(2.0),
            emulating_mobile: true,
            is_landscape: false,
            has_touch: true,
        })
        // Generous bounds for a loaded CI runner; nextest's slow-timeout is
        // what decides a hang.
        .launch_timeout(Duration::from_secs(120))
        .request_timeout(Duration::from_secs(300))
        .args([
            "no-first-run",
            "no-default-browser-check",
            "disable-background-networking",
            "disable-component-update",
            "disable-sync",
            "disable-extensions",
            "mute-audio",
            "lang=ja-JP",
        ]);
    if let Some(executable) = std::env::var_os("CHROME") {
        config = config.chrome_executable(executable);
    }
    if std::env::var_os("WORKBENCH_WEB_CHROME_NO_SANDBOX").is_some() {
        config = config.no_sandbox();
    }
    let (browser, mut handler) = Browser::launch(config.build().unwrap())
        .await
        .expect("launch Chrome or Chromium (set CHROME to its executable)");
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    browser
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chrome or Chromium; run with --run-ignored only (CI browser-e2e job)"]
async fn web_workbench_main_flows_in_chromium() {
    let backend = Arc::new(UiBackend::new());
    let server = WebServer::start_with_assets(
        WebServerConfig {
            enabled: true,
            bind: "127.0.0.1:0".parse().unwrap(),
            ..WebServerConfig::default()
        },
        Arc::clone(&backend),
        assets_with_unit_tests(),
    )
    .await
    .unwrap()
    .unwrap();
    let base = format!("http://127.0.0.1:{}", server.local_addr().port());
    let profile = tempfile::tempdir().unwrap();
    let mut browser = launch_browser(profile.path()).await;
    let screenshots = std::env::var_os("WORKBENCH_WEB_SCREENSHOT_DIR").map(PathBuf::from);
    if let Some(dir) = &screenshots {
        std::fs::create_dir_all(dir).unwrap();
    }

    // ---- JavaScript unit tests, DOM included ----
    let unit = Driver {
        page: browser
            .new_page(format!("{base}/tests/web/unit.html"))
            .await
            .unwrap(),
        screenshots: None,
    };
    let result = unit.wait("() => document.body.dataset.result").await;
    let result: Value = serde_json::from_str(result.as_str().unwrap()).unwrap();
    assert_eq!(result["failures"], json!([]), "{result:#}");
    assert_eq!(result["skipped"], 0, "{result:#}");
    assert!(result["passed"].as_u64().unwrap() > 20, "{result:#}");
    unit.page.close().await.unwrap();

    // ---- project list ----
    let page = browser.new_page("about:blank").await.unwrap();
    page.evaluate_on_new_document(PROBE).await.unwrap();
    let ui = Driver { page, screenshots };
    ui.page.goto(format!("{base}/")).await.unwrap();
    ui.wait(r##"() => document.querySelector('#conn[data-state="live"]')"##)
        .await;
    ui.wait(r##"() => document.querySelector('a.row[href="#/p/proj-1"]')?.textContent.includes("evorch")"##)
        .await;
    // The snapshot is one cut-short page: the badge says "partial" until the
    // lists were completed with queries (one of them with an expired cursor).
    ui.wait(r##"() => document.querySelector("#inbox-badge").textContent === "3""##)
        .await;
    assert_eq!(
        ui.eval(
            r##"document.querySelector(".attention-summary").textContent.includes("一部のみ")"##
        )
        .await,
        false
    );
    assert_eq!(
        backend.world.lock().unwrap().conflicts,
        1,
        "the expired thread cursor was hit and the listing started over"
    );
    ui.screenshot("01-projects").await;

    // ---- thread list ----
    ui.click(r##"a.row[href="#/p/proj-1"]"##).await;
    let badges = ui
        .wait(r##"() => document.querySelector('a.row[href="#/t/thr-1"] .row-badges')?.textContent || false"##)
        .await;
    assert_eq!(
        badges, "質問 1承認 1",
        "the lead-routed question is not counted"
    );
    let lane = ui
        .wait(r##"() => document.querySelector('a.row[href="#/t/thr-2"] .row-badges')?.textContent || false"##)
        .await;
    assert_eq!(lane, "質問 1");
    ui.screenshot("02-threads").await;

    // ---- thread: newest history, Markdown, long body ----
    ui.click(r##"a.row[href="#/t/thr-1"]"##).await;
    ui.wait(
        r##"() => document.querySelector(".messages")?.textContent.includes("本文の終わり。")"##,
    )
    .await;
    let shown = ui
        .eval(r##"document.querySelectorAll(".messages .msg").length"##)
        .await;
    assert_eq!(shown, 130, "the last two pages of 100 are on screen");
    let markdown = ui
        .eval(
            r##"(() => {
                const box = document.querySelector(".messages");
                const link = box.querySelector('a[href="https://example.com/design"]');
                return {
                    rel: link?.rel, target: link?.target,
                    images: box.querySelectorAll("img").length,
                    unsafe: box.querySelectorAll('a[href^="javascript"]').length,
                    literal: box.textContent.includes("<img src=x onerror=alert(1)>"),
                    heading: box.querySelector(".md-h2")?.textContent,
                    code: box.querySelector("pre code")?.textContent,
                    tool: box.querySelector(".msg-tool details summary")?.textContent,
                };
            })()"##,
        )
        .await;
    assert_eq!(
        markdown,
        json!({
            "rel": "noopener noreferrer", "target": "_blank", "images": 0, "unsafe": 0,
            "literal": true, "heading": "調査結果",
            "code": "fn redirect_target() -> &'static str { \"/home\" }",
            "tool": "ツール出力（2 行）",
        })
    );
    ui.screenshot("03-thread-bottom").await;
    ui.eval("window.scrollTo(0, 0); true").await;
    ui.wait(&format!(
        r##"() => document.querySelector(".thread .card.question")?.textContent.includes({QUESTION_TITLE:?})"##
    ))
    .await;
    let routed = ui
        .eval(
            r##"(() => {
                const card = document.querySelector(".thread .card.lead-routed");
                return card && {
                    text: card.textContent,
                    inputs: card.querySelectorAll("textarea, button").length,
                };
            })()"##,
        )
        .await;
    assert!(
        routed["text"]
            .as_str()
            .unwrap()
            .contains(LANE_QUESTION_TITLE)
            && routed["text"].as_str().unwrap().contains("Lead が対応中"),
        "{routed}"
    );
    assert_eq!(routed["inputs"], 0, "a lead-routed question is read-only");
    ui.screenshot("04-thread-top").await;
    ui.click("button.older").await;
    ui.wait(r##"() => document.querySelectorAll(".messages .msg").length === 230"##)
        .await;
    assert_eq!(
        ui.eval(r##"document.querySelector(".messages .msg .msg-body").textContent"##)
            .await,
        "メッセージ 0"
    );

    // ---- send: an open IME composition never sends ----
    ui.insert_text(".composer-input", SENT_TEXT).await;
    ui.eval(
        r##"document.querySelector(".composer-input").dispatchEvent(new KeyboardEvent("keydown", {
            key: "Enter", ctrlKey: true, isComposing: true, bubbles: true, cancelable: true })); true"##,
    )
    .await;
    assert_eq!(backend.count("send_chat"), 0);
    assert_eq!(
        ui.eval(r##"document.querySelector(".composer-input").value"##)
            .await,
        SENT_TEXT
    );
    ui.click("button.send").await;
    let sent = backend.command("send_chat").await;
    assert_eq!(sent["payload"]["text"], SENT_TEXT);
    assert_eq!(sent["payload"]["thread"], "thr-1");
    assert_eq!(sent["payload"]["project"], "proj-1");
    assert_eq!(
        sent["payload"]["composer_role"], "orchestrator",
        "the root run's role"
    );
    assert!(
        sent["payload"]["submission_id"]
            .as_str()
            .unwrap()
            .starts_with("sub-")
    );
    ui.wait(&format!(
        r##"() => [...document.querySelectorAll(".messages .msg-user")].some((m) => m.textContent.includes({SENT_TEXT:?}))
            && document.querySelector(".outbox").children.length === 0"##
    ))
    .await;
    assert_eq!(
        ui.eval(r##"document.querySelector(".composer-input").value"##)
            .await,
        "",
        "the composer is cleared after sending"
    );

    // ---- answer the thread's question with an option ----
    ui.click(r##"button[aria-label="「はい」と回答"]"##).await;
    let answer = backend.command("answer_user_question").await;
    assert_eq!(
        answer["payload"],
        json!({"thread": "thr-1", "question_id": "q-1", "answer": "はい", "question_revision": "9000"})
    );
    ui.wait(r##"() => !document.querySelector(".thread .card.question")"##)
        .await;

    // ---- inbox: the approval, a free-text question, the lead's question ----
    ui.go("#/inbox").await;
    ui.wait(r##"() => document.querySelector(".card.approval") && document.querySelector(".cards .card.question")"##)
        .await;
    let lead = ui
        .eval(
            r##"(() => {
                const section = document.querySelector(".lead-section");
                return { hidden: section.hidden, count: section.querySelector("summary").textContent,
                         answerable: section.querySelectorAll("textarea, .card button").length };
            })()"##,
        )
        .await;
    assert_eq!(
        lead,
        json!({"hidden": false, "count": "Lead が対応中の質問 (1)", "answerable": 0})
    );
    ui.screenshot("05-inbox").await;
    // ApprovalView only has a bounded summary, so the Web cannot show the
    // full input and does not approve; denying stays possible.
    let approval = ui
        .eval(
            r##"(() => {
                const card = document.querySelector(".card.approval");
                return { approve: card.querySelector('button[aria-label="このツール実行を承認"]').disabled,
                         deny: card.querySelector('button[aria-label="このツール実行を拒否"]').disabled,
                         hint: card.querySelector(".card-hint").textContent };
            })()"##,
        )
        .await;
    assert_eq!(
        approval,
        json!({"approve": true, "deny": false, "hint": "全文を確認できないため Web からは承認できません（拒否はできます）。"})
    );
    ui.click(".card.approval button.link").await;
    ui.screenshot("06-inbox-approval-expanded").await;
    ui.click(r##"button[aria-label="このツール実行を拒否"]"##)
        .await;
    let decision = backend.command("decide_tool_approval").await;
    assert_eq!(
        decision["payload"],
        json!({"approval_id": "ap-1", "core_epoch": "7", "approval_revision": "77777", "decision": "deny"})
    );
    ui.wait(r##"() => !document.querySelector(".card.approval")"##)
        .await;

    // ---- free-text answer ----
    ui.insert_text(".cards .card.question .answer-input", "Chromium と Firefox")
        .await;
    ui.click(".cards .card.question .answer-row button.primary")
        .await;
    ui.wait(r##"() => !document.querySelector(".cards .card.question")"##)
        .await;
    let answers: Vec<Value> = backend
        .world
        .lock()
        .unwrap()
        .commands
        .iter()
        .filter(|c| c["type"] == "answer_user_question")
        .map(|c| c["payload"].clone())
        .collect();
    assert_eq!(
        answers[1],
        json!({"thread": "thr-2", "question_id": "q-3", "answer": "Chromium と Firefox", "question_revision": "31337"})
    );
    assert_eq!(
        ui.wait(r##"() => document.querySelector("#inbox-badge").hidden"##)
            .await,
        true
    );

    // ---- other renderings for the record ----
    ui.go("#/t/thr-1").await;
    ui.wait(
        r##"() => document.querySelector(".messages")?.textContent.includes("本文の終わり。")"##,
    )
    .await;
    ui.color_scheme("dark").await;
    ui.screenshot("07-thread-dark").await;
    ui.go("#/").await;
    ui.wait(r##"() => document.querySelector('a.row[href="#/p/proj-1"]')"##)
        .await;
    ui.screenshot("08-projects-dark").await;
    ui.color_scheme("light").await;
    ui.page
        .execute(SetDeviceMetricsOverrideParams::new(1280, 800, 1.0, false))
        .await
        .unwrap();
    ui.go("#/t/thr-1").await;
    ui.wait(
        r##"() => document.querySelector(".messages")?.textContent.includes("本文の終わり。")"##,
    )
    .await;
    ui.screenshot("09-thread-desktop").await;

    // ---- a core restart: reconnect, then prompt to reload ----
    backend.restart();
    let banner = ui
        .wait(r##"() => !document.querySelector("#banner").hidden && document.querySelector("#banner").textContent"##)
        .await;
    assert!(
        banner.as_str().unwrap().contains("コアが再起動しました"),
        "{banner}"
    );
    ui.wait(r##"() => document.querySelector('#conn[data-state="live"]')"##)
        .await;
    let visible = ui
        .eval(r##"(() => { const r = document.querySelector("#banner").getBoundingClientRect(); return r.top >= 0 && r.bottom <= innerHeight && r.height > 0; })()"##)
        .await;
    assert_eq!(visible, true, "the banner stays in view on a scrolled page");
    ui.screenshot("10-restarted").await;

    assert_eq!(
        ui.eval("window.__problems").await,
        json!([]),
        "CSP violations or script errors"
    );
    browser.close().await.unwrap();
    server.shutdown().await.unwrap();
}
