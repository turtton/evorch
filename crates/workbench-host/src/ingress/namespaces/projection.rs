//! Projection namespace adapter: command receipts are the ingress session's
//! own record; snapshot pages and bodies are the projection's.
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::ids::SessionId;
use workbench_protocol::namespaces::{ProjectionQuery, ProjectionReply};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn projection_query(
        &self,
        session: &SessionId,
        query: &Query,
        projection: &ProjectionQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        match projection {
            ProjectionQuery::GetCommandReceipt { request_id } => Ok(QueryReply::Projection(
                ProjectionReply::CommandReceipt(self.command_receipt(session, request_id)?),
            )),
            ProjectionQuery::ReadBody(_) | ProjectionQuery::ReadSnapshot(_) => {
                self.projection.query(query)
            }
        }
    }
}
