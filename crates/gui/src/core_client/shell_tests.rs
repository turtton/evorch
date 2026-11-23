use std::sync::{Arc, Mutex};

use egui_kittest::{Harness, kittest::Queryable};
use serde_json::json;
use workbench_protocol::{
    handshake::Capability,
    ids::{ProjectId, ThreadId},
    views::{
        ApprovalView, CommittedMessage, ProjectSummary, QuestionView, RunSummary, ThreadSummary,
    },
};

use super::{CoreClientScreen, Shared};

fn view<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).unwrap()
}

fn message(index: u64, role: &str, text: &str) -> CommittedMessage {
    view(json!({
        "thread": "thr-1",
        "run": "run-1",
        "index": index.to_string(),
        "role": role,
        "content": {"type": "inline", "payload": {"text": text}},
    }))
}

fn populated() -> Shared {
    let mut shared = Shared {
        connected: true,
        status: "Connected".into(),
        capabilities: Capability::IMPLEMENTED.to_vec(),
        ..Shared::default()
    };
    let cache = &mut shared.cache;
    let project: ProjectSummary = view(json!({
        "id": "proj-1", "display_name": "evorch", "thread_count": 2, "revision": "1",
    }));
    cache.projects.insert("proj-1".into(), project);
    for (id, title, state) in [
        ("thr-1", "Fix login redirect", "waiting_for_approval"),
        ("thr-2", "Refactor storage writer", "idle"),
    ] {
        let thread: ThreadSummary = view(json!({
            "id": id, "project": "proj-1", "title": title, "state": state,
            "revision": "3", "updated_at": "1793799134000",
        }));
        cache.threads.insert(id.into(), thread);
    }
    let run: RunSummary = view(json!({
        "id": "run-1", "thread": "thr-1", "role": "orchestrator",
        "phase": "waiting_for_approval", "revision": "4",
    }));
    cache.runs.insert("run-1".into(), run);
    cache.history.insert(
        "thr-1".into(),
        vec![
            message(0, "user", "The login page redirects to /home even when `next` is set. Fix it."),
            message(
                1,
                "assistant",
                "I'll look at the redirect handler first.\n\n- `src/auth/login.rs`\n- `src/auth/session.rs`",
            ),
            message(2, "tool", "exit code: 0\nsrc/auth/login.rs:42: redirect_to(\"/home\")"),
            message(3, "tool", "--- a/src/auth/login.rs\n+++ b/src/auth/login.rs\n-redirect_to(\"/home\")\n+redirect_to(next.unwrap_or(\"/home\"))"),
            message(
                4,
                "assistant",
                "The handler ignored `next`. I changed it to honour the parameter and will run the tests.",
            ),
        ],
    );
    let question: QuestionView = view(json!({
        "id": "q-1",
        "requester": {"run": "run-1", "thread": "thr-1", "root_run": "run-1"},
        "target": {"type": "user"},
        "management_revision": "1", "routing_revision": "1", "blocking": true,
        "title": "Should an invalid `next` fall back to /home or to /?",
        "options": ["/home", "/"],
        "revision": "2",
        "raised_by": {"actor": "agent", "agent_run_id": "run-1", "surface": "none"},
    }));
    cache.questions.insert("q-1".into(), question);
    let approval: ApprovalView = view(json!({
        "id": "ap-1", "core_epoch": "7", "revision": "2", "thread": "thr-1", "run": "run-1",
        "tool_name": "shell", "summary": "cargo test -p auth", "summary_truncated": false,
        "input_bytes": "22", "state": "pending",
    }));
    cache.approvals.insert("ap-1".into(), approval);
    shared
}

fn screen() -> (CoreClientScreen, tokio::sync::mpsc::Receiver<super::Action>) {
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    let screen = CoreClientScreen::new(
        Arc::new(Mutex::new(populated())),
        tx,
        Some(ProjectId::new("proj-1").unwrap()),
        Some(ThreadId::new("thr-1").unwrap()),
    );
    (screen, rx)
}

#[test]
fn shell_shows_the_selected_conversation_beside_the_area_tabs() {
    let (screen, _actions) = screen();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .build_ui_state(|ui, screen: &mut CoreClientScreen| screen.ui(ui), screen);
    crate::theme::style::install_preset(&harness.ctx, crate::theme::style::ThemePreset::Graphite);
    harness.run_steps(4);
    if let Ok(path) = std::env::var("EVORCH_CORE_CLIENT_CAPTURE") {
        harness.render().unwrap().save(path).unwrap();
    }
    harness.get_by_label("Tool approval needed");
    harness.get_by_label("Question for you");
    harness.get_by_label("Message");
}
