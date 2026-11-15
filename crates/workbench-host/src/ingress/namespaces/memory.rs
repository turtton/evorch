//! Memory namespace adapter (read side, PR 13): the namespace names no command,
//! and a query only names its project; the directory is resolved here, from the
//! project directory the core owns.
use workbench_protocol::commands::CommandReply;
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::memory::{MemoryCommand, MemoryQuery, MemoryReply};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn memory_command(
        &mut self,
        command: MemoryCommand,
        _provenance: &Provenance,
        _restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        match command {}
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
        }))
    }
}

impl CommandHost {
    /// How a planned restart treats this namespace's commands. The namespace has
    /// none yet, so this cannot be called; PR 14, which adds the first command,
    /// writes the classification here (default: new work), not in the shared
    /// `restart.rs`.
    pub(in crate::ingress) fn memory_command_access(
        command: &MemoryCommand,
    ) -> crate::bootstrap::restart::CommandAccess {
        match *command {}
    }
}
