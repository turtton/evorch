//! A scripted management facade. It stands in for the core behind the real web
//! adapter: it answers management operations with typed replies and checks the
//! confirmed bindings the way the core's coordinator does (a changed target
//! revision or core epoch is a `Conflict`). It uses the trait's default
//! `authorize_management`, so only a verified principal ever reaches it.
//!
//! A positive answer from this fake proves the browser and adapter contract
//! only. It is not evidence of production authentication.
#![allow(dead_code)]

use crate::common::{core_status, cursor, server_hello, started};
use std::sync::Mutex;
use workbench_authority::IngressContext;
use workbench_protocol::commands::{Command, CommandDisposition, CommandReply, CommandResult};
use workbench_protocol::envelope::{
    ReplyEnvelope, ReplyPayload, ReplyResult, RequestBody, RequestEnvelope,
};
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::events::Subscribe;
use workbench_protocol::handshake::ServerHello;
use workbench_protocol::ids::ClientId;
use workbench_protocol::namespaces::lifecycle::quarantine::{
    QuarantineClaim, QuarantineCompletion, QuarantineEligibility, QuarantineItem, QuarantineReason,
    QuarantineReply,
};
use workbench_protocol::namespaces::lifecycle::restart::{
    AdminAuditDiagnostics, RestartCommand, RestartManagementStatus, RestartQuery, RestartReply,
    RestartTarget, RestartTargets,
};
use workbench_protocol::namespaces::lifecycle::{
    AdminId, AdminOperationState, AdminOutcome, AdminReceipt, LifecycleCommand, LifecycleQuery,
    LifecycleReply, LifecycleResult, RestartPhase, RestartStatus,
};
use workbench_protocol::queries::{Query, QueryReply};
use workbench_protocol::wire::{InstanceEpoch, Revision};
use workbench_web::{BackendSubscription, WebSession, WorkbenchBackend};

#[derive(Default)]
struct State {
    management_epoch: u64,
    target_revision: u64,
    /// Operation ids of restart requests that passed the binding check.
    restarts: Vec<String>,
    /// Every management action that reached the facade.
    calls: Vec<String>,
    /// Answer every management request with an error carrying this text.
    error_message: Option<String>,
    subscription: Option<Result<BackendSubscription, WorkbenchError>>,
    hello_epoch: Option<u64>,
    subscribes: Vec<Subscribe>,
    live: Option<tokio::sync::mpsc::Sender<workbench_protocol::events::EventEnvelope>>,
}

pub struct Facade {
    inner: Mutex<State>,
}

impl Facade {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(State {
                management_epoch: 2,
                target_revision: 1,
                ..State::default()
            }),
        }
    }

    pub fn set_target_revision(&self, revision: u64) {
        self.inner.lock().unwrap().target_revision = revision;
    }

    pub fn set_management_epoch(&self, epoch: u64) {
        self.inner.lock().unwrap().management_epoch = epoch;
    }

    pub fn set_hello_epoch(&self, epoch: u64) {
        self.inner.lock().unwrap().hello_epoch = Some(epoch);
    }

    pub fn fail_management_with_text(&self, text: &str) {
        self.inner.lock().unwrap().error_message = Some(text.to_owned());
    }

    pub fn script_subscription(&self, subscription: Result<BackendSubscription, WorkbenchError>) {
        self.inner.lock().unwrap().subscription = Some(subscription);
    }

    pub fn restarts(&self) -> Vec<String> {
        self.inner.lock().unwrap().restarts.clone()
    }

    pub fn calls(&self) -> Vec<String> {
        self.inner.lock().unwrap().calls.clone()
    }

    pub fn subscribes(&self) -> Vec<Subscribe> {
        self.inner.lock().unwrap().subscribes.clone()
    }

    /// Push an event into the open unscripted management stream.
    pub async fn push(&self, envelope: workbench_protocol::events::EventEnvelope) {
        let sender = self
            .inner
            .lock()
            .unwrap()
            .live
            .clone()
            .expect("no open stream");
        sender.send(envelope).await.unwrap();
    }

    pub fn has_open_stream(&self) -> bool {
        self.inner.lock().unwrap().live.is_some()
    }
}

fn id(text: &str) -> AdminId {
    AdminId::new(text).unwrap()
}

fn accepted(operation: &AdminId) -> ReplyPayload {
    ReplyPayload::Command(CommandReply {
        disposition: CommandDisposition::Accepted,
        submission_id: None,
        run_id: None,
        thread_revision: None,
        result: Some(Box::new(CommandResult::Lifecycle(
            LifecycleResult::Accepted(AdminReceipt {
                operation_id: operation.clone(),
                state: AdminOperationState::Started,
                outcome: AdminOutcome::Pending,
                audit_warning: false,
            }),
        ))),
    })
}

fn lifecycle_reply(reply: LifecycleReply) -> ReplyResult {
    ReplyResult::Success(ReplyPayload::Query(QueryReply::Lifecycle(reply)))
}

fn conflict(text: &str) -> ReplyResult {
    ReplyResult::Error(WorkbenchError::new(
        ErrorCode::Conflict,
        text,
        RetryAdvice::Never,
    ))
}

impl WorkbenchBackend for Facade {
    fn protect_output(&self, output: serde_json::Value) -> serde_json::Value {
        output
    }

    async fn handle_request(
        &self,
        _session: &WebSession,
        request: RequestEnvelope,
        _context: IngressContext,
    ) -> ReplyEnvelope {
        let mut state = self.inner.lock().unwrap();
        let action = request.body.admin_action();
        state.calls.push(format!("{action:?}"));
        let request_id = request.request_id.clone();
        if action.is_some()
            && let Some(text) = state.error_message.clone()
        {
            return ReplyEnvelope {
                request_id,
                result: conflict(&text),
            };
        }
        let epoch = InstanceEpoch(state.management_epoch);
        let revision = Revision(state.target_revision);
        let result = match request.body {
            RequestBody::Query(Query::Lifecycle(LifecycleQuery::Restart(
                RestartQuery::Targets,
            ))) => lifecycle_reply(LifecycleReply::Restart(RestartReply::Targets(
                RestartTargets {
                    core_epoch: epoch,
                    revision,
                    targets: vec![RestartTarget {
                        id: id("current"),
                        revision,
                    }],
                    truncated: false,
                },
            ))),
            RequestBody::Query(Query::Lifecycle(LifecycleQuery::Restart(RestartQuery::Status))) => {
                lifecycle_reply(LifecycleReply::Restart(RestartReply::Status(
                    RestartManagementStatus {
                        audit: AdminAuditDiagnostics::default(),
                        core_epoch: epoch,
                        status: RestartStatus {
                            restart_id: None,
                            phase: RestartPhase::Running,
                            revision: 0,
                            blockers: Vec::new(),
                            blocker_total: 0,
                            blocker_omitted: 0,
                            active_producers: 0,
                            cancel_reason: None,
                        },
                    },
                )))
            }
            RequestBody::Query(Query::Lifecycle(LifecycleQuery::OperationStatus {
                operation_id,
            })) => {
                let known = state.restarts.iter().any(|id| id == operation_id.as_str());
                lifecycle_reply(LifecycleReply::OperationStatus(AdminReceipt {
                    operation_id,
                    state: if known {
                        AdminOperationState::Completed
                    } else {
                        AdminOperationState::Unknown
                    },
                    outcome: if known {
                        AdminOutcome::Succeeded
                    } else {
                        AdminOutcome::Unknown
                    },
                    audit_warning: false,
                }))
            }
            RequestBody::Query(Query::Lifecycle(LifecycleQuery::Quarantine(_))) => {
                lifecycle_reply(LifecycleReply::Quarantine(QuarantineReply::List {
                    items: vec![QuarantineItem {
                        graph_id: id("graph-1"),
                        revision: Revision(3),
                        cold: true,
                        restart_tx_id: id("tx-1"),
                        capture_id: id("capture-1"),
                        reason: QuarantineReason::ColdBoot,
                        eligibility: QuarantineEligibility::Open,
                        claim: QuarantineClaim::Absent,
                        completion: QuarantineCompletion::NotStarted,
                        members: 1,
                    }],
                    revision: Revision(3),
                    truncated: false,
                }))
            }
            RequestBody::Command(Command::Lifecycle(LifecycleCommand::Restart(
                RestartCommand::Request(request),
            ))) => {
                let binding = request.binding;
                if binding.expected_core_epoch != epoch
                    || binding.target_revision != revision
                    || binding.target_id.as_str() != "current"
                {
                    conflict("the confirmed target is no longer current")
                } else {
                    state
                        .restarts
                        .push(binding.operation_id.as_str().to_owned());
                    ReplyResult::Success(accepted(&binding.operation_id))
                }
            }
            _ => ReplyResult::Success(ReplyPayload::Ack),
        };
        ReplyEnvelope { request_id, result }
    }

    async fn subscribe(
        &self,
        _client: ClientId,
        subscribe: Subscribe,
        _context: IngressContext,
    ) -> Result<BackendSubscription, WorkbenchError> {
        let mut state = self.inner.lock().unwrap();
        state.subscribes.push(subscribe);
        if let Some(scripted) = state.subscription.take() {
            return scripted;
        }
        // An unscripted subscription is an idle replay from 7:0; the sender
        // stays here so a test can push events into the open stream.
        let (sender, events) = tokio::sync::mpsc::channel(256);
        state.live = Some(sender);
        Ok(BackendSubscription {
            started: started(cursor(7, 0)),
            events,
        })
    }

    async fn server_hello(&self, _context: &IngressContext) -> Result<ServerHello, WorkbenchError> {
        let mut hello = server_hello();
        if let Some(epoch) = self.inner.lock().unwrap().hello_epoch {
            hello.instance_epoch = InstanceEpoch(epoch);
        }
        hello.status = core_status();
        Ok(hello)
    }
}
