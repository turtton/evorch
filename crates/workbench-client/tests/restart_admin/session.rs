use crate::support::*;
use workbench_client::{
    ClientError, TransportError,
    restart::{
        confirmation::confirm,
        intent::{TargetSelector, plan_restart, request_restart},
        session::{AdminError, RestartAdmin},
    },
};
use workbench_protocol::{
    ErrorCode, RetryAdvice, WorkbenchError,
    commands::{CommandDisposition, CommandReply, CommandResult},
    envelope::{ReplyPayload, ReplyResult, RequestBody},
    namespaces::lifecycle::{
        AdminId, AdminOperationState, AdminOutcome, AdminReceipt, LifecycleReply, LifecycleResult,
        restart::{RestartReply, RestartTarget, RestartTargets},
    },
    queries::QueryReply,
    wire::{InstanceEpoch, Revision},
};

fn targets() -> RestartTargets {
    RestartTargets {
        core_epoch: InstanceEpoch(7),
        revision: Revision(1),
        targets: vec![RestartTarget {
            id: AdminId::new("current").unwrap(),
            revision: Revision(5),
        }],
        truncated: false,
    }
}

fn receipt(op: &str, state: AdminOperationState, outcome: AdminOutcome) -> AdminReceipt {
    AdminReceipt {
        operation_id: AdminId::new(op).unwrap(),
        state,
        outcome,
        audit_warning: false,
    }
}

#[tokio::test]
async fn a_command_whose_reply_is_lost_is_unknown_and_is_never_sent_again() {
    let (client, mut server) = connected().await;
    let binding = plan_restart(
        &targets(),
        &TargetSelector::Current,
        AdminId::new("op-1").unwrap(),
    )
    .unwrap();
    let command = request_restart(binding, confirm(false, true, false).unwrap());
    let call = tokio::spawn({
        let client = client.clone();
        async move { RestartAdmin::new(&client).send(command).await }
    });
    let sent = server.recv_request().await.expect("the request");
    assert!(matches!(sent.body, RequestBody::Command(_)));
    // The ACK is lost: the connection ends with no reply.
    server.close();
    let Err(AdminError::Client(ClientError::Transport(TransportError::OutcomeUnknown {
        request_id,
    }))) = call.await.unwrap()
    else {
        panic!("an unknown outcome");
    };
    assert_eq!(request_id, sent.request_id);
    // Everything the client had to say has been said: no second request is waiting.
    let mut next = Box::pin(server.recv_request());
    let pending = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(next.as_mut(), cx).is_pending())
    })
    .await;
    assert!(pending, "nothing was sent again");
}

#[tokio::test]
async fn the_receipt_comes_from_the_accepted_result() {
    let (client, mut server) = connected().await;
    let binding = plan_restart(
        &targets(),
        &TargetSelector::Current,
        AdminId::new("op-2").unwrap(),
    )
    .unwrap();
    let command = request_restart(binding, confirm(false, true, false).unwrap());
    let call = tokio::spawn({
        let client = client.clone();
        async move { RestartAdmin::new(&client).send(command).await }
    });
    let request = server.recv_request().await.unwrap();
    let expected = receipt("op-2", AdminOperationState::Pending, AdminOutcome::Pending);
    server.reply(
        &request.request_id,
        ok(ReplyPayload::Command(CommandReply {
            disposition: CommandDisposition::Accepted,
            result: Some(Box::new(CommandResult::Lifecycle(
                LifecycleResult::Accepted(expected.clone()),
            ))),
            ..command_reply()
        })),
    );
    assert_eq!(call.await.unwrap(), Ok(expected));
}

#[tokio::test]
async fn an_operation_the_core_does_not_know_is_none_and_a_known_one_is_read() {
    let (client, mut server) = connected().await;
    let id = AdminId::new("op-3").unwrap();
    let asked = tokio::spawn({
        let client = client.clone();
        let id = id.clone();
        async move { RestartAdmin::new(&client).operation(&id).await }
    });
    let request = server.recv_request().await.unwrap();
    server.reply(
        &request.request_id,
        ReplyResult::Error(WorkbenchError::new(
            ErrorCode::NotFound,
            "unknown",
            RetryAdvice::Never,
        )),
    );
    assert_eq!(asked.await.unwrap(), Ok(None));

    let known = receipt(
        "op-3",
        AdminOperationState::Completed,
        AdminOutcome::Succeeded,
    );
    let asked = tokio::spawn({
        let client = client.clone();
        async move { RestartAdmin::new(&client).operation(&id).await }
    });
    let request = server.recv_request().await.unwrap();
    server.reply(
        &request.request_id,
        ok(ReplyPayload::Query(QueryReply::Lifecycle(
            LifecycleReply::OperationStatus(known.clone()),
        ))),
    );
    assert_eq!(asked.await.unwrap(), Ok(Some(known)));
}

#[tokio::test]
async fn targets_are_read_with_their_epoch() {
    let (client, mut server) = connected().await;
    let asked = tokio::spawn({
        let client = client.clone();
        async move { RestartAdmin::new(&client).targets().await }
    });
    let request = server.recv_request().await.unwrap();
    server.reply(
        &request.request_id,
        ok(ReplyPayload::Query(QueryReply::Lifecycle(
            LifecycleReply::Restart(RestartReply::Targets(targets())),
        ))),
    );
    assert_eq!(asked.await.unwrap(), Ok(targets()));
}

#[tokio::test]
async fn safe_restart_is_offered_only_when_the_core_advertised_it() {
    let (client, _server) = connected().await;
    // The test core advertises what is implemented, which excludes SafeRestart.
    assert!(!RestartAdmin::new(&client).offers_restart());
}
