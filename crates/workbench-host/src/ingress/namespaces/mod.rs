//! Host adapters, one per protocol namespace.
//!
//! [`CommandHost::dispatch_command`] and [`CommandHost::dispatch_query`] only
//! pick the namespace; each adapter translates its namespace's operations and
//! runs them through the shared machinery in `ingress` (ownership check,
//! submission ledger, [`crate::commands::CommandService`]). A later PR that adds
//! operations to a namespace edits that namespace's protocol module and its
//! adapter here, not this dispatch. The reserved namespaces have no
//! operations: their types are empty, so they are unreachable here and the
//! compiler, not a runtime check, refuses them.

mod approvals;
pub(super) mod conversation;
mod execution;
mod lanes;
mod projection;
mod questions;

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
            Command::Execution(c) => self.execution_command(c, provenance),
            Command::Questions(c) => self.questions_command(c, provenance),
            Command::Approvals(c) => self.approvals_command(c, provenance),
            Command::Lanes(c) => self.lanes_command(c, provenance),
            Command::Workspace(c) => match c {},
            Command::Lifecycle(c) => match c {},
            Command::Projection(c) => match c {},
            Command::Settings(c) => match c {},
            Command::Auth(c) => match c {},
            Command::Mcp(c) => match c {},
            Command::Teams(c) => match c {},
            Command::Diagnostics(c) => match c {},
            Command::Memory(c) => match c {},
            Command::Evaluation(c) => match c {},
            Command::Improvement(c) => match c {},
            Command::PullRequests(c) => match c {},
            Command::Browser(c) => match c {},
            Command::Artifacts(c) => match c {},
            Command::Maintenance(c) => match c {},
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
            Query::Lanes(q) => self.lanes_query(q),
            Query::Approvals(q) => self.approvals_query(query, q),
            Query::Projection(q) => self.projection_query(session, query, q),
            Query::Conversation(_)
            | Query::Workspace(_)
            | Query::Execution(_)
            | Query::Questions(_)
            | Query::Lifecycle(_) => self.projection.query(query),
            Query::Settings(q) => match *q {},
            Query::Auth(q) => match *q {},
            Query::Mcp(q) => match *q {},
            Query::Teams(q) => match *q {},
            Query::Diagnostics(q) => match *q {},
            Query::Memory(q) => match *q {},
            Query::Evaluation(q) => match *q {},
            Query::Improvement(q) => match *q {},
            Query::PullRequests(q) => match *q {},
            Query::Browser(q) => match *q {},
            Query::Artifacts(q) => match *q {},
            Query::Maintenance(q) => match *q {},
        }
    }
}
