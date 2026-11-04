//! Handshake, request/reply routing and connection loss.

mod support;

use support::*;
use workbench_client::fake::FakeTransport;
use workbench_client::{ClientError, ConnectionState, TransportError, WorkbenchClient};
use workbench_protocol::commands::*;
use workbench_protocol::envelope::*;
use workbench_protocol::errors::*;
use workbench_protocol::framing::encode_frame;
use workbench_protocol::handshake::*;
use workbench_protocol::ids::*;
use workbench_protocol::queries::*;
use workbench_protocol::wire::*;

// --- handshake -----------------------------------------------------------

#[tokio::test]
async fn the_first_frame_is_the_hello_and_it_names_what_the_client_needs() {
    let (transport, mut server) = FakeTransport::pair();
    let hello = server_hello(WireLimits::default());
    let (client, client_hello) = tokio::join!(
        async { WorkbenchClient::connect(transport, config()).await.unwrap() },
        server.handshake(&hello),
    );
    assert_eq!(client_hello.supported_versions, [ProtocolVersion::CURRENT]);
    let required: Vec<&str> = client_hello
        .required_capabilities
        .iter()
        .map(CapabilityName::as_str)
        .collect();
    assert_eq!(required, ["commands.v1", "queries.v1"]);
    assert_eq!(client_hello.client_instance_id.as_str(), "client-1");
    assert_eq!(client.server_hello(), &hello);
    assert_eq!(client.connection_state(), ConnectionState::Connected);
}

#[tokio::test]
async fn a_rejected_handshake_surfaces_the_core_reason() {
    let (transport, mut server) = FakeTransport::pair();
    let rejected = HandshakeRejected {
        reason: RejectReason::UnsupportedVersion,
        message: "too old".into(),
        supported_versions: vec![ProtocolVersion(2)],
        missing_capabilities: vec![],
        recommended_action: RecommendedAction::UpgradeClient,
    };
    let (result, _) = tokio::join!(WorkbenchClient::connect(transport, config()), async {
        server.recv_hello().await;
        server.send(&HandshakeReply::Rejected(rejected.clone()));
    },);
    assert_eq!(result.err(), Some(ClientError::HandshakeRejected(rejected)));
}

#[tokio::test]
async fn an_accept_that_contradicts_the_hello_is_a_protocol_violation() {
    type Mutate = fn(&mut ServerHello);
    let cases: [(&str, Mutate); 4] = [
        ("unoffered version", |h| {
            h.protocol_version = ProtocolVersion(9)
        }),
        ("missing required capability", |h| {
            h.capabilities.retain(|c| *c != Capability::Queries)
        }),
        ("unusable limits", |h| h.limits.max_frame_bytes = 0),
        ("other core", |h| {
            h.core = CoreIdentity::new("other").unwrap()
        }),
    ];
    for (name, mutate) in cases {
        let (transport, mut server) = FakeTransport::pair();
        let mut hello = server_hello(WireLimits::default());
        mutate(&mut hello);
        let mut cfg = config();
        cfg.desired_core = Some(CoreIdentity::new("core-abc").unwrap());
        let (result, _) = tokio::join!(WorkbenchClient::connect(transport, cfg), async {
            server.recv_hello().await;
            server.accept(&hello);
        });
        assert!(
            matches!(
                result.err(),
                Some(ClientError::Transport(TransportError::ProtocolViolation(_)))
            ),
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_core_that_hangs_up_during_the_handshake_is_a_disconnect() {
    let (transport, mut server) = FakeTransport::pair();
    let (result, _) = tokio::join!(WorkbenchClient::connect(transport, config()), async {
        server.recv_hello().await;
        server.close();
    });
    assert_eq!(
        result.err(),
        Some(ClientError::Transport(TransportError::Disconnected))
    );
}

// --- requests and replies --------------------------------------------------

#[tokio::test]
async fn a_command_round_trips_and_carries_the_session() {
    let (client, mut server) = connected().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.command(stop_chat("thr-9")).await }
    });
    let request = server.recv_request().await.unwrap();
    assert_eq!(request.session_id, *client.session_id());
    assert_eq!(request.body, RequestBody::Command(stop_chat("thr-9")));
    server.reply(
        &request.request_id,
        ok(ReplyPayload::Command(command_reply())),
    );
    assert_eq!(call.await.unwrap().unwrap(), command_reply());
}

#[tokio::test]
async fn replies_may_arrive_in_any_order() {
    let (client, mut server) = connected().await;
    let calls: Vec<_> = (0..3)
        .map(|n| {
            let client = client.clone();
            let request_id = RequestId::new(format!("probe-{n}")).unwrap();
            tokio::spawn(async move {
                let query = Query::GetCommandReceipt {
                    request_id: request_id.clone(),
                };
                (request_id, client.query(query).await)
            })
        })
        .collect();
    let mut requests = Vec::new();
    for _ in 0..3 {
        requests.push(server.recv_request().await.unwrap());
    }
    // Answer last-received first, echoing which probe each request asked about.
    for request in requests.iter().rev() {
        let RequestBody::Query(Query::GetCommandReceipt { request_id: probe }) = &request.body
        else {
            panic!("unexpected request {request:?}");
        };
        server.reply(
            &request.request_id,
            ok(ReplyPayload::Query(QueryReply::CommandReceipt(
                CommandReceipt {
                    request_id: probe.clone(),
                    state: CommandReceiptState::Unknown,
                },
            ))),
        );
    }
    for call in calls {
        let (probe, reply) = call.await.unwrap();
        let Ok(QueryReply::CommandReceipt(receipt)) = reply else {
            panic!("{reply:?}")
        };
        assert_eq!(receipt.request_id, probe, "each caller gets its own reply");
    }
}

#[tokio::test]
async fn request_ids_are_unique_within_a_client() {
    let (client, mut server) = connected().await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        let call = tokio::spawn({
            let client = client.clone();
            async move { client.command(stop_chat("t")).await }
        });
        let request = server.recv_request().await.unwrap();
        ids.push(request.request_id.clone());
        server.reply(
            &request.request_id,
            ok(ReplyPayload::Command(command_reply())),
        );
        call.await.unwrap().unwrap();
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3);
}

#[tokio::test]
async fn a_core_error_reaches_the_caller_unchanged() {
    let (client, mut server) = connected().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.command(stop_chat("t")).await }
    });
    let request = server.recv_request().await.unwrap();
    let error = WorkbenchError::stale_revision(Revision(1), Revision(2));
    server.reply(&request.request_id, ReplyResult::Error(error.clone()));
    assert_eq!(call.await.unwrap(), Err(ClientError::Server(error)));
    // An error reply is an answer; the connection stays usable.
    assert_eq!(client.connection_state(), ConnectionState::Connected);
}

#[tokio::test]
async fn a_reply_with_an_unknown_or_repeated_request_id_closes_the_connection() {
    // Unknown id while a request is outstanding.
    let (client, mut server) = connected().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.query(status_query()).await }
    });
    server.recv_request().await.unwrap();
    server.reply(
        &RequestId::new("never-sent").unwrap(),
        ok(ReplyPayload::Ack),
    );
    let reason = client.closed().await;
    assert!(
        matches!(reason, TransportError::ProtocolViolation(_)),
        "{reason:?}"
    );
    assert_eq!(
        call.await.unwrap(),
        Err(ClientError::Transport(reason.clone()))
    );

    // The same id answered twice.
    let (client, mut server) = connected().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.command(stop_chat("t")).await }
    });
    let request = server.recv_request().await.unwrap();
    let reply = ok(ReplyPayload::Command(command_reply()));
    server.reply(&request.request_id, reply.clone());
    server.reply(&request.request_id, reply);
    call.await.unwrap().expect("the first reply is honoured");
    assert!(matches!(
        client.closed().await,
        TransportError::ProtocolViolation(_)
    ));
}

#[tokio::test]
async fn a_reply_of_the_wrong_type_is_a_protocol_violation() {
    let (client, mut server) = connected().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.command(stop_chat("t")).await }
    });
    let request = server.recv_request().await.unwrap();
    server.reply(
        &request.request_id,
        ok(ReplyPayload::Query(QueryReply::CoreStatus(status()))),
    );
    assert!(matches!(
        call.await.unwrap(),
        Err(ClientError::Transport(TransportError::ProtocolViolation(_)))
    ));
    assert!(matches!(
        client.closed().await,
        TransportError::ProtocolViolation(_)
    ));
}

#[tokio::test]
async fn ping_checks_the_nonce() {
    let (client, mut server) = connected().await;
    barrier(&client, &mut server).await;

    let call = tokio::spawn({
        let client = client.clone();
        async move { client.ping(Nonce(5)).await }
    });
    let request = server.recv_request().await.unwrap();
    server.reply(
        &request.request_id,
        ok(ReplyPayload::Pong { nonce: Nonce(6) }),
    );
    assert!(matches!(
        call.await.unwrap(),
        Err(ClientError::Transport(TransportError::ProtocolViolation(_)))
    ));
}

// --- not sent ---------------------------------------------------------------

#[tokio::test]
async fn requests_that_break_rules_are_refused_before_they_are_sent() {
    let mut hello = server_hello(WireLimits::default());
    hello.capabilities.retain(|c| *c != Capability::Approvals);
    let (client, mut server) = connected_with(hello).await;

    let approval = Command::DecideToolApproval(DecideToolApproval {
        approval_id: ApprovalId::new("ap-1").unwrap(),
        core_epoch: InstanceEpoch(7),
        approval_revision: Revision(1),
        decision: ApprovalDecision::Approve,
    });
    assert_eq!(
        client.command(approval).await,
        Err(ClientError::UnsupportedCapability(Capability::Approvals))
    );
    let blank_answer = Command::AnswerUserQuestion(AnswerUserQuestion {
        thread: thread(),
        question_id: QuestionId::new("q").unwrap(),
        answer: String::new(),
        question_revision: Revision(1),
    });
    assert!(matches!(
        client.command(blank_answer).await,
        Err(ClientError::Invalid(_))
    ));

    // Neither reached the core: the next request it sees is this one.
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.command(stop_chat("after")).await }
    });
    let request = server.recv_request().await.unwrap();
    assert_eq!(request.body, RequestBody::Command(stop_chat("after")));
    server.reply(
        &request.request_id,
        ok(ReplyPayload::Command(command_reply())),
    );
    call.await.unwrap().unwrap();
}

#[tokio::test]
async fn too_many_requests_in_flight_are_refused_without_being_sent() {
    let (client, mut server) = connected_with(server_hello(limits(2))).await;
    let mut first = Box::pin(client.command(stop_chat("one")));
    let mut second = Box::pin(client.command(stop_chat("two")));
    poll_once(&mut first).await;
    poll_once(&mut second).await;

    assert_eq!(
        client.command(stop_chat("three")).await,
        Err(ClientError::Transport(TransportError::QueueFull))
    );

    for expected in ["one", "two"] {
        let request = server.recv_request().await.unwrap();
        assert_eq!(request.body, RequestBody::Command(stop_chat(expected)));
        server.reply(
            &request.request_id,
            ok(ReplyPayload::Command(command_reply())),
        );
    }
    first.await.unwrap();
    second.await.unwrap();

    // Room again; and "three" was never sent.
    let mut fourth = Box::pin(client.command(stop_chat("four")));
    poll_once(&mut fourth).await;
    let request = server.recv_request().await.unwrap();
    assert_eq!(request.body, RequestBody::Command(stop_chat("four")));
    server.reply(
        &request.request_id,
        ok(ReplyPayload::Command(command_reply())),
    );
    fourth.await.unwrap();
}

#[tokio::test]
async fn an_oversized_request_is_refused_locally() {
    let (client, mut server) = connected_with(server_hello(small_limits())).await;
    let chat = Command::SendChat(SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: thread(),
        submission_id: SubmissionId::new("s").unwrap(),
        composer_role: ComposerRole::Worker,
        text: "x".repeat(10_000),
        skills: vec![],
        images: vec![],
        model_preference: None,
        fork_seed: None,
    });
    assert!(matches!(
        client.command(chat).await,
        Err(ClientError::Transport(TransportError::FrameTooLarge {
            max: 8192,
            ..
        }))
    ));
    let mut next = Box::pin(client.command(stop_chat("next")));
    poll_once(&mut next).await;
    let request = server.recv_request().await.unwrap();
    assert_eq!(request.body, RequestBody::Command(stop_chat("next")));
}

// --- dropping futures ---------------------------------------------------------

#[tokio::test]
async fn dropping_a_request_future_does_not_cancel_the_request() {
    let (client, mut server) = connected().await;
    let mut abandoned = Box::pin(client.command(stop_chat("abandoned")));
    poll_once(&mut abandoned).await;
    drop(abandoned);

    // The core still receives it, and a late reply to nobody does no harm.
    let request = server.recv_request().await.unwrap();
    assert_eq!(request.body, RequestBody::Command(stop_chat("abandoned")));
    server.reply(
        &request.request_id,
        ok(ReplyPayload::Command(command_reply())),
    );

    let call = tokio::spawn({
        let client = client.clone();
        async move { client.command(stop_chat("later")).await }
    });
    let request = server.recv_request().await.unwrap();
    server.reply(
        &request.request_id,
        ok(ReplyPayload::Command(command_reply())),
    );
    call.await.unwrap().unwrap();
    assert_eq!(client.connection_state(), ConnectionState::Connected);
}

// --- losing the connection ---------------------------------------------------

#[tokio::test]
async fn a_command_whose_reply_is_lost_reports_outcome_unknown_and_is_not_resent() {
    let (client, mut server) = connected().await;
    let command = tokio::spawn({
        let client = client.clone();
        async move { client.command(stop_chat("t")).await }
    });
    let query = tokio::spawn({
        let client = client.clone();
        async move { client.query(status_query()).await }
    });
    let mut seen = Vec::new();
    for _ in 0..2 {
        seen.push(server.recv_request().await.unwrap());
    }
    server.close();

    let sent_command = seen
        .iter()
        .find(|r| matches!(r.body, RequestBody::Command(_)))
        .unwrap();
    assert_eq!(
        command.await.unwrap(),
        Err(ClientError::Transport(TransportError::OutcomeUnknown {
            request_id: sent_command.request_id.clone()
        }))
    );
    // A query can simply be asked again, so it is a plain disconnect.
    assert_eq!(
        query.await.unwrap(),
        Err(ClientError::Transport(TransportError::Disconnected))
    );
    assert_eq!(client.closed().await, TransportError::Disconnected);
    // No reconnect or resend happens at this layer.
    assert_eq!(
        client.command(stop_chat("t")).await,
        Err(ClientError::Transport(TransportError::Disconnected))
    );
}

#[tokio::test]
async fn a_command_never_handed_to_the_transport_is_a_plain_disconnect() {
    let (client, mut server) = connected().await;
    server.hold_writes();
    // The first command's write starts and stalls: the core may or may not get it.
    let stalled = tokio::spawn({
        let client = client.clone();
        async move { client.command(stop_chat("stalled")).await }
    });
    server.write_blocked().await;
    // The second is queued behind it and certainly never reached the core.
    let mut queued = Box::pin(client.command(stop_chat("queued")));
    poll_once(&mut queued).await;

    server.close();

    let Err(ClientError::Transport(TransportError::OutcomeUnknown { .. })) = stalled.await.unwrap()
    else {
        panic!("a started write has an unknown outcome");
    };
    assert_eq!(
        queued.await,
        Err(ClientError::Transport(TransportError::Disconnected))
    );
}

#[tokio::test]
async fn connection_loss_is_published() {
    let (client, mut server) = connected().await;
    let mut state = client.watch_connection();
    server.close();
    state
        .wait_for(|s| matches!(s, ConnectionState::Closed(_)))
        .await
        .unwrap();
    assert_eq!(
        client.connection_state(),
        ConnectionState::Closed(TransportError::Disconnected)
    );
}

// --- bytes on the wire -----------------------------------------------------------

fn reply_frame(request_id: &RequestId) -> Vec<u8> {
    encode_frame(
        &ServerFrame::Reply(ReplyEnvelope {
            request_id: request_id.clone(),
            result: ok(ReplyPayload::Command(command_reply())),
        }),
        u32::MAX,
    )
    .unwrap()
}

#[tokio::test]
async fn frames_split_into_single_bytes_and_frames_glued_together_both_work() {
    let (client, mut server) = connected().await;
    let call = |name: &'static str| {
        tokio::spawn({
            let client = client.clone();
            async move { client.command(stop_chat(name)).await }
        })
    };

    let one = call("one");
    let request = server.recv_request().await.unwrap();
    for byte in reply_frame(&request.request_id) {
        server.send_raw(vec![byte]);
    }
    one.await.unwrap().unwrap();

    let (a, b) = (call("a"), call("b"));
    let first = server.recv_request().await.unwrap();
    let second = server.recv_request().await.unwrap();
    let mut glued = reply_frame(&second.request_id);
    glued.extend(reply_frame(&first.request_id));
    server.send_raw(glued);
    a.await.unwrap().unwrap();
    b.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_corrupt_frame_closes_the_connection_with_a_violation() {
    let (client, server) = connected().await;
    let mut garbage = 5u32.to_be_bytes().to_vec();
    garbage.extend_from_slice(b"{nope");
    server.send_raw(garbage);
    assert!(matches!(
        client.closed().await,
        TransportError::ProtocolViolation(_)
    ));
}

#[tokio::test]
async fn a_frame_cut_off_by_disconnect_is_a_violation() {
    let (client, mut server) = connected().await;
    let mut cut = reply_frame(&RequestId::new("x").unwrap());
    cut.truncate(cut.len() / 2);
    server.send_raw(cut);
    server.close();
    assert!(matches!(
        client.closed().await,
        TransportError::ProtocolViolation(_)
    ));
}

#[tokio::test]
async fn an_oversized_frame_from_the_core_is_refused_by_its_length_alone() {
    let (client, server) = connected_with(server_hello(small_limits())).await;
    // Only the header: the client must not wait for, or buffer, 9000 bytes.
    server.send_raw(9000u32.to_be_bytes().to_vec());
    assert_eq!(
        client.closed().await,
        TransportError::FrameTooLarge {
            len: 9000,
            max: 8192
        }
    );
}
