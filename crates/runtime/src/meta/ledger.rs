use event_bus::{Event, LedgerEvent};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{DispatchResult, EmptyArgs, error, parse, serialize};
use crate::AgentRuntime;
use crate::agent_loop::LoopState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AppendArgs {
    body: String,
}

#[derive(Serialize)]
#[serde(tag = "code", content = "message", rename_all = "snake_case")]
enum LedgerError {
    RunStoreUnavailable,
    StorageFailure(String),
}

pub(super) fn append(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<AppendArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    let Some(store) = runtime.shared.run_store.get() else {
        return error(json!(LedgerError::RunStoreUnavailable).to_string());
    };
    let run_id = state.caller_run_id().to_string();
    match store.handle.append_run_ledger(&run_id, &args.body) {
        Ok(seq) => {
            state
                .shared
                .bus
                .emit(Event::new(LedgerEvent::RunLedgerAppended {
                    run_id: run_id.clone(),
                    seq,
                    body: args.body,
                }));
            match store.handle.latest_run_ledger(&run_id) {
                Ok(Some(entry)) => serialize(&crate::continuity::WorkingNote::from(entry)),
                Ok(None) => error("working note disappeared after write"),
                Err(error_) => error(error_.to_string()),
            }
        }
        Err(storage_error) => {
            error(json!(LedgerError::StorageFailure(storage_error.to_string())).to_string())
        }
    }
}

pub(super) fn read(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    if let Err(message) = parse::<EmptyArgs>(input) {
        return error(message);
    }
    let Some(_store) = runtime.shared.run_store.get() else {
        return error(json!(LedgerError::RunStoreUnavailable).to_string());
    };
    match state.continuity.latest_note(runtime, state.caller_run_id()) {
        Ok(note) => serialize(&note),
        Err(storage_error) => {
            error(json!(LedgerError::StorageFailure(storage_error.to_string())).to_string())
        }
    }
}
