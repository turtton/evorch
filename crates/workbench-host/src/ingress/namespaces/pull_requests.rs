//! PullRequests namespace adapter: translates the protocol's operations into
//! the `pull_requests` area service (`self.areas.pull_requests`), which retains
//! operation identity, thread validation and duplicate suppression. Every
//! command is new work for a planned
//! restart: it writes the pull request state, so a preparing host refuses it,
//! and one that was accepted finishes before the host seals.
use workbench_protocol::commands::{CommandDisposition, CommandReply, CommandResult};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::pull_requests::{
    PullRequestsCommand, PullRequestsQuery, PullRequestsReply, PullRequestsResult,
};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn pull_requests_command(
        &mut self,
        command: PullRequestsCommand,
        _provenance: &Provenance,
        restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        let submitted = self.areas.pull_requests.submit(command, Some(restart))?;
        let mut reply = CommandReply::with_result(CommandResult::PullRequests(
            PullRequestsResult::Operation(submitted.view),
        ));
        if submitted.already_applied {
            reply.disposition = CommandDisposition::AlreadyApplied;
        }
        Ok(reply)
    }

    pub(in crate::ingress) fn pull_requests_query(
        &self,
        _query: &Query,
        query: &PullRequestsQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        let area = &self.areas.pull_requests;
        Ok(QueryReply::PullRequests(match query {
            PullRequestsQuery::GetThreadPullRequest { thread } => {
                PullRequestsReply::Thread(Box::new(area.thread(thread)?))
            }
            PullRequestsQuery::GetPullRequestNotice { notice_id } => {
                PullRequestsReply::Notice(Box::new(area.notice(notice_id)?))
            }
            PullRequestsQuery::GetPullRequestOperation { operation_id } => {
                PullRequestsReply::Operation(area.operation(operation_id)?)
            }
            PullRequestsQuery::ListPullRequestOperations {} => PullRequestsReply::Operations {
                operations: area.operations()?,
            },
        }))
    }
}

impl CommandHost {
    /// Every pull request command writes the pull request state: new work.
    pub(in crate::ingress) fn pull_requests_command_access(
        _command: &PullRequestsCommand,
    ) -> crate::bootstrap::restart::CommandAccess {
        crate::bootstrap::restart::CommandAccess::NewWork
    }
}
