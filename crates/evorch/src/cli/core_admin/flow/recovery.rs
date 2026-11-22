//! Q1 / Q2 from the command line: list, show, and the three operations that
//! change something (`quarantine discard`, `graphs release`, `graphs resume`).
//!
//! Reads go out as they are. A change is built from the detail the core just
//! gave (bound to its revision and epoch), is sent only after a confirmation
//! and only when the core offers management at all, goes out once, and a lost
//! reply is settled by reading the same operation id, never by sending again.
use super::super::args::{GraphsOp, QuarantineOp};
use super::{AdminError, ClientError, Flow, exit, operation_id};
use workbench_client::{
    WorkbenchClient,
    restart::{
        recovery::{
            GraphAction, describe_graph, describe_graph_operation, describe_quarantine,
            describe_undisposed, discard_quarantined, dispose_graph,
        },
        session::RestartAdmin,
        view::describe_receipt,
    },
};
use workbench_protocol::namespaces::lifecycle::{
    AdminId, AdminOperationState, AdminOutcome, AdminReceipt,
    owned_graphs::{OwnedGraphsQuery, OwnedGraphsReply},
    quarantine::{QuarantineQuery, QuarantineReply},
};

/// How often an accepted operation is read before the command stops waiting.
const SETTLE_READS: u32 = 50;
const SETTLE_PAUSE: std::time::Duration = std::time::Duration::from_millis(200);

impl Flow<'_> {
    fn require_management(&mut self, admin: &RestartAdmin<'_>) -> bool {
        if admin.offers_restart() {
            return true;
        }
        self.warn("this core does not offer recovery management; nothing was sent");
        false
    }

    pub(super) async fn quarantine(&mut self, client: &WorkbenchClient, op: QuarantineOp) -> u8 {
        let admin = RestartAdmin::new(client);
        if !self.require_management(&admin) {
            return exit::FAILURE;
        }
        match op {
            QuarantineOp::List => match admin.quarantine(QuarantineQuery::List).await {
                Ok(reply) => self.say_all(describe_quarantine(&reply)),
                Err(error) => self.fail(&error),
            },
            QuarantineOp::Show { graph } => {
                match admin
                    .quarantine(QuarantineQuery::Detail { graph_id: graph })
                    .await
                {
                    Ok(reply) => self.say_all(describe_quarantine(&reply)),
                    Err(error) => self.fail(&error),
                }
            }
            QuarantineOp::Discard { graph, yes } => {
                let detail = match admin
                    .quarantine(QuarantineQuery::Detail { graph_id: graph })
                    .await
                {
                    Ok(QuarantineReply::Detail(detail)) => detail,
                    Ok(_) => return self.fail(&AdminError::Unexpected("quarantine detail")),
                    Err(error) => return self.fail(&error),
                };
                for line in describe_quarantine(&QuarantineReply::Detail(detail.clone())) {
                    self.say(line);
                }
                self.say(
                    "discarding removes this graph for good: its work is never resumed from it",
                );
                let proof = match self.confirmed(yes, "Discard this graph? [y/N] ") {
                    Ok(proof) => proof,
                    Err(code) => return code,
                };
                let op = operation_id();
                self.change(&admin, op.clone(), discard_quarantined(&detail, op, proof))
                    .await
            }
        }
    }

    pub(super) async fn graphs(&mut self, client: &WorkbenchClient, op: GraphsOp) -> u8 {
        let admin = RestartAdmin::new(client);
        if !self.require_management(&admin) {
            return exit::FAILURE;
        }
        let (graph, action, yes) = match op {
            GraphsOp::List => {
                return match admin.owned_graphs(OwnedGraphsQuery::Undisposed).await {
                    Ok(OwnedGraphsReply::Undisposed(graphs)) => {
                        self.say_all(describe_undisposed(&graphs))
                    }
                    Ok(_) => self.fail(&AdminError::Unexpected("undisposed query")),
                    Err(error) => self.fail(&error),
                };
            }
            GraphsOp::Show { graph } => (graph, None, false),
            GraphsOp::Release { graph, yes } => (graph, Some(GraphAction::Release), yes),
            GraphsOp::Resume { graph, yes } => (graph, Some(GraphAction::Resume), yes),
        };
        let view = match admin
            .owned_graphs(OwnedGraphsQuery::Detail { graph_ref: graph })
            .await
        {
            Ok(OwnedGraphsReply::Detail(view)) => view,
            Ok(_) => return self.fail(&AdminError::Unexpected("graph detail")),
            Err(error) => return self.fail(&error),
        };
        for line in describe_graph(&view) {
            self.say(line);
        }
        let Some(action) = action else {
            return exit::SUCCESS;
        };
        let (what, question) = match action {
            GraphAction::Release => (
                "release gives the graph up: its work is not resumed",
                "Release this graph? [y/N] ",
            ),
            GraphAction::Resume => (
                "resume continues the graph's work in this core",
                "Resume this graph? [y/N] ",
            ),
        };
        self.say(what);
        let status = match admin.status().await {
            Ok(status) => status,
            Err(error) => return self.fail(&error),
        };
        let proof = match self.confirmed(yes, question) {
            Ok(proof) => proof,
            Err(code) => return code,
        };
        let op = operation_id();
        let command = match dispose_graph(&view, action, op.clone(), status.core_epoch, proof) {
            Ok(command) => command,
            Err(error) => return self.refuse(&error),
        };
        let code = self.change(&admin, op.clone(), command).await;
        if let Ok(OwnedGraphsReply::Operation(view)) = admin
            .owned_graphs(OwnedGraphsQuery::Operation { operation_id: op })
            .await
        {
            for line in describe_graph_operation(&view) {
                self.say(line);
            }
        }
        code
    }

    fn say_all(&mut self, lines: Vec<String>) -> u8 {
        for line in lines {
            self.say(line);
        }
        exit::SUCCESS
    }

    /// Send one change, once, and read it to its end.
    async fn change(
        &mut self,
        admin: &RestartAdmin<'_>,
        op: AdminId,
        command: workbench_protocol::commands::Command,
    ) -> u8 {
        let sent = tokio::select! {
            biased;
            () = &mut self.env.interrupt => {
                self.warn("stopped waiting for the reply (Ctrl-C): the request may already have reached the core and was not cancelled, so its outcome is unknown");
                return exit::INTERRUPTED;
            }
            sent = admin.send(command) => sent,
        };
        let receipt = match sent {
            Ok(receipt) => receipt,
            Err(AdminError::Client(ClientError::Transport(_))) => {
                self.warn(format!(
                    "the reply was lost: the outcome is unknown and the request was not sent again; read operation {} once the core is back",
                    op.as_str()
                ));
                return self.settle_by_id(&op).await;
            }
            Err(error) => return self.fail(&error),
        };
        self.read_to_end(admin, &op, receipt).await
    }

    /// Reconnect once and read the operation by its own id.
    async fn settle_by_id(&mut self, op: &AdminId) -> u8 {
        let client = match self.connect().await {
            Ok(client) => client,
            Err(code) => return code,
        };
        let admin = RestartAdmin::new(&client);
        match admin.operation(op).await {
            Ok(Some(receipt)) => self.read_to_end(&admin, op, receipt).await,
            Ok(None) => {
                self.warn(format!(
                    "the core does not know operation {}: it may not have arrived. List again before asking again",
                    op.as_str()
                ));
                exit::FAILURE
            }
            Err(error) => self.fail(&error),
        }
    }

    async fn read_to_end(
        &mut self,
        admin: &RestartAdmin<'_>,
        op: &AdminId,
        mut receipt: AdminReceipt,
    ) -> u8 {
        for _ in 0..SETTLE_READS {
            if receipt.state == AdminOperationState::Completed {
                break;
            }
            tokio::select! {
                biased;
                () = &mut self.env.interrupt => return self.interrupted(op),
                () = tokio::time::sleep(SETTLE_PAUSE) => {}
            }
            match admin.operation(op).await {
                Ok(Some(next)) => receipt = next,
                Ok(None) => break,
                Err(error) => return self.fail(&error),
            }
        }
        if receipt.state != AdminOperationState::Completed {
            self.warn(format!(
                "operation {} has not finished: it goes on in the core and is not sent again",
                op.as_str()
            ));
        }
        for line in describe_receipt(&receipt) {
            self.say(line);
        }
        match (receipt.state, receipt.outcome) {
            (AdminOperationState::Completed, AdminOutcome::Succeeded) => exit::SUCCESS,
            _ => exit::FAILURE,
        }
    }

    /// After a refused restart request: say which undisposed graphs held it back.
    pub(super) async fn hint_undisposed(&mut self, admin: &RestartAdmin<'_>) {
        if let Ok(OwnedGraphsReply::Undisposed(graphs)) =
            admin.owned_graphs(OwnedGraphsQuery::Undisposed).await
            && graphs.count.get() > 0
        {
            self.warn(format!(
                "{} undisposed graph(s) are left from an earlier restart; see `evorch core graphs list`, then release or resume each, or discard a quarantined one with `evorch core quarantine list`",
                graphs.count.get()
            ));
        }
    }
}
