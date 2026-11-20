//! A renderer helper started and ended by a PRODUCT process, observed from
//! outside. The helper is whatever executable the product is told to use
//! through its own `EVORCH_RENDERER_EXEC` setting: the fake helper of the
//! `renderer-test-helper` feature (a harness binary, labelled as such in the
//! test) or the real `evorch-renderer` (ignored test, needs CEF).
//!
//! Synchronization: the run's own events say when the page was captured and
//! opened; the helper's exit is the kernel's wait notification, obtained by
//! the family's reaper (`ManagedProcess::join_family`), which is the helper's
//! adoptive parent once the product has exited. No sleep, no polling of `/proc`
//! for an exit.
#![allow(dead_code)]
use crate::product_exec::{
    launch::ProductProcess,
    names::ENV_RENDERER_HELPER,
    observe::{self, ProcInfo},
    wire::{self, MockProvider},
};
use mock_openai::ScriptedResponse;
use serde_json::json;
use std::path::{Path, PathBuf};
use workbench_protocol::commands::ComposerRole;

/// The helper executable: the explicit provisional setting, else the fake
/// helper Cargo built for this test binary (read at run time).
pub fn helper_binary() -> PathBuf {
    if let Some(helper) = std::env::var_os(ENV_RENDERER_HELPER).filter(|v| !v.is_empty()) {
        return helper.into();
    }
    std::env::var_os("CARGO_BIN_EXE_evorch-renderer-test-helper")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            panic!(
                "no renderer helper: set {ENV_RENDERER_HELPER} or build with the renderer-test-helper feature (the fixture does not skip)"
            )
        })
}

/// First turn script: capture `page.html` as an artifact.
pub fn capture_turn() -> Vec<ScriptedResponse> {
    vec![
        ScriptedResponse::tool_call(
            "f4p-capture",
            "mock-model",
            0,
            "capture-call",
            "render_artifact",
            [json!({"path": "page.html", "title": "page"}).to_string()],
        ),
        ScriptedResponse::text_stream("f4p-captured", "mock-model", ["captured"]),
    ]
}

/// Second turn script: open that artifact, which starts the helper.
pub fn open_turn(artifact_id: &str) -> Vec<ScriptedResponse> {
    vec![
        ScriptedResponse::tool_call(
            "f4p-open",
            "mock-model",
            0,
            "open-call",
            "inspect_artifact",
            [json!({"action": "open", "artifact_id": artifact_id}).to_string()],
        ),
        ScriptedResponse::text_stream("f4p-opened", "mock-model", ["opened"]),
    ]
}

/// `artifact-<32 hex>` that the product sent back to the provider.
pub fn artifact_id(provider: &MockProvider) -> String {
    for request in provider.chat_requests() {
        let body = request.body.to_string();
        for (at, _) in body.match_indices("artifact-") {
            let id: String = body[at..].chars().take("artifact-".len() + 32).collect();
            if id.len() == "artifact-".len() + 32
                && id["artifact-".len()..]
                    .chars()
                    .all(|c| c.is_ascii_hexdigit())
            {
                return id;
            }
        }
    }
    panic!(
        "no request carries the captured artifact id: {:?}",
        provider
            .chat_requests()
            .iter()
            .map(|r| r.body["messages"]
                .as_array()
                .and_then(|m| m.last().cloned()))
            .collect::<Vec<_>>()
    );
}

/// Helpers among the product's descendants: same file as `helper`.
pub fn helpers_of(product_pid: i32, helper: &Path) -> Vec<ProcInfo> {
    observe::descendants(product_pid)
        .into_iter()
        .filter(|p| {
            p.exe
                .as_deref()
                .is_some_and(|exe| observe::same_file(exe, helper))
        })
        .collect()
}

/// Drives the product through two turns on its public wire: capture a page,
/// open it. Returns the helper processes seen as the product's descendants
/// after the open completed (the run's `Waiting` event is the signal).
pub async fn open_page(
    product: &mut ProductProcess<'_>,
    provider: &MockProvider,
    project_dir: &Path,
) -> (Vec<ProcInfo>, workbench_client::WorkbenchClient) {
    std::fs::write(
        project_dir.join("page.html"),
        "<!doctype html><button>Add</button>",
    )
    .unwrap();
    let ready = product.ready().clone();
    assert!(
        helpers_of(product.pid(), &helper_binary()).is_empty(),
        "no helper exists before a page is opened"
    );
    let client = wire::connect(&ready.socket, "f4p-renderer-client").await;
    let mut events = wire::subscribe_all(&client).await;
    provider.0.enqueue(capture_turn());
    let reply = client
        .command(wire::send_chat_as(
            ComposerRole::Worker,
            "f4p-thread",
            "capture",
            "capture the page",
        ))
        .await
        .expect("send");
    let run = reply.run_id.unwrap().as_str().to_owned();
    wire::until_waiting(&mut events, &run).await.unwrap();
    assert!(
        helpers_of(product.pid(), &helper_binary()).is_empty(),
        "capturing needs no helper"
    );
    provider.0.enqueue(open_turn(&artifact_id(provider)));
    client
        .command(wire::send_chat_as(
            ComposerRole::Worker,
            "f4p-thread",
            "open",
            "open it",
        ))
        .await
        .expect("send");
    wire::until_waiting(&mut events, &run).await.unwrap();
    eprintln!(
        "F4P open turn tool result: {}",
        provider
            .chat_requests()
            .last()
            .and_then(|r| r.body["messages"]
                .as_array()
                .and_then(|m| m.last().cloned()))
            .unwrap_or_default()
    );
    (helpers_of(product.pid(), &helper_binary()), client)
}
