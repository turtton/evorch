//! Durable, optional conversation procedure state. It grants no execution authority.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadTodoStatus {
    Pending,
    InProgress,
    Completed,
    /// Runtime-only: the work period ended before completion was recorded.
    /// Models cannot write it; a status update can change it back.
    /// It says nothing about any other lane or thread.
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadTodoItem {
    pub content: String,
    pub status: ThreadTodoStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadTodoSnapshot {
    /// Stable across replacements, clearing, and conversation handoff.
    pub list_id: String,
    pub thread_id: String,
    /// Increases for each replacement or handoff, including empty lists.
    pub revision: u64,
    pub items: Vec<ThreadTodoItem>,
}
