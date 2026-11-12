//! Host adapters, one per protocol namespace.
//!
//! [`CommandHost::dispatch_command`] and [`CommandHost::dispatch_query`] only
//! pick the namespace; each adapter translates its namespace's operations and
//! runs them through the shared machinery in `ingress` (ownership check,
//! submission ledger, [`crate::commands::CommandService`]). A later PR that adds
//! operations to a namespace edits that namespace's protocol module, its
//! adapter here and (for the `workspace`, `mcp`, `execution`, `teams` and `lanes`
//! areas) its service in `crate::areas`, which the adapter reaches as
//! `self.areas.<area>`; this dispatch stays as it is. The reserved namespaces
//! have an adapter file each too, with no operations: their types are empty, so
//! the compiler, not a runtime check, refuses them, and the PR that implements
//! one fills in its own file.

mod approvals;
mod artifacts;
mod auth;
mod browser;
pub(super) mod conversation;
mod diagnostics;
mod evaluation;
mod execution;
mod improvement;
mod lanes;
mod lifecycle;
mod maintenance;
mod mcp;
mod memory;
mod projection;
mod pull_requests;
mod questions;
mod settings;
mod teams;
mod workspace;

use workbench_protocol::commands::{Command, CommandReply};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::ids::SessionId;
use workbench_protocol::queries::{Query, QueryReply};

use super::CommandHost;

impl CommandHost {
    /// Route a validated command to the adapter of its namespace.
    pub(in crate::ingress) fn dispatch_command(
        &mut self,
        command: Command,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        match command {
            Command::Conversation(c) => self.conversation_command(c, provenance),
            Command::Workspace(c) => self.workspace_command(c, provenance),
            Command::Execution(c) => self.execution_command(c, provenance),
            Command::Questions(c) => self.questions_command(c, provenance),
            Command::Approvals(c) => self.approvals_command(c, provenance),
            Command::Lanes(c) => self.lanes_command(c, provenance),
            Command::Lifecycle(c) => self.lifecycle_command(c, provenance),
            Command::Projection(c) => self.projection_command(c, provenance),
            Command::Settings(c) => self.settings_command(c, provenance),
            Command::Auth(c) => self.auth_command(c, provenance),
            Command::Mcp(c) => self.mcp_command(c, provenance),
            Command::Teams(c) => self.teams_command(c, provenance),
            Command::Diagnostics(c) => self.diagnostics_command(c, provenance),
            Command::Memory(c) => self.memory_command(c, provenance),
            Command::Evaluation(c) => self.evaluation_command(c, provenance),
            Command::Improvement(c) => self.improvement_command(c, provenance),
            Command::PullRequests(c) => self.pull_requests_command(c, provenance),
            Command::Browser(c) => self.browser_command(c, provenance),
            Command::Artifacts(c) => self.artifacts_command(c, provenance),
            Command::Maintenance(c) => self.maintenance_command(c, provenance),
        }
    }

    /// Route a validated query: what the host itself owns (lane management,
    /// approval inputs, command receipts) is answered by its adapter, the rest
    /// by the projection.
    pub(in crate::ingress) fn dispatch_query(
        &self,
        session: &SessionId,
        query: &Query,
    ) -> Result<QueryReply, WorkbenchError> {
        match query {
            Query::Conversation(q) => self.conversation_query(query, q),
            Query::Workspace(q) => self.workspace_query(query, q),
            Query::Execution(q) => self.execution_query(query, q),
            Query::Questions(q) => self.questions_query(query, q),
            Query::Approvals(q) => self.approvals_query(query, q),
            Query::Lanes(q) => self.lanes_query(q),
            Query::Lifecycle(q) => self.lifecycle_query(query, q),
            Query::Projection(q) => self.projection_query(session, query, q),
            Query::Settings(q) => self.settings_query(query, q),
            Query::Auth(q) => self.auth_query(query, q),
            Query::Mcp(q) => self.mcp_query(query, q),
            Query::Teams(q) => self.teams_query(query, q),
            Query::Diagnostics(q) => self.diagnostics_query(query, q),
            Query::Memory(q) => self.memory_query(query, q),
            Query::Evaluation(q) => self.evaluation_query(query, q),
            Query::Improvement(q) => self.improvement_query(query, q),
            Query::PullRequests(q) => self.pull_requests_query(query, q),
            Query::Browser(q) => self.browser_query(query, q),
            Query::Artifacts(q) => self.artifacts_query(query, q),
            Query::Maintenance(q) => self.maintenance_query(query, q),
        }
    }
}
