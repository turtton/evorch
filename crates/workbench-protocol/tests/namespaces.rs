//! The typed namespaces (PR 08s): one table of operations, the refusals for
//! everything outside it, and the reserved namespaces nothing can say yet.

mod common;

use common::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use workbench_protocol::commands::{CommandReply, CommandResult};
use workbench_protocol::envelope::{RequestRejection, decode_request};
use workbench_protocol::events::UiEvent;
use workbench_protocol::handshake::{
    Capability, RecommendedAction, RejectReason, ServerOffer, WireLimits, negotiate,
};
use workbench_protocol::namespaces::{
    ConversationResult, NAMESPACE_NAMES, NAMESPACES, NamespaceStatus, namespace,
};
use workbench_protocol::{ErrorCode, ProtocolVersion};

const RESERVED: [&str; 12] = [
    "settings",
    "auth",
    "mcp",
    "teams",
    "diagnostics",
    "memory",
    "evaluation",
    "improvement",
    "pull_requests",
    "browser",
    "artifacts",
    "maintenance",
];

fn decode(value: Value) -> Result<(), RequestRejection> {
    decode_request(&serde_json::to_vec(&value).unwrap(), &WireLimits::default()).map(|_| ())
}

fn request(kind: &str, payload: Value) -> Value {
    json!({"request_id": "req-1", "session_id": "sess-1", "kind": kind, "payload": payload})
}

fn code_of(value: Value) -> ErrorCode {
    decode(value).unwrap_err().error.code
}

/// `<namespace>/<operation>` of a namespaced wire value.
fn qualified(value: &Value) -> String {
    format!(
        "{}/{}",
        value["type"].as_str().unwrap(),
        value["payload"]["type"].as_str().unwrap()
    )
}

fn samples<T: serde::Serialize>(values: Vec<T>) -> Vec<(String, Value)> {
    values
        .into_iter()
        .map(|v| {
            let wire = serde_json::to_value(v).unwrap();
            (qualified(&wire), wire)
        })
        .collect()
}

#[test]
fn the_operation_table_and_the_enums_are_one_list() {
    let declared =
        |pick: fn(&workbench_protocol::namespaces::NamespaceInfo) -> &'static [&'static str]| {
            NAMESPACES
                .iter()
                .flat_map(|n| pick(n).iter().map(move |op| format!("{}/{op}", n.name)))
                .collect::<BTreeSet<_>>()
        };
    let sampled = |values: Vec<(String, Value)>| {
        let names: Vec<String> = values.into_iter().map(|(n, _)| n).collect();
        let unique: BTreeSet<String> = names.iter().cloned().collect();
        assert_eq!(names.len(), unique.len(), "an operation is sampled twice");
        unique
    };
    assert_eq!(sampled(samples(commands())), declared(|n| n.commands));
    assert_eq!(sampled(samples(queries())), declared(|n| n.queries));
    assert_eq!(sampled(samples(ui_events())), declared(|n| n.events));

    // An operation name belongs to exactly one namespace per request kind.
    for pick in [
        (|n: &workbench_protocol::namespaces::NamespaceInfo| n.commands)
            as fn(&workbench_protocol::namespaces::NamespaceInfo) -> &'static [&'static str],
        |n| n.queries,
        |n| n.events,
    ] {
        let all: Vec<&str> = NAMESPACES
            .iter()
            .flat_map(|n| pick(n).iter().copied())
            .collect();
        let unique: BTreeSet<&str> = all.iter().copied().collect();
        assert_eq!(all.len(), unique.len());
    }

    let names: Vec<&str> = NAMESPACES.iter().map(|n| n.name).collect();
    assert_eq!(names, NAMESPACE_NAMES);
    for info in NAMESPACES {
        let empty = info.commands.is_empty() && info.queries.is_empty() && info.events.is_empty();
        assert_eq!(
            info.status == NamespaceStatus::Reserved,
            empty,
            "{}: only a reserved namespace is empty",
            info.name
        );
    }
}

#[test]
fn every_operation_round_trips_and_decodes_as_a_request() {
    for (name, wire) in samples(commands()) {
        let value = request("command", wire.clone());
        decode(value).unwrap_or_else(|e| panic!("command {name}: {:?}", e.error));
    }
    for (name, wire) in samples(queries()) {
        let value = request("query", wire.clone());
        decode(value).unwrap_or_else(|e| panic!("query {name}: {:?}", e.error));
    }
    for (name, wire) in samples(ui_events()) {
        let event: UiEvent = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&event).unwrap(), wire, "{name}");
    }
}

#[test]
fn each_operation_asks_for_the_capability_it_always_asked_for() {
    use Capability::*;
    let commands_expected = [
        ("conversation/resolve_project", Conversation),
        ("conversation/create_execution", Conversation),
        ("conversation/create_goal", Conversation),
        ("conversation/prepare_existing_thread", Conversation),
        ("conversation/upload_image", Conversation),
        ("conversation/discard_attachment", Conversation),
        ("conversation/send_chat", InputReceipts),
        ("conversation/withdraw_follow_up", InputReceipts),
        ("conversation/continue_chat", Commands),
        ("conversation/compact_chat", Commands),
        ("conversation/deliver_follow_ups_next_turn", Commands),
        ("execution/stop_chat", Commands),
        ("execution/stop_run", Commands),
        ("execution/submit_goal", Commands),
        ("execution/set_goal_review", Commands),
        ("execution/set_goal_checks_paused", Commands),
        ("execution/set_agent_web_tools_enabled", Commands),
        ("questions/answer_user_question", Questions),
        ("approvals/decide_tool_approval", Approvals),
        ("lanes/manage_lane", Commands),
    ];
    let queries_expected = [
        ("conversation/get_input_receipts", InputReceipts),
        ("conversation/read_committed_history", CommittedBodies),
        ("workspace/list_projects", Queries),
        ("workspace/list_threads", Queries),
        ("workspace/get_thread", Queries),
        ("execution/list_runs", Queries),
        ("questions/list_questions", Questions),
        ("approvals/list_approvals", Approvals),
        ("approvals/get_approval_input", Approvals),
        ("lanes/get_lane_management", Queries),
        ("lifecycle/get_core_status", Queries),
        ("projection/get_command_receipt", Commands),
        ("projection/read_body", CommittedBodies),
        ("projection/read_snapshot", Queries),
    ];
    let actual_commands: Vec<(String, Capability)> = commands()
        .into_iter()
        .map(|c| {
            (
                qualified(&serde_json::to_value(&c).unwrap()),
                c.required_capability(),
            )
        })
        .collect();
    let actual_queries: Vec<(String, Capability)> = queries()
        .into_iter()
        .map(|q| {
            (
                qualified(&serde_json::to_value(&q).unwrap()),
                q.required_capability(),
            )
        })
        .collect();
    for (table, actual) in [
        (&commands_expected[..], actual_commands),
        (&queries_expected[..], actual_queries),
    ] {
        assert_eq!(table.len(), actual.len());
        for (name, capability) in table {
            let found = actual
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("{name}"));
            assert_eq!(found.1, *capability, "{name}");
        }
    }
}

#[test]
fn the_core_offers_an_explicit_implemented_set_without_safe_restart() {
    assert!(!Capability::IMPLEMENTED.contains(&Capability::SafeRestart));
    for capability in Capability::IMPLEMENTED {
        assert!(Capability::ALL.contains(&capability));
    }
    assert_eq!(Capability::IMPLEMENTED.len(), Capability::ALL.len() - 1);
    // No reserved namespace has an operation, so none has a capability to advertise.
    for info in NAMESPACES {
        if info.status == NamespaceStatus::Reserved {
            assert!(RESERVED.contains(&info.name));
        }
    }
    for command in commands() {
        assert!(Capability::IMPLEMENTED.contains(&command.required_capability()));
    }
    for query in queries() {
        assert!(Capability::IMPLEMENTED.contains(&query.required_capability()));
    }
}

#[test]
fn reserved_namespaces_are_refused_whatever_is_sent_and_never_echoed() {
    let secret = format!("sk-proj-{}", "R".repeat(48));
    for name in RESERVED {
        assert_eq!(namespace(name).unwrap().status, NamespaceStatus::Reserved);
        for kind in ["command", "query"] {
            for payload in [
                json!({"type": name, "payload": {"type": "anything", "payload": {}}}),
                json!({"type": name, "payload": {"type": secret, "payload": {"token": secret}}}),
                json!({"type": name}),
                json!({"type": name, "payload": null}),
            ] {
                let rejection = decode(request(kind, payload.clone())).unwrap_err();
                assert_eq!(
                    rejection.error.code,
                    ErrorCode::UnsupportedCapability,
                    "{kind}/{name}: {payload}"
                );
                assert!(!rejection.error.message.contains("sk-proj-"));
                assert_eq!(rejection.request_id.unwrap().as_str(), "req-1");
            }
        }
        // Nothing exists in it to deserialize into either.
        let wire = json!({"type": name, "payload": {"type": "anything"}});
        assert!(
            serde_json::from_value::<workbench_protocol::commands::Command>(wire.clone()).is_err()
        );
        assert!(
            serde_json::from_value::<workbench_protocol::queries::Query>(wire.clone()).is_err()
        );
        assert!(serde_json::from_value::<UiEvent>(wire).is_err());
    }
}

#[test]
fn unknown_namespaces_and_operations_are_unsupported_and_defects_are_invalid() {
    let secret = format!("sk-proj-{}", "U".repeat(48));
    let unsupported = [
        request(
            "command",
            json!({"type": secret, "payload": {"type": "stop_chat", "payload": {}}}),
        ),
        request(
            "command",
            json!({"type": "execution", "payload": {"type": secret, "payload": {}}}),
        ),
        // A real operation in the wrong namespace is not that namespace's.
        request(
            "command",
            json!({"type": "conversation", "payload": {"type": "stop_chat", "payload": {"thread": "t"}}}),
        ),
        // A query name sent as a command, and the other way round.
        request(
            "command",
            json!({"type": "lifecycle", "payload": {"type": "get_core_status"}}),
        ),
        request(
            "query",
            json!({"type": "execution", "payload": {"type": "stop_chat", "payload": {"thread": "t"}}}),
        ),
        // The flat shape of protocol version 1 has no alias.
        request(
            "command",
            json!({"type": "send_chat", "payload": {"project": "p", "thread": "t"}}),
        ),
        request("query", json!({"type": "get_core_status"})),
        request(
            "query",
            json!({"type": "list_threads", "payload": {"project": "p"}}),
        ),
    ];
    for value in unsupported {
        let rejection = decode(value.clone()).unwrap_err();
        assert_eq!(
            rejection.error.code,
            ErrorCode::UnsupportedCapability,
            "{value}"
        );
        assert!(!rejection.error.message.contains("sk-proj-"), "{value}");
    }
    let invalid = [
        // Missing required field, wrong type, unknown field.
        request(
            "command",
            json!({"type": "execution", "payload": {"type": "stop_run", "payload": {"thread": "t"}}}),
        ),
        request(
            "command",
            json!({"type": "execution", "payload": {"type": "stop_run", "payload": {"thread": 1, "run": "r"}}}),
        ),
        request(
            "command",
            json!({"type": "execution", "payload": {"type": "stop_run", "payload": {"thread": "t", "run": "r", (secret.clone()): 1}}}),
        ),
        // The namespace level and the operation level are closed shapes.
        request(
            "command",
            json!({"type": "execution", "payload": {"type": "stop_run", "payload": {"thread": "t", "run": "r"}, (secret.clone()): 1}}),
        ),
        request(
            "query",
            json!({"type": "workspace", "payload": {"type": "get_thread", "payload": {"thread": "t"}}, (secret.clone()): 1}),
        ),
        // Namespace without an operation, or an operation that is not a name.
        request("command", json!({"type": "execution"})),
        request(
            "command",
            json!({"type": "execution", "payload": {"type": 7}}),
        ),
        request("query", json!({"type": 7})),
    ];
    for value in invalid {
        let rejection = decode(value.clone()).unwrap_err();
        assert_eq!(rejection.error.code, ErrorCode::InvalidRequest, "{value}");
        assert!(!rejection.error.message.contains("sk-proj-"), "{value}");
    }
}

#[test]
fn nested_lane_management_is_classified_like_every_other_operation() {
    let manage = |action: Value, extra: Option<(&str, Value)>| {
        let mut payload = json!({"request": {
            "operation_id": "op-1", "lane_id": "lane-1",
            "expected_management_revision": "1", "action": action,
        }});
        if let Some((key, value)) = extra {
            payload[key] = value;
        }
        request(
            "command",
            json!({"type": "lanes", "payload": {"type": "manage_lane", "payload": payload}}),
        )
    };
    let set_policy = json!({"type": "set_policy", "payload": {"policy": "summary"}});
    decode(manage(set_policy.clone(), None)).unwrap();
    decode(manage(json!({"type": "detach"}), None)).unwrap();
    // Provenance is the core's to attach, never the client's.
    assert_eq!(
        code_of(manage(set_policy.clone(), Some(("actor", json!("user"))))),
        ErrorCode::InvalidRequest
    );
    // `detached` is not a policy a client may ask for.
    assert_eq!(
        code_of(manage(
            json!({"type": "set_policy", "payload": {"policy": "detached"}}),
            None
        )),
        ErrorCode::InvalidRequest
    );
    // An action this protocol does not name is unsupported (a newer client),
    // not malformed; the diagnostic never echoes it.
    let unknown = decode(manage(
        json!({"type": "teleport", "payload": {"secret": "s3cr3t"}}),
        None,
    ))
    .unwrap_err();
    assert_eq!(unknown.error.code, ErrorCode::UnsupportedCapability);
    assert!(!format!("{unknown:?}").contains("teleport"));
    assert!(!format!("{unknown:?}").contains("s3cr3t"));
    // A known action with a missing field, a wrong type or an unknown field is
    // malformed.
    for malformed in [
        json!({"type": "set_policy"}),
        json!({"type": "set_policy", "payload": {}}),
        json!({"type": "set_policy", "payload": {"policy": 7}}),
        json!({"type": "set_policy", "payload": {"policy": "summary", "extra": 1}}),
        json!({"type": "reattach", "payload": {"lead_thread_id": "t"}}),
        json!({"type": "detach", "payload": {"extra": 1}}),
        json!({"payload": {"policy": "summary"}}),
        json!({"type": 5}),
    ] {
        assert_eq!(code_of(manage(malformed, None)), ErrorCode::InvalidRequest);
    }
    // Every action name the classifier knows decodes; the list tracks the enum.
    for action in workbench_protocol::namespaces::lanes::MANAGE_LANE_ACTIONS {
        let full = match *action {
            "set_policy" => json!({"type": "set_policy", "payload": {"policy": "summary"}}),
            "reattach" => json!({"type": "reattach", "payload": {
                "lead_thread_id": "t", "expected_lead_root_run_id": "r"}}),
            _ => json!({"type": action}),
        };
        decode(manage(full, None)).unwrap();
    }
    decode(request(
        "query",
        json!({"type": "lanes", "payload": {"type": "get_lane_management", "payload": {"lane_id": "lane-1"}}}),
    ))
    .unwrap();
}

#[test]
fn a_version_1_client_fails_the_handshake_as_a_clear_mismatch() {
    assert_eq!(ProtocolVersion::CURRENT, ProtocolVersion(2));
    let core = workbench_protocol::ids::CoreIdentity::new("core-abc").unwrap();
    let mut hello = client_hello();
    hello.supported_versions = vec![ProtocolVersion(1)];
    let rejected = negotiate(
        &hello,
        &ServerOffer {
            core: &core,
            versions: &[ProtocolVersion::CURRENT],
            capabilities: &Capability::IMPLEMENTED,
        },
    )
    .unwrap_err();
    assert_eq!(rejected.reason, RejectReason::UnsupportedVersion);
    assert_eq!(
        rejected.recommended_action,
        RecommendedAction::UpgradeClient
    );
    assert_eq!(rejected.supported_versions, [ProtocolVersion::CURRENT]);
}

#[test]
fn a_query_is_answered_in_its_own_namespace() {
    let query_namespaces: BTreeSet<String> = samples(queries())
        .into_iter()
        .map(|(name, _)| name.split('/').next().unwrap().to_owned())
        .collect();
    let reply_namespaces: BTreeSet<String> = samples(query_replies())
        .into_iter()
        .map(|(name, _)| name.split('/').next().unwrap().to_owned())
        .collect();
    assert_eq!(query_namespaces, reply_namespaces);
    // Spot-check the pairing the host adapters rely on.
    let reply_names: BTreeSet<String> = samples(query_replies())
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    for expected in [
        "conversation/input_receipts",
        "conversation/committed_history",
        "workspace/projects",
        "workspace/threads",
        "workspace/thread",
        "execution/runs",
        "questions/questions",
        "approvals/approvals",
        "approvals/approval_input",
        "lanes/lane_management",
        "lifecycle/core_status",
        "projection/command_receipt",
        "projection/body",
        "projection/snapshot",
    ] {
        assert!(reply_names.contains(expected), "{expected}");
    }
}

#[test]
fn a_command_receipt_has_no_per_namespace_optional_fields() {
    let plain = serde_json::to_value(CommandReply::accepted()).unwrap();
    assert_eq!(plain, json!({"disposition": "accepted"}));
    let typed = serde_json::to_value(CommandReply::with_result(CommandResult::Conversation(
        ConversationResult::AttachmentDiscarded,
    )))
    .unwrap();
    assert_eq!(
        typed,
        json!({"disposition": "accepted", "result": {"type": "conversation", "payload": {"type": "attachment_discarded"}}})
    );
    // The former per-namespace fields are gone, not tolerated.
    for field in ["conversation", "lane_management"] {
        let mut old = plain.clone();
        old[field] = json!({"type": "attachment_discarded"});
        assert!(
            serde_json::from_value::<CommandReply>(old).is_err(),
            "{field}"
        );
    }
    // A reserved namespace has no result to put there.
    let reserved = json!({"disposition": "accepted", "result": {"type": "settings", "payload": {"type": "x"}}});
    assert!(serde_json::from_value::<CommandReply>(reserved).is_err());
}
