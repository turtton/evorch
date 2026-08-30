//! `AgentRuntime::production` の fail-closed 構成テスト (bwrap 実在環境限定)。

mod support;

use std::sync::Arc;

use event_bus::EventBus;
use runtime::{AgentRuntime, ExecutionPolicy, Role};
use support::ScriptedModel;

// Given: bwrap が利用可能な環境 / When: production 構成でランタイムを生成する /
// Then: sandbox 伝播済みのランタイムが得られ、run 一覧は空である
#[ignore = "requires bwrap"]
#[test]
fn production_composes_fail_closed_runtime() {
    let bus = Arc::new(EventBus::new(8));
    let workspace = tempfile::tempdir().expect("tempdir");
    let runtime = AgentRuntime::production(
        bus,
        &ExecutionPolicy::for_role(Role::Orchestrator),
        workspace.path().to_path_buf(),
        Arc::new(ScriptedModel::new([])),
    )
    .expect("bwrap 環境では production 構成に成功するはず");
    assert!(runtime.list_agents().is_empty());
}
