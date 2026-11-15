//! Maintenance namespace adapter (reserved). The namespace has no operations yet,
//! so its command and query types are empty and these bodies are unreachable;
//! the PR that implements it replaces them, without editing the shared router.
use workbench_protocol::commands::CommandReply;
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::maintenance::{MaintenanceCommand, MaintenanceQuery};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn maintenance_command(
        &mut self,
        command: MaintenanceCommand,
        _provenance: &Provenance,
        _restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        match command {}
    }

    pub(in crate::ingress) fn maintenance_query(
        &self,
        _query: &Query,
        query: &MaintenanceQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        match *query {}
    }
}

impl CommandHost {
    /// How a planned restart treats this namespace's commands. The namespace has
    /// none yet, so this cannot be called; the PR that gives it its first command
    /// writes the classification here (default: new work), not in the shared
    /// `restart.rs`.
    pub(in crate::ingress) fn maintenance_command_access(
        command: &MaintenanceCommand,
    ) -> crate::bootstrap::restart::CommandAccess {
        match *command {}
    }
}
