//! Maintenance namespace adapter: a command only names its task. The work
//! itself is an operation of the `maintenance` area, which runs it in bounded
//! batches through the one storage writer.
use workbench_protocol::commands::{CommandDisposition, CommandReply, CommandResult};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::maintenance::{
    MaintenanceCommand, MaintenanceQuery, MaintenanceReply, MaintenanceResult,
};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn maintenance_command(
        &mut self,
        command: MaintenanceCommand,
        _provenance: &Provenance,
        restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        let accepted = match &command {
            MaintenanceCommand::StartMaintenance(start) => {
                self.areas.maintenance.start(start, restart)?
            }
            // Cancelling holds the restart gate only for this call; the
            // operation it names keeps the permit of its own start.
            MaintenanceCommand::CancelMaintenance(cancel) => {
                self.areas.maintenance.cancel(&cancel.operation_id)?
            }
        };
        let mut reply = CommandReply::with_result(CommandResult::Maintenance(
            MaintenanceResult::Operation(Box::new(accepted.view)),
        ));
        if accepted.already_applied {
            reply.disposition = CommandDisposition::AlreadyApplied;
        }
        Ok(reply)
    }

    pub(in crate::ingress) fn maintenance_query(
        &self,
        _query: &Query,
        query: &MaintenanceQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        let maintenance = &self.areas.maintenance;
        Ok(QueryReply::Maintenance(match query {
            MaintenanceQuery::GetMaintenance(target) => {
                MaintenanceReply::Operation(Box::new(maintenance.get(&target.operation_id)?))
            }
            MaintenanceQuery::ListMaintenance(list) => {
                MaintenanceReply::List(maintenance.list(&list.page)?)
            }
        }))
    }
}

impl CommandHost {
    /// A start is new work: a preparing host refuses it, and one that was
    /// accepted settles before the host seals (its batches are bounded and
    /// nothing waits for a person). A cancel only stops work, so a planned
    /// restart always lets it through.
    pub(in crate::ingress) fn maintenance_command_access(
        command: &MaintenanceCommand,
    ) -> crate::bootstrap::restart::CommandAccess {
        use crate::bootstrap::restart::CommandAccess;
        match command {
            MaintenanceCommand::StartMaintenance(_) => CommandAccess::NewWork,
            MaintenanceCommand::CancelMaintenance(_) => CommandAccess::Stop,
        }
    }
}
