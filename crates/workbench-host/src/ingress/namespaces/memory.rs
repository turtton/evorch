//! Memory namespace adapter: a command or query only names its project; the
//! directory is resolved here, from the project directory the core owns. An
//! update is handed to the `memory` area, which runs it as an operation.
use workbench_protocol::commands::{CommandDisposition, CommandReply, CommandResult};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::memory::{
    MemoryCommand, MemoryQuery, MemoryReply, MemoryResult,
};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn memory_command(
        &mut self,
        command: MemoryCommand,
        _provenance: &Provenance,
        restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        let root = self.project_root(&command.update().project)?;
        let submitted = self.areas.memory.submit(&command, root, restart)?;
        let mut reply = CommandReply::with_result(CommandResult::Memory(MemoryResult::Operation(
            submitted.view,
        )));
        if submitted.already_applied {
            reply.disposition = CommandDisposition::AlreadyApplied;
        }
        Ok(reply)
    }

    pub(in crate::ingress) fn memory_query(
        &self,
        _query: &Query,
        query: &MemoryQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        let memory = &self.areas.memory;
        Ok(QueryReply::Memory(match query {
            MemoryQuery::ListMemory(list) => MemoryReply::List(Box::new(
                memory.list(self.project_root(&list.project)?, list)?,
            )),
            MemoryQuery::GetMemory(get) => {
                MemoryReply::Detail(Box::new(memory.get(self.project_root(&get.project)?, get)?))
            }
            MemoryQuery::GetMemoryOperation(target) => {
                MemoryReply::Operation(memory.operation(&target.operation_id)?)
            }
            MemoryQuery::GetMemoryUpdate(target) => {
                MemoryReply::Update(Box::new(memory.update_of(&target.operation_id)?))
            }
        }))
    }
}

impl CommandHost {
    /// Every memory command writes a lesson: new work for a planned restart. A
    /// preparing host refuses it, and one that was accepted settles before the
    /// host seals (bounded I/O, nothing waits for a person).
    pub(in crate::ingress) fn memory_command_access(
        _command: &MemoryCommand,
    ) -> crate::bootstrap::restart::CommandAccess {
        crate::bootstrap::restart::CommandAccess::NewWork
    }
}
