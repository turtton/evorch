//! Evaluation namespace adapter (reserved). The namespace has no operations yet,
//! so its command and query types are empty and these bodies are unreachable;
//! the PR that implements it replaces them, without editing the shared router.
use workbench_protocol::commands::CommandReply;
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::evaluation::{EvaluationCommand, EvaluationQuery};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn evaluation_command(
        &mut self,
        command: EvaluationCommand,
        _provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        match command {}
    }

    pub(in crate::ingress) fn evaluation_query(
        &self,
        _query: &Query,
        query: &EvaluationQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        match *query {}
    }
}
