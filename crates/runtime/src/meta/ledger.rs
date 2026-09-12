use event_bus::{Event, LedgerEvent};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{DispatchResult, EmptyArgs, error, parse, serialize};
use crate::AgentRuntime;
use crate::agent_loop::LoopState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppendArgs {
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
                    run_id,
                    seq,
                    body: args.body,
                }));
            serialize(&json!({ "seq": seq }))
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
    let Some(store) = runtime.shared.run_store.get() else {
        return error(json!(LedgerError::RunStoreUnavailable).to_string());
    };
    match store.ledger_entries(state.caller_run_id()) {
        Ok(entries) => serialize(
            &entries
                .iter()
                .map(|entry| {
                    json!({
                        "seq": entry.seq,
                        "body": entry.body,
                        "created_at_ns": entry.created_at_ns,
                    })
                })
                .collect::<Vec<_>>(),
        ),
        Err(storage_error) => {
            error(json!(LedgerError::StorageFailure(storage_error.to_string())).to_string())
        }
    }
}
