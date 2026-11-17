//! PullRequests namespace adapter: translates the protocol's operations into
//! the `pull_requests` area service (`self.areas.pull_requests`). The adapter
//! is where the thread's owner permit is obtained, and only for a request the
//! area has not accepted before. Every command is new work for a planned
//! restart: it writes the pull request state, so a preparing host refuses it,
//! and one that was accepted finishes before the host seals.
use workbench_protocol::commands::{CommandDisposition, CommandReply, CommandResult};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::ids::ThreadId;
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
        let thread = command.thread().clone();
        let owner = self.service.ownership().cloned();
        let submitted = self
            .areas
            .pull_requests
            .submit(command, Some(restart), move || {
                let denied = |message: &str| {
                    WorkbenchError::new(
                        ErrorCode::PermissionDenied,
                        message,
                        RetryAdvice::UserActionRequired,
                    )
                };
                owner
                    .ok_or_else(|| denied("ownership service is unavailable"))?
                    .owned_permit(thread.as_str())
                    .map_err(|_| denied("this core does not hold the thread"))
            })?;
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
                let owned = self.holds(thread);
                PullRequestsReply::Thread(Box::new(area.thread(thread, owned)?))
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

    /// Whether this core holds `thread` right now.
    fn holds(&self, thread: &ThreadId) -> bool {
        self.service
            .ownership()
            .is_some_and(|owner| owner.owned_permit(thread.as_str()).is_ok())
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
