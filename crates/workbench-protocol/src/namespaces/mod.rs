//! Typed protocol namespaces (ADR 0045, PR 08s).
//!
//! `Command`, `Query`, `QueryReply`, `CommandResult` and `UiEvent` are thin
//! routers: each variant names one namespace and wraps that namespace's own
//! type. A later PR adds operations by editing only its namespace module (and
//! the matching host adapter), never the shared enums or their dispatch.
//!
//! ## Operation table
//!
//! | namespace | commands | queries | events |
//! |---|---|---|---|
//! | `conversation` | `resolve_project`, `create_execution`, `create_goal`, `prepare_existing_thread`, `upload_image`, `discard_attachment`, `send_chat`, `continue_chat`, `compact_chat`, `deliver_follow_ups_next_turn`, `withdraw_follow_up` | `get_input_receipts`, `read_committed_history` | `input_receipt_changed`, `message_committed` |
//! | `workspace` | - | `list_projects`, `list_threads`, `get_thread` | `thread_changed` |
//! | `execution` | `stop_chat`, `stop_run`, `submit_goal`, `set_goal_review`, `set_goal_checks_paused`, `set_agent_web_tools_enabled` | `list_runs` | `run_changed` |
//! | `questions` | `answer_user_question` | `list_questions` | `question_changed` |
//! | `approvals` | `decide_tool_approval` | `list_approvals`, `get_approval_input` | `approval_changed` |
//! | `lanes` | `manage_lane`, `close_lane` | `get_lane_management` | - |
//! | `lifecycle` | - | `get_core_status` | `core_status_changed` |
//! | `projection` | - | `get_command_receipt`, `read_body`, `read_snapshot` | `resync_required`, `subscription_closed` |
//!
//! ## Reserved namespaces
//!
//! `settings`, `auth`, `mcp`, `teams`, `diagnostics`, `memory`, `evaluation`,
//! `improvement`, `pull_requests`, `browser`, `artifacts` and `maintenance` are
//! reserved for later PRs. Every type in them is an empty (uninhabited) enum,
//! so no value of them exists: they cannot be constructed, answered or
//! advertised, and a peer naming one is refused as
//! [`ErrorCode::UnsupportedCapability`](crate::ErrorCode). There is no generic
//! `serde_json::Value` operation and no free-form method name anywhere.
//!
//! ## Implementing a namespace
//!
//! A namespace is whatever its module declares. Replace the module's
//! `reserved_namespace!` (or extend its operations) with real types and list
//! the wire names in `COMMAND_OPERATIONS`, `QUERY_OPERATIONS` and
//! `EVENT_TYPES` (and `NESTED_COMMANDS` for a tagged choice inside a payload).
//! [`NamespaceInfo::status`] follows from those lists, so it flips from
//! `Reserved` to `Implemented` without touching this file, `Command`, `Query`,
//! `QueryReply`, `CommandResult` or `UiEvent`, which only delegate to the
//! namespace's own types. Each operation names the capability it needs
//! (`required_capability`); it must be one of [`crate::Capability::IMPLEMENTED`]
//! (the protocol tests enforce this), so an operation never advertises more than
//! the core offers. Only the `projection` namespace's control notices end a
//! subscription.

/// An empty command enum: the namespace has no commands (yet).
macro_rules! no_commands {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        pub enum $name {}
        impl $name {
            /// The capability a core must offer; never reached, the type is empty.
            pub fn required_capability(&self) -> crate::Capability {
                match *self {}
            }
            pub fn validate(&self) -> Result<(), crate::validation::ValidationError> {
                match *self {}
            }
        }
    };
}

macro_rules! no_results {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        pub enum $name {}
    };
}

macro_rules! no_queries {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        pub enum $name {}
        impl $name {
            pub fn required_capability(&self) -> crate::Capability {
                match *self {}
            }
            pub fn validate(
                &self,
                _limits: &crate::handshake::WireLimits,
            ) -> Result<(), crate::validation::ValidationError> {
                match *self {}
            }
        }
    };
}

macro_rules! no_replies {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        pub enum $name {}
        impl $name {
            pub fn validate(
                &self,
                _limits: &crate::handshake::WireLimits,
            ) -> Result<(), crate::validation::ValidationError> {
                match *self {}
            }
        }
    };
}

macro_rules! no_events {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        pub enum $name {}
        impl $name {
            pub fn validate(&self) -> Result<(), crate::validation::ValidationError> {
                match *self {}
            }
        }
    };
}

/// Everything an unimplemented namespace consists of: five empty enums and
/// empty operation lists.
macro_rules! reserved_namespace {
    ($command:ident, $result:ident, $query:ident, $reply:ident, $event:ident) => {
        no_commands!($command);
        no_results!($result);
        no_queries!($query);
        no_replies!($reply);
        no_events!($event);
        /// Wire names of the namespace's commands (none).
        pub const COMMAND_OPERATIONS: &[&str] = &[];
        /// Wire names of the namespace's queries (none).
        pub const QUERY_OPERATIONS: &[&str] = &[];
        /// Wire names of the namespace's events (none).
        pub const EVENT_TYPES: &[&str] = &[];
        /// Tagged choices inside a command's payload that classification names (none).
        pub const NESTED_COMMANDS: &[$crate::namespaces::NestedOperation] = &[];
    };
}

pub mod approvals;
pub mod artifacts;
pub mod auth;
pub mod browser;
pub mod conversation;
pub mod diagnostics;
pub mod evaluation;
pub mod execution;
pub mod improvement;
pub mod lanes;
pub mod lifecycle;
pub mod maintenance;
pub mod mcp;
pub mod memory;
pub mod projection;
pub mod pull_requests;
pub mod questions;
pub mod settings;
pub mod teams;
pub mod workspace;

/// How far a namespace is implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceStatus {
    /// Has operations a core serves and capabilities it may advertise.
    Implemented,
    /// Only a typed placeholder; every request naming it is refused.
    Reserved,
}

/// One namespace as the wire names it, with its operations per request kind.
#[derive(Debug, Clone, Copy)]
pub struct NamespaceInfo {
    pub name: &'static str,
    pub status: NamespaceStatus,
    pub commands: &'static [&'static str],
    pub queries: &'static [&'static str],
    pub events: &'static [&'static str],
    /// Operations whose payload itself names a tagged choice (a lane
    /// management action, say) that classification must look into.
    pub nested_commands: &'static [NestedOperation],
}

/// A command whose payload contains a tagged choice: `pointer` locates the
/// choice's `type` inside the operation's payload and `known` lists the
/// choices this protocol names.
#[derive(Debug, Clone, Copy)]
pub struct NestedOperation {
    pub operation: &'static str,
    /// JSON pointer, relative to the operation's `payload`, of the choice's
    /// `type` string.
    pub pointer: &'static str,
    pub known: &'static [&'static str],
}

/// How far a namespace is implemented, from what its module declares: a
/// namespace that names no operation at all is reserved, whatever else it
/// contains. Nothing else decides it, so giving a reserved namespace its first
/// operation (in its own module) is all it takes to implement it.
const fn status_of(commands: &[&str], queries: &[&str], events: &[&str]) -> NamespaceStatus {
    if commands.is_empty() && queries.is_empty() && events.is_empty() {
        NamespaceStatus::Reserved
    } else {
        NamespaceStatus::Implemented
    }
}

/// A namespace's declaration, read from its module: every module exports
/// `COMMAND_OPERATIONS`, `QUERY_OPERATIONS`, `EVENT_TYPES` and
/// `NESTED_COMMANDS`.
macro_rules! info {
    ($name:literal, $module:ident) => {
        NamespaceInfo {
            name: $name,
            status: status_of(
                $module::COMMAND_OPERATIONS,
                $module::QUERY_OPERATIONS,
                $module::EVENT_TYPES,
            ),
            commands: $module::COMMAND_OPERATIONS,
            queries: $module::QUERY_OPERATIONS,
            events: $module::EVENT_TYPES,
            nested_commands: $module::NESTED_COMMANDS,
        }
    };
}

/// Every namespace, in the order `Command`, `Query` and `UiEvent` list them.
/// The status of each follows from its module (see [`status_of`]); this table
/// only lists the namespaces.
pub const NAMESPACES: [NamespaceInfo; 20] = [
    info!("conversation", conversation),
    info!("workspace", workspace),
    info!("execution", execution),
    info!("questions", questions),
    info!("approvals", approvals),
    info!("lanes", lanes),
    info!("lifecycle", lifecycle),
    info!("projection", projection),
    info!("settings", settings),
    info!("auth", auth),
    info!("mcp", mcp),
    info!("teams", teams),
    info!("diagnostics", diagnostics),
    info!("memory", memory),
    info!("evaluation", evaluation),
    info!("improvement", improvement),
    info!("pull_requests", pull_requests),
    info!("browser", browser),
    info!("artifacts", artifacts),
    info!("maintenance", maintenance),
];

/// Wire names of every namespace, taken from [`NAMESPACES`].
pub const NAMESPACE_NAMES: [&str; 20] = {
    let mut names = [""; 20];
    let mut i = 0;
    while i < names.len() {
        names[i] = NAMESPACES[i].name;
        i += 1;
    }
    names
};

/// The namespace named `name` on the wire.
pub fn namespace(name: &str) -> Option<&'static NamespaceInfo> {
    NAMESPACES.iter().find(|n| n.name == name)
}

/// What a request kind's payload names, for classifying a request that did
/// not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classified {
    /// The namespace is not one this protocol names.
    UnknownNamespace,
    /// The namespace has no such operation (also: it has no operations at all).
    UnknownOperation,
    /// Namespace and operation exist; the payload is what is wrong.
    KnownOperation,
    /// The shape does not allow telling.
    Malformed,
}

/// Classify `payload` (`{"type": <namespace>, "payload": {"type": <operation>, ..}}`)
/// without reading anything but those two names.
pub fn classify(
    payload: &serde_json::Value,
    operations: impl Fn(&NamespaceInfo) -> &'static [&'static str],
) -> Classified {
    let Some(name) = payload.get("type").and_then(serde_json::Value::as_str) else {
        return Classified::Malformed;
    };
    let Some(info) = namespace(name) else {
        return Classified::UnknownNamespace;
    };
    let known = operations(info);
    if known.is_empty() {
        // Nothing in this namespace exists, whatever else was sent.
        return Classified::UnknownOperation;
    }
    match payload
        .pointer("/payload/type")
        .and_then(serde_json::Value::as_str)
    {
        Some(operation) if known.contains(&operation) => Classified::KnownOperation,
        Some(_) => Classified::UnknownOperation,
        None => Classified::Malformed,
    }
}

/// [`classify`] for a command: besides the namespace and operation, a tagged
/// choice inside the operation's payload (`manage_lane`'s action) is named too,
/// so an unknown choice is unsupported rather than malformed. Only the
/// choice's name is read.
pub fn classify_command(payload: &serde_json::Value) -> Classified {
    let classified = classify(payload, |n| n.commands);
    if classified != Classified::KnownOperation {
        return classified;
    }
    let (Some(info), Some(operation)) = (
        payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .and_then(namespace),
        payload
            .pointer("/payload/type")
            .and_then(serde_json::Value::as_str),
    ) else {
        return classified;
    };
    for nested in info
        .nested_commands
        .iter()
        .filter(|n| n.operation == operation)
    {
        let choice = payload
            .pointer("/payload/payload")
            .and_then(|operation_payload| operation_payload.pointer(nested.pointer))
            .and_then(serde_json::Value::as_str);
        // A missing or non-string choice is a malformed known operation.
        if let Some(choice) = choice
            && !nested.known.contains(&choice)
        {
            return Classified::UnknownOperation;
        }
    }
    classified
}

/// The error for a client that names a namespace or operation this core does
/// not have.
pub(crate) fn unsupported_operation() -> crate::WorkbenchError {
    crate::WorkbenchError::new(
        crate::ErrorCode::UnsupportedCapability,
        "unknown request type",
        crate::RetryAdvice::Never,
    )
}

pub use approvals::{
    ApprovalsCommand, ApprovalsEvent, ApprovalsQuery, ApprovalsReply, ApprovalsResult,
};
pub use conversation::{
    ConversationCommand, ConversationEvent, ConversationQuery, ConversationReply,
    ConversationResult,
};
pub use execution::{
    ExecutionCommand, ExecutionEvent, ExecutionQuery, ExecutionReply, ExecutionResult,
};
pub use lanes::{LanesCommand, LanesEvent, LanesQuery, LanesReply, LanesResult};
pub use lifecycle::{
    LifecycleCommand, LifecycleEvent, LifecycleQuery, LifecycleReply, LifecycleResult,
};
pub use projection::{
    ProjectionCommand, ProjectionEvent, ProjectionQuery, ProjectionReply, ProjectionResult,
};
pub use questions::{
    QuestionsCommand, QuestionsEvent, QuestionsQuery, QuestionsReply, QuestionsResult,
};
pub use workspace::{
    WorkspaceCommand, WorkspaceEvent, WorkspaceQuery, WorkspaceReply, WorkspaceResult,
};
