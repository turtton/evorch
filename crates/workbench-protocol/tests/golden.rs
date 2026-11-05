//! JSON golden files for every wire variant.
//!
//! Each `golden/<group>.json` maps a sample name to its exact wire form. A
//! sample must serialize to that JSON and that JSON must deserialize back to
//! the sample, so a change to the wire format shows up as a diff in review.
//! Regenerate deliberately with `WORKBENCH_UPDATE_GOLDEN=1 cargo test -p workbench-protocol --test golden`.

mod common;

use common::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::fmt::Debug;
use std::path::PathBuf;
use workbench_protocol::commands::*;
use workbench_protocol::envelope::*;
use workbench_protocol::errors::*;
use workbench_protocol::events::*;
use workbench_protocol::handshake::*;
use workbench_protocol::queries::*;
use workbench_protocol::views::*;
use workbench_protocol::wire::*;

fn golden_path(group: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{group}.json"))
}

fn check<T>(group: &str, samples: Vec<(String, T)>)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let mut actual = Map::new();
    for (name, sample) in &samples {
        let wire = serde_json::to_value(sample).expect("serialize");
        let back: T = serde_json::from_value(wire.clone())
            .unwrap_or_else(|e| panic!("{group}/{name} does not parse back: {e}"));
        assert_eq!(&back, sample, "{group}/{name} round trip");
        assert!(
            actual.insert(name.clone(), wire).is_none(),
            "duplicate sample {name}"
        );
    }
    let actual = Value::Object(actual);
    let path = golden_path(group);
    if std::env::var_os("WORKBENCH_UPDATE_GOLDEN").is_some() {
        let mut text = serde_json::to_string_pretty(&actual).unwrap();
        text.push('\n');
        std::fs::write(&path, text).unwrap();
    }
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing golden {}: {e}", path.display())),
    )
    .unwrap();
    assert_eq!(actual, expected, "{group} differs from {}", path.display());
    // The golden JSON, not just our serializer's output, must be accepted.
    for (name, sample) in &samples {
        let from_file: T = serde_json::from_value(expected[name].clone()).unwrap();
        assert_eq!(&from_file, sample, "{group}/{name} from golden file");
    }
}

fn type_of<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value).unwrap()["type"]
        .as_str()
        .expect("adjacently tagged")
        .to_owned()
}

fn named<T: Serialize>(values: Vec<T>) -> Vec<(String, T)> {
    values.into_iter().map(|v| (type_of(&v), v)).collect()
}

fn assert_covers(sample_types: &[String], declared: &[&str]) {
    let mut sample: Vec<&str> = sample_types.iter().map(String::as_str).collect();
    let mut declared = declared.to_vec();
    sample.sort_unstable();
    declared.sort_unstable();
    assert_eq!(
        sample, declared,
        "samples must cover exactly the declared variants"
    );
}

#[test]
fn commands_golden_covers_every_variant() {
    let samples = named(commands());
    let types: Vec<String> = samples.iter().map(|(n, _)| n.clone()).collect();
    assert_covers(&types, &Command::TYPES);
    check("commands", samples);
}

#[test]
fn queries_golden_covers_every_variant() {
    let samples = named(queries());
    let types: Vec<String> = samples.iter().map(|(n, _)| n.clone()).collect();
    assert_covers(&types, &Query::TYPES);
    check("queries", samples);
}

#[test]
fn controls_golden_covers_every_variant() {
    let samples = named(controls());
    let types: Vec<String> = samples.iter().map(|(n, _)| n.clone()).collect();
    assert_covers(&types, &Control::TYPES);
    check("controls", samples);
}

#[test]
fn ui_events_golden_covers_every_variant() {
    let samples = named(ui_events());
    let types: Vec<String> = samples.iter().map(|(n, _)| n.clone()).collect();
    assert_covers(&types, &UiEvent::TYPES);
    check("ui_events", samples);
}

#[test]
fn query_replies_golden() {
    check("query_replies", named(query_replies()));
}

#[test]
fn request_envelopes_golden() {
    check(
        "request_envelopes",
        vec![
            (
                "command".into(),
                envelope(1, RequestBody::Command(commands().remove(0))),
            ),
            (
                "query".into(),
                envelope(2, RequestBody::Query(queries().remove(10))),
            ),
            (
                "control".into(),
                envelope(3, RequestBody::Control(controls().remove(0))),
            ),
        ],
    );
}

#[test]
fn reply_envelopes_golden() {
    let mut samples: Vec<(String, ReplyEnvelope)> = reply_payloads()
        .into_iter()
        .map(|p| {
            (
                format!("success_{}", type_of(&p)),
                ReplyEnvelope {
                    request_id: request(1),
                    result: ReplyResult::Success(p),
                },
            )
        })
        .collect();
    samples.push((
        "error".into(),
        ReplyEnvelope {
            request_id: request(1),
            result: ReplyResult::Error(error()),
        },
    ));
    check("reply_envelopes", samples);
}

#[test]
fn server_frames_golden() {
    check(
        "server_frames",
        vec![
            (
                "reply".into(),
                ServerFrame::Reply(ReplyEnvelope {
                    request_id: request(1),
                    result: ReplyResult::Success(ReplyPayload::Ack),
                }),
            ),
            (
                "event".into(),
                ServerFrame::Event(Box::new(event_envelope(UiEvent::ResyncRequired {
                    reason: ResyncReason::CursorTooOld,
                }))),
            ),
        ],
    );
}

#[test]
fn handshake_golden() {
    check(
        "handshake",
        vec![
            ("accepted".into(), HandshakeReply::Accepted(server_hello())),
            ("rejected".into(), HandshakeReply::Rejected(rejected())),
        ],
    );
    check("client_hello", vec![("full".into(), client_hello())]);
}

#[test]
fn core_states_and_capabilities_golden() {
    check(
        "core_states",
        [
            CoreState::Starting,
            CoreState::Ready,
            CoreState::Quiescing,
            CoreState::Draining,
            CoreState::Restarting,
            CoreState::Failed,
        ]
        .into_iter()
        .map(|s| {
            (
                serde_json::to_value(s)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned(),
                s,
            )
        })
        .collect(),
    );
    check(
        "capabilities",
        Capability::ALL
            .into_iter()
            .map(|c| (c.name().to_owned(), c))
            .collect(),
    );
}

#[test]
fn errors_golden_covers_every_code_and_retry() {
    let codes = [
        ErrorCode::InvalidRequest,
        ErrorCode::NotFound,
        ErrorCode::Conflict,
        ErrorCode::AlreadyResolved,
        ErrorCode::UnsupportedVersion,
        ErrorCode::UnsupportedCapability,
        ErrorCode::PermissionDenied,
        ErrorCode::StaleRevision,
        ErrorCode::StaleEpoch,
        ErrorCode::ResyncRequired,
        ErrorCode::Busy,
        ErrorCode::Draining,
        ErrorCode::LimitExceeded,
        ErrorCode::StorageUnavailable,
        ErrorCode::RestoreBlocked,
        ErrorCode::Internal,
    ];
    let retries = [
        RetryAdvice::Never,
        RetryAdvice::SameRequest,
        RetryAdvice::AfterResync,
        RetryAdvice::AfterReconnect,
        RetryAdvice::UserActionRequired,
    ];
    let mut samples: Vec<(String, WorkbenchError)> = codes
        .into_iter()
        .zip(retries.into_iter().cycle())
        .map(|(code, retry)| {
            let name = serde_json::to_value(code)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned();
            (name, WorkbenchError::new(code, "message", retry))
        })
        .collect();
    samples.push(("with_details".into(), error()));
    samples.push((
        "epoch".into(),
        WorkbenchError::stale_epoch(InstanceEpoch(1), InstanceEpoch(2)),
    ));
    samples.push((
        "capability".into(),
        WorkbenchError::unsupported_capability(Capability::Approvals),
    ));
    check("errors", samples);
}

#[test]
fn views_golden() {
    check("views", vec![("thread_summary".into(), thread_summary())]);
    check(
        "provenance",
        vec![("human".into(), human()), ("agent".into(), agent())],
    );
    check(
        "question_targets",
        vec![
            ("user".into(), question(QuestionTarget::User)),
            (
                "lead".into(),
                question(QuestionTarget::Lead {
                    lead_thread: thread(),
                    lead_run: run(),
                }),
            ),
        ],
    );
    check(
        "messages",
        vec![
            (
                "inline".into(),
                message(MessageContent::Inline { text: "hi".into() }),
            ),
            ("body".into(), message(MessageContent::Body(body_ref()))),
        ],
    );
}

#[test]
fn u64_values_are_decimal_strings_beyond_2_pow_53() {
    let wire = serde_json::to_value(Revision(u64::MAX)).unwrap();
    assert_eq!(wire, Value::String("18446744073709551615".into()));
    for bad in [
        "01",
        "+1",
        "-1",
        " 1",
        "1 ",
        "",
        "1e3",
        "18446744073709551616",
        "0x10",
    ] {
        assert!(
            serde_json::from_value::<Revision>(Value::String(bad.into())).is_err(),
            "{bad:?} must be rejected"
        );
    }
    assert!(serde_json::from_value::<Revision>(serde_json::json!(5)).is_err());
    assert_eq!(
        serde_json::from_value::<Revision>(Value::String("0".into())).unwrap(),
        Revision(0)
    );
}
