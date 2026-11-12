//! Wire samples of the namespaces later PRs fill in, one file per area.
//!
//! A PR that gives `workspace`, `mcp`, `execution`, `teams` or `lanes` new
//! operations adds their samples to its own file here (`samples()`), and the
//! shared consistency tests (`tests/namespaces.rs`) and the golden check
//! (`tests/golden.rs`, file `tests/golden/areas/<area>.json`) pick them up
//! without any shared file changing. The operations that existed before this
//! mechanism keep their samples in the parent module.
use workbench_protocol::commands::Command;
use workbench_protocol::events::UiEvent;
use workbench_protocol::queries::{Query, QueryReply};

pub mod execution;
pub mod lanes;
pub mod mcp;
pub mod teams;
pub mod workspace;

/// Samples of the operations one area added.
pub struct AreaSamples {
    pub name: &'static str,
    pub commands: Vec<Command>,
    pub queries: Vec<Query>,
    pub ui_events: Vec<UiEvent>,
    pub query_replies: Vec<QueryReply>,
}

impl AreaSamples {
    pub fn none(name: &'static str) -> Self {
        Self {
            name,
            commands: vec![],
            queries: vec![],
            ui_events: vec![],
            query_replies: vec![],
        }
    }
}

pub fn all() -> Vec<AreaSamples> {
    vec![
        workspace::samples(),
        mcp::samples(),
        execution::samples(),
        teams::samples(),
        lanes::samples(),
    ]
}

pub fn commands() -> Vec<Command> {
    all().into_iter().flat_map(|a| a.commands).collect()
}

pub fn queries() -> Vec<Query> {
    all().into_iter().flat_map(|a| a.queries).collect()
}

pub fn ui_events() -> Vec<UiEvent> {
    all().into_iter().flat_map(|a| a.ui_events).collect()
}

pub fn query_replies() -> Vec<QueryReply> {
    all().into_iter().flat_map(|a| a.query_replies).collect()
}
