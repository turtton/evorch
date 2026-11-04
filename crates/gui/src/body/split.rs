//! Turns a stored event container into an event plus its deferred body.
//!
//! The container is the event's JSON with every long (or U+E000-leading)
//! string replaced by a token. Exactly one string per event is worth deferring:
//! the body the transcript displays. Every other token position is resolved
//! here, at once, because something other than the transcript may read it (a
//! tool input, a shell result a job card parses). Resolved text is not scanned
//! again: it may itself start with U+E000.

use std::sync::Arc;

use event_bus::EventKind;
use serde_json::Value;

use super::backend::DeferredEvent;
use super::meter::ImmediateClass;
use super::text::BodyText;
use super::types::{BodyError, BodyErrorKind, BodyRef, DeferredBody};

/// Tools whose result a job card parses, so it is never left for later.
const CONTROL_TOOLS: [&str; 2] = ["shell", "bash"];

/// The field of an event's payload that is displayed as a body.
fn primary_field(outer: &str, inner: &str, payload: &Value) -> Option<&'static str> {
    match (outer, inner) {
        ("Message", "MessageCompleted" | "MessagePart" | "FinalResultPublished") => Some("text"),
        ("Message", "MessageDelta" | "ReasoningDelta") => Some("delta"),
        ("Compaction", "Compacted") => Some("summary"),
        ("Tool", "ToolCompleted") => {
            let control = payload
                .get("tool_name")
                .and_then(Value::as_str)
                .is_some_and(|name| CONTROL_TOOLS.contains(&name));
            (!control).then_some("output")
        }
        _ => None,
    }
}

/// Reads one body at once, saying why it is needed now.
pub type ResolveNow<'a> = dyn FnMut(&BodyRef, ImmediateClass) -> Result<Arc<str>, BodyError> + 'a;

/// The result of splitting: the typed kind and its deferred body.
pub struct Split {
    pub kind: EventKind,
    pub body: Option<BodyText>,
}

/// Splits `container` (the stored JSON of an [`EventKind`]).
///
/// `resolve_now` reads a body at once; it is used for token positions that
/// are not the display body, and for the display body too when
/// `defer_display` is false (a reader that joins strings). Any failure fails
/// the event, which then shows as an unreadable one.
///
/// # Errors
/// A malformed token, a body `resolve_now` cannot read, or a container that
/// is not an event.
pub fn split_container(
    mut container: Value,
    run_id_hint: Option<&str>,
    event_id: i64,
    defer_display: bool,
    resolve_now: &mut ResolveNow<'_>,
) -> Result<Split, BodyError> {
    let mut body = None;
    let control_result = control_result_token(&container);
    if let Some(slot) = primary_slot(&mut container)
        && let Some(parsed) = slot.as_str().and_then(BodyRef::parse_token)
    {
        let key = parsed?;
        if defer_display {
            body = Some(BodyText::deferred(DeferredBody::new(
                key,
                run_id_hint,
                Some(event_id),
            )));
            *slot = Value::String(String::new());
        } else {
            *slot = Value::String(resolve_now(&key, ImmediateClass::FeedDisplay)?.to_string());
        }
    }
    resolve_nested(&mut container, control_result.as_deref(), resolve_now)?;
    let kind = serde_json::from_value::<EventKind>(container).map_err(|error| {
        BodyError::new(
            BodyErrorKind::Corrupt,
            format!("the event does not decode: {error}"),
        )
    })?;
    Ok(Split { kind, body })
}

/// The mutable display-body string of `container`, if its kind has one.
fn primary_slot(container: &mut Value) -> Option<&mut Value> {
    let outer = container.get("kind")?.as_str()?.to_owned();
    let payload = container.get_mut("payload")?;
    let inner = payload.get("kind")?.as_str()?.to_owned();
    let inner_payload = payload.get_mut("payload")?;
    let field = primary_field(&outer, &inner, inner_payload)?;
    inner_payload.get_mut(field)
}

/// The `output` string of a control tool's result: the one token position that
/// is read at once because a job card parses it, not because it is nested.
fn control_result_token(container: &Value) -> Option<String> {
    let outer = container.get("kind")?.as_str()?;
    let payload = container.get("payload")?;
    let inner = payload.get("kind")?.as_str()?;
    let inner_payload = payload.get("payload")?;
    if (outer, inner) != ("Tool", "ToolCompleted") {
        return None;
    }
    let control = inner_payload
        .get("tool_name")
        .and_then(Value::as_str)
        .is_some_and(|name| CONTROL_TOOLS.contains(&name));
    if !control {
        return None;
    }
    inner_payload.get("output")?.as_str().map(str::to_owned)
}

/// Replaces every remaining token position, without recursion.
fn resolve_nested(
    root: &mut Value,
    control_result: Option<&str>,
    resolve_now: &mut ResolveNow<'_>,
) -> Result<(), BodyError> {
    let mut stack = vec![root];
    while let Some(value) = stack.pop() {
        match value {
            Value::String(text) => {
                if let Some(parsed) = BodyRef::parse_token(text) {
                    let class = if control_result == Some(text.as_str()) {
                        ImmediateClass::ControlResult
                    } else {
                        ImmediateClass::Nested
                    };
                    let resolved = resolve_now(&parsed?, class)?;
                    *text = resolved.to_string();
                }
            }
            Value::Array(items) => stack.extend(items.iter_mut()),
            Value::Object(map) => stack.extend(map.values_mut()),
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
    Ok(())
}

/// Splits a stored container into a [`DeferredEvent`]'s parts.
///
/// # Errors
/// See [`split_container`].
pub fn deferred_event(
    id: i64,
    meta: event_bus::EventMeta,
    container: Value,
    run_id_hint: Option<&str>,
    defer_display: bool,
    resolve_now: &mut ResolveNow<'_>,
) -> Result<DeferredEvent, BodyError> {
    let split = split_container(container, run_id_hint, id, defer_display, resolve_now)?;
    Ok(DeferredEvent {
        id,
        event: event_bus::Event {
            meta,
            kind: split.kind,
        },
        body: split.body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn token(byte: u8, len: u64) -> (BodyRef, String) {
        let key = BodyRef {
            hash: [byte; 32],
            byte_len: len,
        };
        let token = key.to_token();
        (key, token)
    }

    fn never(_: &BodyRef, _: ImmediateClass) -> Result<Arc<str>, BodyError> {
        panic!("a display body is never read while splitting")
    }

    #[test]
    fn the_display_body_is_deferred_and_left_empty_in_the_event() {
        let (key, token) = token(1, 9000);
        let container = json!({"kind": "Message", "payload": {"kind": "MessageCompleted",
            "payload": {"run_id": "r", "text": token}}});
        let split = split_container(container, Some("r"), 7, true, &mut never).unwrap();
        let body = split.body.expect("deferred");
        assert_eq!(body.bodies().next().unwrap().key, key);
        assert_eq!(body.bodies().next().unwrap().origin.event_id, Some(7));
        let EventKind::Message(event_bus::MessageEvent::MessageCompleted { text, .. }) = split.kind
        else {
            panic!("kind")
        };
        assert_eq!(text, "");
    }

    #[test]
    fn nested_tokens_and_control_results_are_read_at_once_and_not_scanned_again() {
        let (input_key, input_token) = token(2, 5);
        let (out_key, out_token) = token(3, 7);
        // The resolved text itself starts with the escape character.
        let tricky = "\u{e000}evorch-body:v1:not-a-token";
        let mut reads = Vec::new();
        let mut resolve = |key: &BodyRef, class: ImmediateClass| -> Result<Arc<str>, BodyError> {
            reads.push((*key, class));
            Ok(Arc::from(if *key == input_key { "abcde" } else { tricky }))
        };
        let container = json!({"kind": "Tool", "payload": {"kind": "ToolCompleted", "payload": {
            "tool_name": "shell", "call_id": "c", "is_error": false, "output": out_token,
            "detail": {"nested": [input_token]}, "run_id": "r"}}});
        let split = split_container(container, Some("r"), 1, true, &mut resolve).unwrap();
        assert!(split.body.is_none(), "a shell result is resolved at once");
        assert_eq!(
            reads,
            vec![
                (out_key, ImmediateClass::ControlResult),
                (input_key, ImmediateClass::Nested)
            ],
            "a control result and a nested input are told apart"
        );
        let EventKind::Tool(event_bus::ToolEvent::ToolCompleted { output, detail, .. }) =
            split.kind
        else {
            panic!("kind")
        };
        assert_eq!(output.as_deref(), Some(tricky));
        assert_eq!(detail.unwrap()["nested"][0], "abcde");
    }

    #[test]
    fn a_malformed_token_fails_the_event() {
        let container = json!({"kind": "Message", "payload": {"kind": "MessageDelta",
            "payload": {"delta": "\u{e000}broken", "run_id": "r"}}});
        let error = split_container(container, None, 1, true, &mut never)
            .err()
            .expect("fails");
        assert_eq!(error.kind, BodyErrorKind::Corrupt);
    }

    #[test]
    fn a_reader_that_joins_strings_reads_the_display_body_too() {
        let (key, token) = token(4, 6);
        let container = json!({"kind": "Message", "payload": {"kind": "MessageCompleted",
            "payload": {"run_id": "r", "text": token}}});
        let mut reads = Vec::new();
        let mut resolve = |read: &BodyRef, class: ImmediateClass| -> Result<Arc<str>, BodyError> {
            reads.push((*read, class));
            Ok(Arc::from("joined"))
        };
        let split = split_container(container, Some("r"), 1, false, &mut resolve).unwrap();
        assert!(split.body.is_none(), "nothing is left in the store");
        assert_eq!(reads, vec![(key, ImmediateClass::FeedDisplay)]);
        let EventKind::Message(event_bus::MessageEvent::MessageCompleted { text, .. }) = split.kind
        else {
            panic!("kind")
        };
        assert_eq!(text, "joined");
    }
}
