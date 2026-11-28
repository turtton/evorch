//! The side question's refusals through the host's command entry. What an answered
//! question holds, and what it sends the model, is covered through a real core and
//! provider by `core-client-e2e`'s `default_gui_cache`.
#[path = "memory_reads/support.rs"]
mod support;

mod common;

use support::{operation, world};
use workbench_host::areas::Areas;
use workbench_host::areas::execution::ExecutionArea;
use workbench_protocol::commands::Command;
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::ids::ThreadId;
use workbench_protocol::namespaces::execution::{AskBtw, CancelBtw, GetBtw};
use workbench_protocol::namespaces::{ExecutionCommand, ExecutionQuery};
use workbench_protocol::queries::Query;

fn ask(id: &str, thread: &str) -> Command {
    Command::Execution(ExecutionCommand::AskBtw(AskBtw {
        operation_id: operation(id),
        thread: ThreadId::new(thread).unwrap(),
        question: "why?".into(),
    }))
}

fn get(id: &str) -> Query {
    Query::Execution(ExecutionQuery::GetBtw(GetBtw {
        operation_id: operation(id),
    }))
}

// Given: a host whose execution area answers side questions, and a thread with no
//        conversation root in this core
// When: it is asked about, and an unknown operation is read and cancelled
// Then: each is refused as not found and nothing is kept for the id
#[test]
fn a_thread_without_a_conversation_and_an_unknown_operation_are_not_found() {
    let mut w = world(|parts| Areas {
        execution: ExecutionArea::with_runtime(parts.runtime)
            .with_side_questions(parts.tasks, parts.projection.source("execution").unwrap()),
        ..Default::default()
    });
    let refused = w
        .host
        .handle_command(ask("op-1", "thread-1"), common::web())
        .expect_err("no context to ask");
    assert_eq!(refused.code, ErrorCode::NotFound);
    assert_eq!(w.query(get("op-1")).unwrap_err().code, ErrorCode::NotFound);

    let cancel = Command::Execution(ExecutionCommand::CancelBtw(CancelBtw {
        operation_id: operation("op-2"),
    }));
    let refused = w
        .host
        .handle_command(cancel, common::web())
        .expect_err("nothing to cancel");
    assert_eq!(refused.code, ErrorCode::NotFound);
    assert_eq!(w.query(get("op-2")).unwrap_err().code, ErrorCode::NotFound);
}

// Given: a host whose execution area was not given the means to run side questions
// When: one is asked
// Then: it is refused as unsupported instead of hanging
#[test]
fn an_area_without_side_questions_refuses_them() {
    let mut w = world(|parts| Areas {
        execution: ExecutionArea::with_runtime(parts.runtime),
        ..Default::default()
    });
    let refused = w
        .host
        .handle_command(ask("op-1", "thread-1"), common::web())
        .expect_err("not wired");
    assert_eq!(refused.code, ErrorCode::UnsupportedCapability);
}
