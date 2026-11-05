//! Negotiation, rejection of bad requests, provenance and size limits.

mod common;

use common::*;
use serde_json::json;
use workbench_protocol::commands::*;
use workbench_protocol::envelope::*;
use workbench_protocol::errors::*;
use workbench_protocol::events::*;
use workbench_protocol::framing::encode_frame;
use workbench_protocol::handshake::*;
use workbench_protocol::ids::*;
use workbench_protocol::limits::*;
use workbench_protocol::queries::*;
use workbench_protocol::validation::ValidationError;
use workbench_protocol::views::*;
use workbench_protocol::wire::*;

fn offer<'a>(
    core: &'a CoreIdentity,
    versions: &'a [ProtocolVersion],
    capabilities: &'a [Capability],
) -> ServerOffer<'a> {
    ServerOffer {
        core,
        versions,
        capabilities,
    }
}

fn core() -> CoreIdentity {
    CoreIdentity::new("core-abc").unwrap()
}

// --- handshake ---------------------------------------------------------

#[test]
fn negotiation_picks_the_highest_common_version_and_requested_capabilities() {
    let core = core();
    let versions = [ProtocolVersion(1), ProtocolVersion(2)];
    let mut hello = client_hello();
    hello.supported_versions = vec![ProtocolVersion(1), ProtocolVersion(2), ProtocolVersion(3)];
    hello.optional_capabilities = vec![
        Capability::SnapshotEvents.into(),
        CapabilityName::new("from_the_future.v9").unwrap(),
        Capability::SafeRestart.into(), // asked for but not offered: dropped, not fatal
    ];
    let offered = [
        Capability::Queries,
        Capability::Commands,
        Capability::SnapshotEvents,
    ];
    let negotiated = negotiate(&hello, &offer(&core, &versions, &offered)).unwrap();
    assert_eq!(negotiated.version, ProtocolVersion(2));
    assert_eq!(
        negotiated.capabilities,
        [
            Capability::Commands,
            Capability::Queries,
            Capability::SnapshotEvents
        ]
    );
}

#[test]
fn version_mismatch_is_rejected_with_a_direction() {
    let core = core();
    let hello = client_hello();
    let newer_core = negotiate(
        &hello,
        &offer(&core, &[ProtocolVersion(2)], &Capability::ALL),
    )
    .unwrap_err();
    assert_eq!(newer_core.reason, RejectReason::UnsupportedVersion);
    assert_eq!(
        newer_core.recommended_action,
        RecommendedAction::UpgradeClient
    );
    assert_eq!(newer_core.supported_versions, [ProtocolVersion(2)]);

    let mut newer_client = client_hello();
    newer_client.supported_versions = vec![ProtocolVersion(5)];
    let rejected = negotiate(
        &newer_client,
        &offer(&core, &[ProtocolVersion(1)], &Capability::ALL),
    )
    .unwrap_err();
    assert_eq!(rejected.recommended_action, RecommendedAction::UpgradeCore);
}

#[test]
fn missing_and_unknown_required_capabilities_are_rejected_by_name() {
    let core = core();
    let mut hello = client_hello();
    hello.required_capabilities = vec![
        Capability::Commands.into(),
        Capability::Approvals.into(),
        CapabilityName::new("teleport.v1").unwrap(),
    ];
    let rejected = negotiate(
        &hello,
        &offer(&core, &[ProtocolVersion::CURRENT], &[Capability::Commands]),
    )
    .unwrap_err();
    assert_eq!(rejected.reason, RejectReason::MissingCapabilities);
    let names: Vec<&str> = rejected
        .missing_capabilities
        .iter()
        .map(CapabilityName::as_str)
        .collect();
    assert_eq!(names, ["approvals.v1", "teleport.v1"]);
}

#[test]
fn a_different_core_and_a_malformed_hello_are_rejected() {
    let other = CoreIdentity::new("core-other").unwrap();
    let rejected = negotiate(
        &client_hello(),
        &offer(&other, &[ProtocolVersion::CURRENT], &Capability::ALL),
    )
    .unwrap_err();
    assert_eq!(rejected.reason, RejectReason::CoreMismatch);

    let mut hello = client_hello();
    hello.supported_versions.clear();
    let rejected = negotiate(
        &hello,
        &offer(&core(), &[ProtocolVersion::CURRENT], &Capability::ALL),
    )
    .unwrap_err();
    assert_eq!(rejected.reason, RejectReason::InvalidHello);
}

#[test]
fn unknown_capability_in_a_server_hello_is_not_silently_accepted() {
    let mut wire = serde_json::to_value(server_hello()).unwrap();
    wire["capabilities"] = json!(["commands.v1", "teleport.v1"]);
    assert!(serde_json::from_value::<ServerHello>(wire).is_err());
}

#[test]
fn wire_limits_defaults_match_the_spec_and_bad_values_are_refused() {
    let limits = WireLimits::default();
    assert_eq!(limits.max_frame_bytes, 1024 * 1024);
    assert_eq!(limits.max_body_chunk_bytes, 64 * 1024);
    assert_eq!(limits.max_in_flight_requests, 64);
    limits.validate().unwrap();

    let broken: [fn(&mut WireLimits); 6] = [
        |l| l.max_frame_bytes = 0,
        |l| l.max_frame_bytes = HARD_MAX_FRAME_BYTES + 1,
        |l| l.max_body_chunk_bytes = l.max_frame_bytes,
        |l| l.max_in_flight_requests = 0,
        |l| l.subscription_queue_events = 0,
        |l| l.subscription_queue_bytes = 1,
    ];
    for mutate in broken {
        let mut l = WireLimits::default();
        mutate(&mut l);
        assert!(l.validate().is_err(), "{l:?}");
    }
}

// --- request decoding ---------------------------------------------------

fn decode(value: serde_json::Value) -> Result<RequestEnvelope, RequestRejection> {
    decode_request(&serde_json::to_vec(&value).unwrap(), &WireLimits::default())
}

fn request_json(kind: &str, payload: serde_json::Value) -> serde_json::Value {
    json!({"request_id": "req-9", "session_id": "sess-1", "kind": kind, "payload": payload})
}

#[test]
fn unknown_command_query_and_control_types_are_refused_as_unsupported() {
    for (kind, ty) in [
        ("command", "launch_missiles"),
        ("query", "get_secrets"),
        ("control", "reboot"),
    ] {
        let rejection = decode(request_json(kind, json!({"type": ty, "payload": {}}))).unwrap_err();
        assert_eq!(
            rejection.error.code,
            ErrorCode::UnsupportedCapability,
            "{kind}/{ty}"
        );
        assert_eq!(rejection.request_id, Some(request(9)));
        assert_eq!(rejection.error.correlation_id, Some(request(9)));
        assert_eq!(rejection.error.retry, RetryAdvice::Never);
    }
}

#[test]
fn malformed_requests_are_invalid_not_unsupported() {
    let cases = [
        request_json("command", json!({"type": "stop_chat", "payload": {}})), // missing thread
        request_json(
            "command",
            json!({"type": "stop_chat", "payload": {"thread": "t", "x": 1}}),
        ),
        request_json(
            "teleport",
            json!({"type": "stop_chat", "payload": {"thread": "t"}}),
        ),
        json!({"session_id": "s", "kind": "query", "payload": {"type": "get_core_status"}}),
    ];
    for case in cases {
        let rejection = decode(case.clone()).unwrap_err();
        assert_eq!(rejection.error.code, ErrorCode::InvalidRequest, "{case}");
    }
    let rejection = decode_request(b"\xff\xfenot json", &WireLimits::default()).unwrap_err();
    assert_eq!(rejection.error.code, ErrorCode::InvalidRequest);
    assert_eq!(rejection.request_id, None);
}

#[test]
fn clients_cannot_supply_provenance_or_automatic_continuation() {
    let mut send = serde_json::to_value(Command::SendChat(send_chat())).unwrap();
    send["payload"]["provenance"] =
        json!({"actor": "agent", "surface": "none", "agent_run_id": "run-1"});
    let rejection = decode(request_json("command", send)).unwrap_err();
    assert_eq!(rejection.error.code, ErrorCode::InvalidRequest);

    let rejection = decode(request_json(
        "command",
        json!({"type": "continue_chat", "payload": {"project": "p", "thread": "t", "automatic": true}}),
    ))
    .unwrap_err();
    assert_eq!(rejection.error.code, ErrorCode::InvalidRequest);
}

#[test]
fn provenance_must_describe_a_possible_actor() {
    human().validate().unwrap();
    agent().validate().unwrap();
    let bad = [
        Provenance {
            agent_run_id: None,
            ..agent()
        },
        Provenance {
            surface: Surface::Cli,
            ..agent()
        },
        Provenance {
            agent_run_id: Some(run()),
            ..human()
        },
        Provenance {
            surface: Surface::None,
            ..human()
        },
    ];
    for provenance in bad {
        assert!(provenance.validate().is_err(), "{provenance:?}");
    }
}

#[test]
fn invalid_field_values_map_to_structured_errors() {
    let mut chat = send_chat();
    chat.text = "x".repeat(MAX_TEXT_BYTES + 1);
    let rejection = decode(request_json(
        "command",
        serde_json::to_value(Command::SendChat(chat)).unwrap(),
    ))
    .unwrap_err();
    assert_eq!(rejection.error.code, ErrorCode::LimitExceeded);
    assert_eq!(
        rejection.error.details,
        Some(ErrorDetails::Field {
            field: "text".into()
        })
    );

    let mut chat = send_chat();
    chat.text = "bell\u{7}".into();
    assert_eq!(
        Command::SendChat(chat).validate(),
        Err(ValidationError::InvalidChars { field: "text" })
    );
}

#[test]
fn command_collections_and_strings_are_bounded() {
    let too_many_skills = SendChat {
        skills: vec!["s".into(); MAX_SKILLS + 1],
        ..send_chat()
    };
    assert!(matches!(
        too_many_skills.validate(),
        Err(ValidationError::TooMany {
            field: "skills",
            ..
        })
    ));

    let too_many_images = SendChat {
        images: vec![send_chat().images.remove(0); MAX_IMAGES + 1],
        ..send_chat()
    };
    assert!(too_many_images.validate().is_err());

    let not_an_image = SendChat {
        images: vec![ImageRef {
            attachment_id: AttachmentId::new("a").unwrap(),
            mime_type: "text/html".into(),
        }],
        ..send_chat()
    };
    assert!(not_an_image.validate().is_err());

    let empty = SendChat {
        text: String::new(),
        images: vec![],
        ..send_chat()
    };
    assert!(matches!(
        empty.validate(),
        Err(ValidationError::Empty { .. })
    ));
    let images_only = SendChat {
        text: String::new(),
        ..send_chat()
    };
    images_only.validate().unwrap();

    let goal = |objective: String, constraints: Vec<String>| SubmitGoal {
        project: project(),
        thread: thread(),
        objective,
        references: vec![],
        constraints,
        delegation: None,
    };
    assert!(goal(String::new(), vec![]).validate().is_err());
    assert!(
        goal("x".repeat(MAX_OBJECTIVE_BYTES + 1), vec![])
            .validate()
            .is_err()
    );
    assert!(
        goal("o".into(), vec!["c".into(); MAX_CONSTRAINTS + 1])
            .validate()
            .is_err()
    );
    goal("o".into(), vec!["c".into(); MAX_CONSTRAINTS])
        .validate()
        .unwrap();

    let blank = Command::AnswerUserQuestion(AnswerUserQuestion {
        thread: thread(),
        question_id: QuestionId::new("q").unwrap(),
        answer: String::new(),
        question_revision: Revision(1),
    });
    assert!(blank.validate().is_err());
}

#[test]
fn ids_reject_empty_oversized_and_control_characters_at_parse_time() {
    assert!(ThreadId::new("").is_err());
    assert!(ThreadId::new("a".repeat(MAX_ID_BYTES + 1)).is_err());
    assert!(ThreadId::new("a\nb").is_err());
    assert!(ThreadId::new("a".repeat(MAX_ID_BYTES)).is_ok());
    assert!(serde_json::from_value::<ThreadId>(json!("")).is_err());
    assert!(serde_json::from_value::<ThreadId>(json!(7)).is_err());
}

#[test]
fn the_largest_valid_command_fits_the_default_frame() {
    // Quotes escape to two bytes, the worst case the text rules allow.
    let worst = "\"".repeat(MAX_TEXT_BYTES);
    let chat = SendChat {
        text: worst.clone(),
        skills: vec!["\"".repeat(MAX_NAME_BYTES); MAX_SKILLS],
        images: vec![
            ImageRef {
                attachment_id: AttachmentId::new("a".repeat(MAX_ID_BYTES)).unwrap(),
                mime_type: format!("image/{}", "\"".repeat(MAX_MIME_BYTES - 6)),
            };
            MAX_IMAGES
        ],
        ..send_chat()
    };
    chat.validate().unwrap();
    let goal = SubmitGoal {
        project: project(),
        thread: thread(),
        objective: "\"".repeat(MAX_OBJECTIVE_BYTES),
        references: vec![
            PacketReference {
                kind: ReferenceKind::Packet,
                value: "\"".repeat(MAX_REFERENCE_BYTES)
            };
            MAX_REFERENCES
        ],
        constraints: vec!["\"".repeat(MAX_CONSTRAINT_BYTES); MAX_CONSTRAINTS],
        delegation: Some(Delegation::Reference {
            value: "\"".repeat(MAX_REFERENCE_BYTES),
        }),
    };
    goal.validate().unwrap();
    let answer = Command::AnswerUserQuestion(AnswerUserQuestion {
        thread: thread(),
        question_id: QuestionId::new("q").unwrap(),
        answer: worst,
        question_revision: Revision(1),
    });
    answer.validate().unwrap();

    for command in [Command::SendChat(chat), Command::SubmitGoal(goal), answer] {
        let frame = encode_frame(
            &envelope(1, RequestBody::Command(command)),
            DEFAULT_FRAME_BYTES,
        )
        .expect("fits the default frame");
        assert!(frame.len() < DEFAULT_FRAME_BYTES as usize);
    }
}

// --- queries ------------------------------------------------------------

#[test]
fn page_and_body_requests_are_bounded() {
    let limits = WireLimits::default();
    let page = |limit| {
        Query::ListProjects(PageRequest {
            cursor: None,
            limit,
        })
    };
    page(1).validate(&limits).unwrap();
    page(MAX_PAGE_SIZE).validate(&limits).unwrap();
    assert!(page(0).validate(&limits).is_err());
    assert!(page(MAX_PAGE_SIZE + 1).validate(&limits).is_err());

    let read = |max_bytes| {
        Query::ReadBody(ReadBody {
            body_id: BodyId::new("b").unwrap(),
            offset: ByteOffset(0),
            max_bytes,
        })
    };
    read(limits.max_body_chunk_bytes).validate(&limits).unwrap();
    read(MIN_BODY_CHUNK_BYTES).validate(&limits).unwrap();
    assert!(
        read(limits.max_body_chunk_bytes + 1)
            .validate(&limits)
            .is_err()
    );
    assert!(read(0).validate(&limits).is_err());
    // A smaller negotiated chunk limit applies.
    let small = WireLimits {
        max_body_chunk_bytes: 1024,
        ..limits
    };
    assert!(read(2048).validate(&small).is_err());
}

#[test]
fn page_request_defaults_apply_when_fields_are_omitted() {
    let query: Query =
        serde_json::from_value(json!({"type": "list_threads", "payload": {"project": "p"}}))
            .unwrap();
    let Query::ListThreads(list) = query else {
        panic!()
    };
    assert_eq!(list.page.limit, DEFAULT_PAGE_SIZE);
    assert_eq!(list.page.cursor, None);
}

// --- capability gating ---------------------------------------------------

#[test]
fn requests_name_the_capability_they_need() {
    let capability = |body: RequestBody| body.required_capability();
    assert_eq!(
        capability(RequestBody::Command(commands().remove(10))),
        Some(Capability::Questions)
    );
    assert_eq!(
        capability(RequestBody::Command(commands().remove(11))),
        Some(Capability::Approvals)
    );
    assert_eq!(
        capability(RequestBody::Query(queries().remove(10))),
        Some(Capability::CommittedBodies)
    );
    assert_eq!(
        capability(RequestBody::Control(controls().remove(0))),
        Some(Capability::SnapshotEvents)
    );
    assert_eq!(capability(RequestBody::Control(controls().remove(2))), None);
}

#[test]
fn every_sample_request_validates() {
    let limits = WireLimits::default();
    for command in commands() {
        command.validate().unwrap();
        let bytes = serde_json::to_vec(&envelope(1, RequestBody::Command(command))).unwrap();
        decode_request(&bytes, &limits).unwrap();
    }
    for query in queries() {
        let bytes = serde_json::to_vec(&envelope(1, RequestBody::Query(query))).unwrap();
        decode_request(&bytes, &limits).unwrap();
    }
    for control in controls() {
        let bytes = serde_json::to_vec(&envelope(1, RequestBody::Control(control))).unwrap();
        decode_request(&bytes, &limits).unwrap();
    }
}

// --- received frames ---------------------------------------------------------

fn reply_frame(payload: ReplyPayload) -> ServerFrame {
    ServerFrame::Reply(ReplyEnvelope {
        request_id: request(1),
        result: ReplyResult::Success(payload),
    })
}

#[test]
fn every_sample_server_frame_passes_validation() {
    let limits = WireLimits::default();
    for payload in reply_payloads() {
        reply_frame(payload).validate(&limits).unwrap();
    }
    for reply in query_replies() {
        reply_frame(ReplyPayload::Query(reply))
            .validate(&limits)
            .unwrap();
    }
    for event in ui_events() {
        ServerFrame::Event(Box::new(event_envelope(event)))
            .validate(&limits)
            .unwrap();
    }
    ServerFrame::Reply(ReplyEnvelope {
        request_id: request(1),
        result: ReplyResult::Error(error()),
    })
    .validate(&limits)
    .unwrap();
}

fn messages(n: usize, text: String) -> QueryReply {
    QueryReply::CommittedHistory(Page {
        items: vec![message(MessageContent::Inline { text }); n],
        next_cursor: None,
        snapshot_revision: Revision(1),
    })
}

#[test]
fn received_pages_chunks_and_text_over_their_bounds_fail_validation() {
    let limits = WireLimits::default();
    let invalid = |payload: ReplyPayload| {
        assert!(reply_frame(payload).validate(&limits).is_err());
    };
    let valid = |payload: ReplyPayload| reply_frame(payload).validate(&limits).unwrap();

    // Page size: 200 items pass, 201 do not.
    valid(ReplyPayload::Query(messages(
        MAX_PAGE_SIZE as usize,
        "x".into(),
    )));
    invalid(ReplyPayload::Query(messages(
        MAX_PAGE_SIZE as usize + 1,
        "x".into(),
    )));
    // Inline message: the limit passes, one byte more does not.
    valid(ReplyPayload::Query(messages(
        1,
        "x".repeat(MAX_INLINE_MESSAGE_BYTES),
    )));
    invalid(ReplyPayload::Query(messages(
        1,
        "x".repeat(MAX_INLINE_MESSAGE_BYTES + 1),
    )));
    // Body chunk against the negotiated chunk size.
    let chunk = |len: usize| {
        ReplyPayload::Query(QueryReply::Body(BodyChunk {
            body_id: BodyId::new("b").unwrap(),
            offset: ByteOffset(0),
            text: "x".repeat(len),
            total_bytes: ByteOffset(len as u64),
            next_offset: None,
        }))
    };
    valid(chunk(limits.max_body_chunk_bytes as usize));
    invalid(chunk(limits.max_body_chunk_bytes as usize + 1));
    // A subscribe answer's snapshot page is bounded like any other page.
    let started = |items: usize| {
        let mut started = SubscriptionStarted {
            subscription_id: sub(),
            snapshot_id: None,
            high_water: cursor(7, 1),
            initial_page: None,
        };
        started.initial_page = Some(Page {
            items: vec![SnapshotItem::Thread(thread_summary()); items],
            next_cursor: None,
            snapshot_revision: Revision(1),
        });
        ReplyPayload::SubscriptionStarted(started)
    };
    valid(started(MAX_PAGE_SIZE as usize));
    invalid(started(MAX_PAGE_SIZE as usize + 1));
    // Text fields of views and errors.
    let mut long_title = thread_summary();
    long_title.title = "t".repeat(MAX_TITLE_BYTES + 1);
    invalid(ReplyPayload::Query(QueryReply::Thread(ThreadView {
        summary: long_title.clone(),
        root_run: None,
        active_run: None,
        pending_questions: 0,
        pending_approvals: 0,
        queued_follow_ups: 0,
    })));
    let event = ServerFrame::Event(Box::new(event_envelope(UiEvent::ThreadChanged {
        thread: long_title,
    })));
    assert!(event.validate(&limits).is_err());
    let mut huge_error = error();
    huge_error.message = "e".repeat(9 * 1024);
    assert!(
        ServerFrame::Reply(ReplyEnvelope {
            request_id: request(1),
            result: ReplyResult::Error(huge_error),
        })
        .validate(&limits)
        .is_err()
    );
    let mut busy = status();
    busy.retention_reasons = vec![RetentionReason::ActiveTurn; 17];
    invalid(ReplyPayload::Query(QueryReply::CoreStatus(busy)));
    // Incoherent provenance in a relayed view.
    let mut bad_receipt = receipt();
    bad_receipt.sent_by = Provenance {
        agent_run_id: None,
        ..agent()
    };
    invalid(ReplyPayload::Query(QueryReply::InputReceipts(Page {
        items: vec![bad_receipt],
        next_cursor: None,
        snapshot_revision: Revision(1),
    })));
}

#[test]
fn agent_authored_question_text_may_contain_control_characters() {
    let mut q = question(QuestionTarget::User);
    q.title = "line one\nline two\u{1b}[0m".into();
    q.options = vec!["a\tb".into()];
    q.validate().unwrap();
}

// --- closed request shapes -------------------------------------------------------

#[test]
fn unknown_fields_are_refused_at_every_level_of_a_request() {
    let cases = [
        // Envelope level: a client-supplied provenance must not be ignored.
        json!({"request_id": "req-9", "session_id": "s", "kind": "query",
               "payload": {"type": "get_core_status"}, "provenance": {"actor": "agent"}}),
        // Struct-like enum variants.
        request_json(
            "control",
            json!({"type": "ping", "payload": {"nonce": "1", "extra": 1}}),
        ),
        request_json(
            "control",
            json!({"type": "unsubscribe", "payload": {"subscription_id": "s", "extra": 1}}),
        ),
        request_json(
            "query",
            json!({"type": "get_command_receipt", "payload": {"request_id": "r", "extra": 1}}),
        ),
        request_json(
            "command",
            json!({"type": "submit_goal", "payload": {
            "project": "p", "thread": "t", "objective": "o",
            "delegation": {"type": "reference", "payload": {"value": "v", "extra": 1}}}}),
        ),
    ];
    for case in cases {
        let rejection = decode(case.clone()).unwrap_err();
        assert_eq!(rejection.error.code, ErrorCode::InvalidRequest, "{case}");
    }
}

// --- answers and handshake replies --------------------------------------------------

fn answered(text: String) -> QuestionView {
    let mut q = question(QuestionTarget::User);
    q.answer = Some(QuestionAnswer {
        text,
        answered_by: human(),
    });
    q
}

#[test]
fn a_question_answer_is_bounded_on_every_received_path() {
    let limits = WireLimits::default();
    let frames = |q: QuestionView| {
        vec![
            reply_frame(ReplyPayload::Query(QueryReply::Questions(one_page(
                q.clone(),
            )))),
            reply_frame(ReplyPayload::SubscriptionStarted(SubscriptionStarted {
                subscription_id: sub(),
                snapshot_id: None,
                high_water: cursor(7, 1),
                initial_page: Some(one_page(SnapshotItem::Question(q.clone()))),
            })),
            ServerFrame::Event(Box::new(event_envelope(UiEvent::QuestionChanged {
                question: q,
            }))),
        ]
    };
    for frame in frames(answered("x".repeat(MAX_TEXT_BYTES))) {
        frame.validate(&limits).unwrap();
    }
    for frame in frames(answered("x".repeat(MAX_TEXT_BYTES + 1))) {
        assert!(frame.validate(&limits).is_err());
    }
    let mut incoherent = answered("ok".into());
    incoherent.answer.as_mut().unwrap().answered_by = Provenance {
        agent_run_id: None,
        ..agent()
    };
    assert!(incoherent.validate().is_err());
}

#[test]
fn a_server_hello_is_checked_field_by_field() {
    server_hello().validate().unwrap();
    type Mutate = fn(&mut ServerHello);
    let broken: Vec<(&str, Mutate)> = vec![
        ("build name", |h| {
            h.server_build.name = "n".repeat(MAX_NAME_BYTES + 1)
        }),
        ("build version", |h| {
            h.server_build.version = "v".repeat(MAX_NAME_BYTES + 1)
        }),
        ("build revision", |h| {
            h.server_build.revision = Some("r".repeat(MAX_NAME_BYTES + 1))
        }),
        ("build control char", |h| {
            h.server_build.name = "a\nb".into()
        }),
        ("status reasons", |h| {
            h.status.retention_reasons = vec![RetentionReason::ActiveTurn; 17]
        }),
        ("limits", |h| h.limits.max_frame_bytes = 0),
        ("capabilities", |h| {
            h.capabilities = vec![Capability::Commands; Capability::ALL.len() + 1]
        }),
    ];
    for (name, mutate) in broken {
        let mut hello = server_hello();
        mutate(&mut hello);
        assert!(hello.validate().is_err(), "{name}");
    }
    let mut at_limit = server_hello();
    at_limit.server_build.name = "n".repeat(MAX_NAME_BYTES);
    at_limit.status.retention_reasons = vec![RetentionReason::ActiveTurn; 16];
    at_limit.validate().unwrap();
}

#[test]
fn a_handshake_rejection_is_bounded() {
    rejected().validate().unwrap();
    let mut long = rejected();
    long.message = "m".repeat(8 * 1024 + 1);
    assert!(long.validate().is_err());
    let mut many = rejected();
    many.missing_capabilities = vec![CapabilityName::new("x").unwrap(); 33];
    assert!(many.validate().is_err());
    let mut versions = rejected();
    versions.supported_versions = vec![ProtocolVersion(1); 17];
    assert!(versions.validate().is_err());
}

fn one_page<T>(item: T) -> Page<T> {
    Page {
        items: vec![item],
        next_cursor: None,
        snapshot_revision: Revision(1),
    }
}
