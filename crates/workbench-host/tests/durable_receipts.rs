//! Submission deduplication that survives the host's memory: the runtime's
//! durable input receipt answers when the in-memory ledger is gone.

mod common;

use common::{HeldModel, host_with_project, send, web};
use std::sync::Arc;

use event_bus::EventBus;
use runtime::AgentRuntime;
use storage::{Storage, StorageConfig};
use tools::ToolExecutor;
use workbench_protocol::commands::{Command, CommandDisposition};
use workbench_protocol::errors::ErrorCode;

fn runtime_with_store(config: &StorageConfig, storage: &Storage) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(64));
    AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(HeldModel),
    )
    .with_run_store(runtime::RunStore::open(config, storage.handle()).unwrap())
}

#[test]
fn a_restarted_host_still_recognises_an_accepted_submission() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("receipts.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let runtime = runtime_with_store(&config, &storage);
    let mut first = host_with_project(&rt, runtime.clone(), dir.path());
    let accepted = first
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    let agents = runtime.list_agents().len();

    // A fresh host over the same runtime has an empty in-memory ledger.
    let mut restarted = host_with_project(&rt, runtime.clone(), dir.path());
    let again = restarted
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    let conflict = restarted
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "bye")), web())
        .unwrap_err();

    assert_eq!(again.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(again.run_id, accepted.run_id);
    assert_eq!(conflict.code, ErrorCode::Conflict);
    assert_eq!(runtime.list_agents().len(), agents);
}

#[test]
fn skills_are_part_of_what_makes_a_resend_identical() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("receipts.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let runtime = runtime_with_store(&config, &storage);
    let mut first = host_with_project(&rt, runtime.clone(), dir.path());
    first
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();

    let mut restarted = host_with_project(&rt, runtime, dir.path());
    let mut with_skill = send("thread-1", "sub-1", "hello");
    with_skill.skills = vec!["review".into()];
    let error = restarted
        .handle_command(Command::SendChat(with_skill), web())
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::Conflict);
}
