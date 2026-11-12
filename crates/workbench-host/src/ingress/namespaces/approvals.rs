//! Approvals namespace adapter: tool approval decisions and approval inputs.
use workbench_protocol::commands::CommandReply;
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::{ApprovalsCommand, ApprovalsQuery, ApprovalsReply};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn approvals_command(
        &mut self,
        command: ApprovalsCommand,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        match command {
            ApprovalsCommand::DecideToolApproval(decide) => {
                self.decide_approval(&decide, provenance)
            }
        }
    }

    /// The listing is the projection's; the complete input is read from the
    /// approval registry, which only the host holds.
    pub(in crate::ingress) fn approvals_query(
        &self,
        query: &Query,
        approvals: &ApprovalsQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        match approvals {
            ApprovalsQuery::GetApprovalInput(q) => {
                query.validate(&Default::default())?;
                let input = self.approval_input_value(
                    &q.approval_id,
                    q.core_epoch,
                    q.approval_revision,
                    crate::queries::approval::registry_error,
                )?;
                Ok(QueryReply::Approvals(ApprovalsReply::ApprovalInput(
                    crate::queries::approval::chunk(q, input)?,
                )))
            }
            ApprovalsQuery::ListApprovals(_) => self.projection.query(query),
        }
    }
}
