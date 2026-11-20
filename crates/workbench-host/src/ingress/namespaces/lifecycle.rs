//! Lifecycle namespace adapter: the core status are read from the projection. The
//! namespace has no commands, so its command type is empty.
pub(crate) mod owned_graphs;
pub(crate) mod quarantine;
pub(crate) mod restart;
use workbench_protocol::commands::CommandReply;
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::{LifecycleCommand, LifecycleQuery};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn lifecycle_command(
        &mut self,
        command: LifecycleCommand,
        _provenance: &Provenance,
        _restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        let _ = command;
        Err(crate::queries::error(
            workbench_protocol::errors::ErrorCode::PermissionDenied,
            "management command requires verified async ingress",
        ))
    }

    pub(in crate::ingress) fn lifecycle_query(
        &self,
        query: &Query,
        _namespace_query: &LifecycleQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        self.projection.query(query)
    }
}
