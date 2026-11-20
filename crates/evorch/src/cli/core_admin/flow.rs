use super::args::AdminCommand;
use core_launcher::{
    ConnectedCore,
    handoff::diagnostics::{
        connect::{Attempt, ConnectTarget, attempt},
        reconnect::{
            ReconnectPolicy, Reconnected, Target, clock_now, reconnect_after_planned_restart,
        },
    },
};
use std::{future::Future, io::Write, pin::Pin};
use workbench_client::{
    ClientConfig, ClientError, Subscription, TransportError, WorkbenchClient,
    restart::{
        confirmation::confirm,
        intent::{
            IntentError, cancel_restart, cancellable, interrupt_shell, plan_restart,
            request_restart, select_blocker, stale_binding,
        },
        reconnect::{EvidenceObserver, PlannedEvidence},
        resume::{check_started, plan_resume, subscribe_request},
        session::{AdminError, RestartAdmin},
        view::{
            ReconnectView, describe_blockers, describe_failure, describe_receipt,
            describe_reconnect, describe_status, describe_targets, phase_name,
        },
    },
};
use workbench_protocol::{
    events::{EventCursor, SubscriptionScope, UiEvent},
    namespaces::lifecycle::{
        AdminId, AdminOperationState, AdminOutcome, AdminReceipt, LifecycleEvent,
    },
    wire::InstanceEpoch,
};

use super::super::core_client::exit;

/// Everything the command needs from outside, so that the end-to-end tests can
/// give it a socket, a terminal and a Ctrl-C of their own.
pub struct AdminEnv<'a> {
    pub target: ConnectTarget,
    pub client: ClientConfig,
    pub policy: ReconnectPolicy,
    /// Whether a person can be asked.
    pub terminal: bool,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    /// Ask the person; only used when `terminal` and no `--yes` was given.
    pub approve: &'a mut dyn FnMut(&str) -> bool,
    /// Completes when the person pressed Ctrl-C.
    pub interrupt: Pin<&'a mut dyn Future<Output = ()>>,
}

pub async fn run(command: AdminCommand, env: AdminEnv<'_>) -> u8 {
    Flow { env }.dispatch(command).await
}

struct Flow<'a> {
    env: AdminEnv<'a>,
}

enum Waited {
    Interrupted,
    Reread,
    Resubscribe,
    Lost,
}

enum ReadFailed {
    Lost,
    Fatal(String),
}

type Rebuilt = (WorkbenchClient, Subscription, Option<AdminReceipt>);

fn operation_id() -> AdminId {
    AdminId::new(format!("cli-{}", uuid::Uuid::new_v4())).expect("a uuid is an admin id")
}

fn lost(error: &AdminError) -> bool {
    matches!(error, AdminError::Client(ClientError::Transport(_)))
}

impl Flow<'_> {
    fn say(&mut self, text: impl AsRef<str>) {
        let _ = writeln!(self.env.out, "{}", text.as_ref());
    }

    fn warn(&mut self, text: impl AsRef<str>) {
        let _ = writeln!(self.env.err, "{}", text.as_ref());
    }

    fn fail(&mut self, error: &AdminError) -> u8 {
        let text = match error {
            AdminError::Client(error) => describe_failure(error),
            other => other.to_string(),
        };
        self.warn(text);
        exit::FAILURE
    }

    fn refuse(&mut self, error: &IntentError) -> u8 {
        self.warn(format!("{error}; nothing was sent"));
        exit::FAILURE
    }

    /// Connect to the running core. Never starts one.
    async fn connect(&mut self) -> Result<WorkbenchClient, u8> {
        match attempt(&self.env.target, &self.env.client).await {
            Ok(Attempt::Ready(core)) => Ok(core.client),
            Ok(Attempt::NotRunning) => {
                self.warn(
                    "no evorch core is running for this configuration; the management CLI never starts one",
                );
                Err(exit::FAILURE)
            }
            Ok(Attempt::NotReady(why)) => {
                self.warn(format!(
                    "the core is not ready ({why}); try again when it is"
                ));
                Err(exit::FAILURE)
            }
            Err(error) => {
                self.warn(format!("cannot reach the core: {error}"));
                Err(exit::FAILURE)
            }
        }
    }

    async fn dispatch(mut self, command: AdminCommand) -> u8 {
        let client = match self.connect().await {
            Ok(client) => client,
            Err(code) => return code,
        };
        match command {
            AdminCommand::Targets => match RestartAdmin::new(&client).targets().await {
                Ok(targets) => {
                    for line in describe_targets(&targets) {
                        self.say(line);
                    }
                    exit::SUCCESS
                }
                Err(error) => self.fail(&error),
            },
            AdminCommand::Status => self.status(&client).await,
            AdminCommand::Request {
                target,
                yes,
                no_wait,
            } => self.request(client, target, yes, no_wait).await,
            AdminCommand::Cancel => self.cancel(&client).await,
            AdminCommand::Interrupt { blocker, yes } => {
                self.interrupt_shell(&client, blocker, yes).await
            }
        }
    }

    async fn status(&mut self, client: &WorkbenchClient) -> u8 {
        match RestartAdmin::new(client).status().await {
            Ok(status) => {
                for line in describe_status(&status) {
                    self.say(line);
                }
                exit::SUCCESS
            }
            Err(error) => self.fail(&error),
        }
    }

    /// The person's yes, or the reason there is none. Asking only happens at a
    /// terminal and only without `--yes`.
    fn confirmed(
        &mut self,
        yes: bool,
        question: &str,
    ) -> Result<workbench_client::restart::confirmation::Confirmed, u8> {
        let approved = !yes && self.env.terminal && (self.env.approve)(question);
        confirm(self.env.terminal, yes, approved).map_err(|_| {
            self.warn(
                "this needs your confirmation: pass --yes, or run in a terminal and answer yes; nothing was sent",
            );
            exit::FAILURE
        })
    }

    async fn request(
        &mut self,
        client: WorkbenchClient,
        target: workbench_client::restart::intent::TargetSelector,
        yes: bool,
        no_wait: bool,
    ) -> u8 {
        let admin = RestartAdmin::new(&client);
        if !admin.offers_restart() {
            self.warn("this core does not offer safe restart; nothing was sent");
            return exit::FAILURE;
        }
        let targets = match admin.targets().await {
            Ok(targets) => targets,
            Err(error) => return self.fail(&error),
        };
        let op = operation_id();
        let binding = match plan_restart(&targets, &target, op.clone()) {
            Ok(binding) => binding,
            Err(error) => return self.refuse(&error),
        };
        let status = match admin.status().await {
            Ok(status) => status,
            Err(error) => return self.fail(&error),
        };
        for line in describe_status(&status) {
            self.say(line);
        }
        self.say(format!(
            "restart target {} (revision {}) on core epoch {}: running work stops at safe points and continues in the new core",
            binding.target_id.as_str(),
            binding.target_revision.get(),
            binding.expected_core_epoch.get()
        ));
        let proof = match self.confirmed(yes, "Restart the core now? [y/N] ") {
            Ok(proof) => proof,
            Err(code) => return code,
        };
        let sent = tokio::select! {
            biased;
            () = &mut self.env.interrupt => {
                self.warn("stopped waiting for the reply (Ctrl-C): the request may already have reached the core and was not cancelled, so its outcome is unknown");
                return exit::INTERRUPTED;
            }
            sent = admin.send(request_restart(binding, proof)) => sent,
        };
        match sent {
            Ok(receipt) => {
                for line in describe_receipt(&receipt) {
                    self.say(line);
                }
                if no_wait {
                    self.say("not waiting (--no-wait); follow with `evorch core restart status`");
                    return exit::SUCCESS;
                }
                self.follow(client, op, receipt, None).await
            }
            Err(AdminError::Client(ClientError::Transport(TransportError::OutcomeUnknown {
                request_id,
            }))) => {
                self.warn(format!(
                    "the reply to request {request_id} was lost: the outcome is unknown and the request was not sent again"
                ));
                self.settle_unknown(op).await
            }
            Err(error) => self.fail(&error),
        }
    }

    async fn cancel(&mut self, client: &WorkbenchClient) -> u8 {
        let admin = RestartAdmin::new(client);
        let status = match admin.status().await {
            Ok(status) => status,
            Err(error) => return self.fail(&error),
        };
        if !cancellable(status.status.phase) {
            self.warn(format!(
                "a restart in phase `{}` cannot be cancelled; nothing was sent",
                phase_name(status.status.phase)
            ));
            return exit::FAILURE;
        }
        let command = match cancel_restart(&status, operation_id()) {
            Ok(command) => command,
            Err(error) => return self.refuse(&error),
        };
        match admin.send(command).await {
            Ok(receipt) => {
                for line in describe_receipt(&receipt) {
                    self.say(line);
                }
                exit::SUCCESS
            }
            Err(error) => self.fail(&error),
        }
    }

    async fn interrupt_shell(
        &mut self,
        client: &WorkbenchClient,
        selector: workbench_client::restart::intent::BlockerSelector,
        yes: bool,
    ) -> u8 {
        let admin = RestartAdmin::new(client);
        if !admin.offers_restart() {
            self.warn("this core does not offer safe restart; nothing was sent");
            return exit::FAILURE;
        }
        let status = match admin.status().await {
            Ok(status) => status,
            Err(error) => return self.fail(&error),
        };
        let blockers = match admin.shell_blockers().await {
            Ok(blockers) => blockers,
            Err(error) => return self.fail(&error),
        };
        for line in describe_blockers(&blockers) {
            self.say(line);
        }
        let chosen = match select_blocker(&blockers, &selector) {
            Ok(chosen) => chosen.clone(),
            Err(error) => return self.refuse(&error),
        };
        if let Some(element) = stale_binding(&chosen.binding, &status) {
            return self.refuse(&IntentError::Stale(element));
        }
        self.say(format!(
            "interrupt the shell of run {} (blocker {}): the command running in it is stopped",
            chosen.run,
            chosen.binding.blocker_identity.as_str()
        ));
        let proof = match self.confirmed(yes, "Interrupt this shell? [y/N] ") {
            Ok(proof) => proof,
            Err(code) => return code,
        };
        match admin
            .send(interrupt_shell(chosen.binding, operation_id(), proof))
            .await
        {
            Ok(receipt) => {
                for line in describe_receipt(&receipt) {
                    self.say(line);
                }
                exit::SUCCESS
            }
            Err(error) => self.fail(&error),
        }
    }

    fn interrupted(&mut self, op: &AdminId) -> u8 {
        self.warn(format!(
            "stopped waiting (Ctrl-C). Nothing was cancelled or stopped: operation {} goes on in the core. Read it with `evorch core restart status`",
            op.as_str()
        ));
        exit::INTERRUPTED
    }

    fn finish(&mut self, receipt: &AdminReceipt) -> u8 {
        for line in describe_receipt(receipt) {
            self.say(line);
        }
        match receipt.outcome {
            AdminOutcome::Succeeded => exit::SUCCESS,
            _ => exit::FAILURE,
        }
    }

    /// The reply was lost, or the connection ended with nothing said about a
    /// restart. Ask once, connect-only, for the operation by its own id, on a
    /// session rebuilt as the epochs require. `Err` is the exit code.
    async fn settle_once(
        &mut self,
        op: &AdminId,
        held: Option<EventCursor>,
    ) -> Result<Rebuilt, u8> {
        enum Settled {
            Interrupted,
            Unreachable,
            Session(Result<Rebuilt, String>),
        }
        let settled = {
            let target = &self.env.target;
            let config = &self.env.client;
            let interrupt = &mut self.env.interrupt;
            tokio::select! {
                biased;
                () = &mut *interrupt => Settled::Interrupted,
                settled = async {
                    match attempt(target, config).await {
                        Ok(Attempt::Ready(core)) => {
                            Settled::Session(rebuild(core, held, op.clone()).await)
                        }
                        _ => Settled::Unreachable,
                    }
                } => settled,
            }
        };
        match settled {
            Settled::Interrupted => Err(self.interrupted(op)),
            Settled::Unreachable => {
                self.warn(format!(
                    "the core cannot be reached, so operation {} is unknown. It was not sent again; read `evorch core restart status` once the core is back",
                    op.as_str()
                ));
                Err(exit::FAILURE)
            }
            Settled::Session(Ok((client, subscription, Some(receipt)))) => {
                Ok((client, subscription, Some(receipt)))
            }
            Settled::Session(Ok((_, _, None))) => {
                self.warn(format!(
                    "the core does not know operation {}: it may not have arrived. Check `evorch core restart status` before asking again",
                    op.as_str()
                ));
                Err(exit::FAILURE)
            }
            Settled::Session(Err(error)) => {
                self.warn(format!(
                    "the core answered but the session could not be rebuilt: {error}"
                ));
                Err(exit::FAILURE)
            }
        }
    }

    async fn settle_unknown(&mut self, op: AdminId) -> u8 {
        match self.settle_once(&op, None).await {
            Ok((client, subscription, Some(receipt))) => {
                for line in describe_receipt(&receipt) {
                    self.say(line);
                }
                self.follow(client, op, receipt, Some(subscription)).await
            }
            Ok(_) => exit::FAILURE,
            Err(code) => code,
        }
    }

    async fn read_state(
        client: &WorkbenchClient,
        op: &AdminId,
    ) -> Result<
        (
            Option<AdminReceipt>,
            workbench_protocol::namespaces::lifecycle::restart::RestartManagementStatus,
        ),
        ReadFailed,
    > {
        let admin = RestartAdmin::new(client);
        let result = async {
            let receipt = admin.operation(op).await?;
            let status = admin.status().await?;
            Ok::<_, AdminError>((receipt, status))
        }
        .await;
        result.map_err(|error| {
            if lost(&error) {
                ReadFailed::Lost
            } else {
                ReadFailed::Fatal(match error {
                    AdminError::Client(error) => describe_failure(&error),
                    other => other.to_string(),
                })
            }
        })
    }

    async fn wait_change(
        &mut self,
        subscription: &mut Subscription,
        observer: &mut EvidenceObserver,
        epoch: InstanceEpoch,
        cursor: &mut Option<EventCursor>,
    ) -> Waited {
        loop {
            let event = tokio::select! {
                biased;
                () = &mut self.env.interrupt => return Waited::Interrupted,
                event = subscription.next() => event,
            };
            let Some(event) = event else {
                return Waited::Lost;
            };
            *cursor = Some(event.cursor);
            if event.event.ends_subscription() {
                return Waited::Resubscribe;
            }
            match event.event {
                UiEvent::Lifecycle(LifecycleEvent::CoreStatusChanged { status }) => {
                    observer.observe_core(&status, epoch, clock_now());
                    return Waited::Reread;
                }
                UiEvent::Lifecycle(LifecycleEvent::AdminChanged { .. }) => return Waited::Reread,
                _ => {}
            }
        }
    }

    /// Follow the operation until the core says it completed.
    async fn follow(
        &mut self,
        client: WorkbenchClient,
        op: AdminId,
        first: AdminReceipt,
        subscription: Option<Subscription>,
    ) -> u8 {
        let mut client = client;
        let mut receipt = first;
        let mut observer = EvidenceObserver::default();
        let mut cursor: Option<EventCursor> = None;
        let mut pending = subscription;
        let mut last_phase = None;
        let started_epoch = client.server_hello().instance_epoch;
        loop {
            let mut subscription = match pending.take() {
                Some(subscription) => subscription,
                None => match tokio::select! {
                    biased;
                    () = &mut self.env.interrupt => return self.interrupted(&op),
                    started = client.subscribe(workbench_protocol::events::Subscribe {
                        scope: SubscriptionScope::Management,
                        after: None,
                    }) => started,
                } {
                    Ok(subscription) => subscription,
                    Err(error) => {
                        if matches!(error, ClientError::Transport(_)) {
                            match self
                                .recover(observer.evidence().cloned(), &op, cursor, started_epoch)
                                .await
                            {
                                Ok((new_client, sub, found)) => {
                                    client = new_client;
                                    pending = Some(sub);
                                    observer = EvidenceObserver::default();
                                    if let Some(found) = found {
                                        receipt = found;
                                    }
                                    continue;
                                }
                                Err(code) => return code,
                            }
                        }
                        return self.fail(&AdminError::Client(error));
                    }
                },
            };
            cursor = Some(subscription.started().high_water);
            let epoch = client.server_hello().instance_epoch;
            let mut lost_connection = false;
            loop {
                match tokio::select! {
                    biased;
                    () = &mut self.env.interrupt => return self.interrupted(&op),
                    read = Self::read_state(&client, &op) => read,
                } {
                    Ok((found, status)) => {
                        observer.observe_status(&status, clock_now());
                        if last_phase != Some(status.status.phase) {
                            last_phase = Some(status.status.phase);
                            self.warn(format!(
                                "restart phase: {}",
                                phase_name(status.status.phase)
                            ));
                        }
                        if let Some(found) = found {
                            receipt = found;
                        }
                        if receipt.state == AdminOperationState::Completed {
                            if epoch != started_epoch {
                                self.say(format!(
                                    "core restarted: epoch {} to {}",
                                    started_epoch.get(),
                                    epoch.get()
                                ));
                            }
                            return self.finish(&receipt);
                        }
                    }
                    Err(ReadFailed::Lost) => {
                        lost_connection = true;
                        break;
                    }
                    Err(ReadFailed::Fatal(text)) => {
                        self.warn(text);
                        return exit::FAILURE;
                    }
                }
                match self
                    .wait_change(&mut subscription, &mut observer, epoch, &mut cursor)
                    .await
                {
                    Waited::Interrupted => return self.interrupted(&op),
                    Waited::Reread => {}
                    Waited::Resubscribe => break,
                    Waited::Lost => {
                        lost_connection = true;
                        break;
                    }
                }
            }
            if !lost_connection {
                continue;
            }
            match self
                .recover(observer.evidence().cloned(), &op, cursor, started_epoch)
                .await
            {
                Ok((new_client, sub, found)) => {
                    client = new_client;
                    pending = Some(sub);
                    observer = EvidenceObserver::default();
                    if let Some(found) = found {
                        receipt = found;
                    }
                }
                Err(code) => return code,
            }
        }
    }

    /// The connection ended. Only a restart the core announced is waited for;
    /// the wait is connect-only, within the fixed deadlines, and the session is
    /// rebuilt from a snapshot of the successor, never from the old cursor.
    async fn recover(
        &mut self,
        evidence: Option<PlannedEvidence>,
        op: &AdminId,
        held: Option<EventCursor>,
        _started_epoch: InstanceEpoch,
    ) -> Result<Rebuilt, u8> {
        let Some(evidence) = evidence else {
            self.warn(format!(
                "the connection to the core ended and the core had not said a restart was under way, so this is not treated as a planned restart (operation {} was not sent again)",
                op.as_str()
            ));
            return self.settle_once(op, held).await;
        };
        self.warn("the core is restarting: waiting for its successor (connect only; no new core is started)");
        let connector = Target {
            target: &self.env.target,
            client: &self.env.client,
        };
        let policy = self.env.policy;
        let mut printed: Option<(workbench_client::restart::reconnect::Stage, u64)> = None;
        let err = &mut *self.env.err;
        let wait = reconnect_after_planned_restart(
            &connector,
            &evidence,
            policy,
            |view: ReconnectView| {
                let key = (view.stage, view.remaining.as_secs() / 10);
                if printed != Some(key) {
                    printed = Some(key);
                    let _ = writeln!(err, "{}", describe_reconnect(&view));
                }
            },
            |core: ConnectedCore| rebuild(core, held, op.clone()),
        );
        let outcome = tokio::select! {
            biased;
            () = &mut self.env.interrupt => return Err(self.interrupted(op)),
            outcome = wait => outcome,
        };
        match outcome {
            Ok(Reconnected::Successor { epoch, value }) => {
                self.warn(format!(
                    "reconnected to the successor core (epoch {} to {}); session rebuilt from a snapshot",
                    evidence.predecessor_epoch.get(),
                    epoch.get()
                ));
                Ok(value)
            }
            Ok(Reconnected::Predecessor {
                value: (_, _, found),
            }) => {
                self.warn("the core did not restart: it still serves at the same epoch (the restart was cancelled or rolled back)");
                if let Some(found) = &found {
                    for line in describe_receipt(found) {
                        self.say(line);
                    }
                }
                Err(exit::FAILURE)
            }
            Err(error) => {
                self.warn(format!(
                    "{error}; operation {} was accepted and is not resent. Read it with `evorch core restart status`",
                    op.as_str()
                ));
                Err(exit::FAILURE)
            }
        }
    }
}

/// The session on a core that came back: choose the plan from the epochs, read
/// the snapshot that plan requires, and ask for the operation by its own id.
async fn rebuild(
    core: ConnectedCore,
    held: Option<EventCursor>,
    op: AdminId,
) -> Result<Rebuilt, String> {
    let client = core.client;
    let epoch = client.server_hello().instance_epoch;
    let plan = plan_resume(held, epoch);
    let subscription = client
        .subscribe(subscribe_request(SubscriptionScope::Management, &plan))
        .await
        .map_err(|e| e.to_string())?;
    check_started(&plan, subscription.started(), epoch).map_err(|e| e.to_string())?;
    let receipt = RestartAdmin::new(&client)
        .operation(&op)
        .await
        .map_err(|e| e.to_string())?;
    Ok((client, subscription, receipt))
}
