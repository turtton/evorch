//! Auth namespace adapter: translates the protocol's operations into the
//! `auth` area service (`self.areas.auth`).
//!
//! A planned restart treats the commands by what they are: a pasted login
//! input continues a login that exists (accepted until the safe point), a
//! cancellation always goes through, everything else is new work.
use workbench_protocol::commands::{CommandDisposition, CommandReply, CommandResult};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::auth::{AuthCommand, AuthQuery, AuthReply, AuthResult};
use workbench_protocol::queries::{Query, QueryReply};

use crate::areas::auth::Submitted;
use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn auth_command(
        &mut self,
        command: AuthCommand,
        _provenance: &Provenance,
        restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        let gate = self.service().restart_gate();
        Ok(
            match self.areas.auth.submit(command, Some(restart), gate)? {
                Submitted::Operation {
                    view,
                    already_applied,
                } => {
                    let mut reply =
                        CommandReply::with_result(CommandResult::Auth(AuthResult::Operation(view)));
                    if already_applied {
                        reply.disposition = CommandDisposition::AlreadyApplied;
                    }
                    reply
                }
                Submitted::LoginInput(outcome) => {
                    CommandReply::with_result(CommandResult::Auth(AuthResult::LoginInput(outcome)))
                }
            },
        )
    }

    pub(in crate::ingress) fn auth_query(
        &self,
        _query: &Query,
        query: &AuthQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        let auth = &self.areas.auth;
        Ok(QueryReply::Auth(match query {
            AuthQuery::GetCredentialStatus {} => AuthReply::Status(auth.status()?),
            AuthQuery::GetLogin { login_id } => AuthReply::Login(auth.login(login_id)?),
            AuthQuery::GetAuthOperation { operation_id } => {
                AuthReply::Operation(auth.operation(operation_id)?)
            }
            AuthQuery::ListAuthOperations {} => AuthReply::Operations {
                operations: auth.operations()?,
            },
        }))
    }
}

impl CommandHost {
    pub(in crate::ingress) fn auth_command_access(
        command: &AuthCommand,
    ) -> crate::bootstrap::restart::CommandAccess {
        use crate::bootstrap::restart::CommandAccess;
        match command {
            AuthCommand::SubmitLoginInput(_) => CommandAccess::ExistingInput,
            AuthCommand::CancelLogin(_) => CommandAccess::Stop,
            AuthCommand::RefreshCredentials(_)
            | AuthCommand::SaveApiKey(_)
            | AuthCommand::DeleteCredential(_)
            | AuthCommand::StartLogin(_) => CommandAccess::NewWork,
        }
    }
}
