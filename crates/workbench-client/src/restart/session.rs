use crate::{ClientError, WorkbenchClient};
use workbench_protocol::{
    ErrorCode,
    commands::{Command, CommandResult},
    handshake::Capability,
    namespaces::lifecycle::{
        AdminId, AdminReceipt, LifecycleQuery, LifecycleReply, LifecycleResult,
        owned_graphs::{OwnedGraphsQuery, OwnedGraphsReply},
        quarantine::{QuarantineQuery, QuarantineReply},
        restart::{
            RestartManagementStatus, RestartQuery, RestartReply, RestartTargets, ShellBlockers,
        },
    },
    queries::{Query, QueryReply},
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdminError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("the core answered a {0} with a reply of another kind")]
    Unexpected(&'static str),
}

/// Typed calls on one connection. A command goes out once: this layer has no
/// retry, and a lost reply is [`ClientError`] with an unknown outcome that the
/// caller settles by reading the operation with the same id.
pub struct RestartAdmin<'a> {
    client: &'a WorkbenchClient,
}

impl<'a> RestartAdmin<'a> {
    pub fn new(client: &'a WorkbenchClient) -> Self {
        Self { client }
    }

    /// Whether the connected core offers restart at all. Without it the answer
    /// to a request is `Unsupported`, so the person is told before anything is sent.
    pub fn offers_restart(&self) -> bool {
        self.client.server_hello().has(Capability::SafeRestart)
    }

    async fn restart_query(&self, query: RestartQuery) -> Result<RestartReply, AdminError> {
        match self
            .client
            .query(Query::Lifecycle(LifecycleQuery::Restart(query)))
            .await?
        {
            QueryReply::Lifecycle(LifecycleReply::Restart(reply)) => Ok(reply),
            _ => Err(AdminError::Unexpected("restart query")),
        }
    }

    pub async fn targets(&self) -> Result<RestartTargets, AdminError> {
        match self.restart_query(RestartQuery::Targets).await? {
            RestartReply::Targets(targets) => Ok(targets),
            _ => Err(AdminError::Unexpected("targets query")),
        }
    }

    pub async fn status(&self) -> Result<RestartManagementStatus, AdminError> {
        match self.restart_query(RestartQuery::Status).await? {
            RestartReply::Status(status) => Ok(status),
            _ => Err(AdminError::Unexpected("status query")),
        }
    }

    pub async fn shell_blockers(&self) -> Result<ShellBlockers, AdminError> {
        match self.restart_query(RestartQuery::ShellBlockers).await? {
            RestartReply::ShellBlockers(blockers) => Ok(blockers),
            _ => Err(AdminError::Unexpected("shell blockers query")),
        }
    }

    /// Q1: the quarantine the core holds. A read; nothing is changed.
    pub async fn quarantine(&self, query: QuarantineQuery) -> Result<QuarantineReply, AdminError> {
        match self
            .client
            .query(Query::Lifecycle(LifecycleQuery::Quarantine(query)))
            .await?
        {
            QueryReply::Lifecycle(LifecycleReply::Quarantine(reply)) => Ok(reply),
            _ => Err(AdminError::Unexpected("quarantine query")),
        }
    }

    /// Q2: the graphs a restart left undisposed. A read; nothing is changed.
    pub async fn owned_graphs(
        &self,
        query: OwnedGraphsQuery,
    ) -> Result<OwnedGraphsReply, AdminError> {
        match self
            .client
            .query(Query::Lifecycle(LifecycleQuery::OwnedGraphs(query)))
            .await?
        {
            QueryReply::Lifecycle(LifecycleReply::OwnedGraphs(reply)) => Ok(reply),
            _ => Err(AdminError::Unexpected("owned graphs query")),
        }
    }

    /// Send one command. Never sent again by this layer.
    pub async fn send(&self, command: Command) -> Result<AdminReceipt, AdminError> {
        let reply = self.client.command(command).await?;
        match reply.result.as_deref() {
            Some(CommandResult::Lifecycle(LifecycleResult::Accepted(receipt))) => {
                Ok(receipt.clone())
            }
            _ => Err(AdminError::Unexpected("command")),
        }
    }

    /// The state of an operation the core was given, by the id the client chose.
    /// `None` when the core does not know it.
    pub async fn operation(&self, id: &AdminId) -> Result<Option<AdminReceipt>, AdminError> {
        let query = Query::Lifecycle(LifecycleQuery::OperationStatus {
            operation_id: id.clone(),
        });
        match self.client.query(query).await {
            Ok(QueryReply::Lifecycle(LifecycleReply::OperationStatus(receipt))) => {
                Ok(Some(receipt))
            }
            Ok(_) => Err(AdminError::Unexpected("operation status query")),
            Err(ClientError::Server(error)) if error.code == ErrorCode::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}
