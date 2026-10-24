use super::{DispatchResult, error, parse, serialize};
use crate::RunId;
use crate::agent_loop::LoopState;
use crate::continuity::HistorySource;
use serde::Deserialize;
use serde_json::{Value, json};

const CHUNK_BYTES: usize = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Args {
    source: Option<String>,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
    #[serde(default)]
    content_offset: usize,
}

pub(super) fn read(state: &LoopState, input: Value) -> DispatchResult {
    let args = match parse::<Args>(input) {
        Ok(args) => args,
        Err(reason) => return error(reason),
    };
    let source = args.source.as_deref().unwrap_or("self");
    let (source_run_id, records) = if source == "self" {
        (
            state.caller_run_id().to_string(),
            crate::continuity::evidence_records(&state.context.messages),
        )
    } else if let Some(snapshot) = state
        .continuity
        .sources
        .iter()
        .find(|item| item.source == source)
    {
        let records = match serde_json::from_str::<Vec<Value>>(&snapshot.records_json) {
            Ok(messages) => messages,
            Err(reason) => return error(format!("history_source_invalid: {reason}")),
        };
        (snapshot.source_run_id.clone(), records)
    } else {
        return error(denied(
            source,
            state.caller_run_id(),
            &state.continuity.sources,
        ));
    };
    match page(
        source,
        &source_run_id,
        &records,
        args.offset,
        args.limit.unwrap_or(4),
        args.content_offset,
    ) {
        Ok(value) => serialize(&value),
        Err(reason) => error(reason),
    }
}

/// Builds the denial guidance; `history_source_denied` stays first for callers
/// that match on the code.
fn denied(source: &str, caller: RunId, sources: &[HistorySource]) -> String {
    let mut message = String::from("history_source_denied: ");
    if let Some(handle) = suggestion(source, caller, sources) {
        message.push_str(&format!("did you mean \"{handle}\"? "));
    }
    message.push_str("Source must be \"self\" or a handoff:<run-id> handle listed in [history-sources]. Available: \"self\" (this run's history)");
    if sources.is_empty() {
        message.push_str("; no handoff handles in this run.");
    } else {
        // freeze enforces the same cap, so this lists every handle.
        for item in sources.iter().take(crate::continuity::MAX_SOURCES) {
            message.push_str(&format!(", \"{}\"", item.source));
        }
        message.push('.');
    }
    message.push_str(" Run IDs and GUI thread IDs (escalation-run-<id>, thread-<n>) are not handles; other threads' and runs' histories cannot be read with this tool. Instead, use run_output for a direct child or parent; if the user wants another thread's content, tell them it cannot be read here and ask them to share the needed part.");
    message
}

/// Maps a run ID, or an escalation thread ID named after one, to the source
/// that reads that run's history.
fn suggestion<'a>(source: &str, caller: RunId, sources: &'a [HistorySource]) -> Option<&'a str> {
    let run = source
        .strip_prefix("escalation-")
        .unwrap_or(source)
        .parse::<RunId>()
        .ok()?;
    if run == caller {
        return Some("self");
    }
    sources
        .iter()
        .find(|item| {
            item.source_run_id
                .parse::<RunId>()
                .is_ok_and(|id| id == run)
        })
        .map(|item| item.source.as_str())
}

fn page(
    source: &str,
    run: &str,
    records: &[Value],
    offset: usize,
    limit: usize,
    content_offset: usize,
) -> Result<Value, String> {
    if !(1..=4).contains(&limit) {
        return Err("history_limit_invalid: limit must be 1-4".into());
    }
    if offset > records.len() || (offset == records.len() && content_offset != 0) {
        return Err("history_offset_invalid".into());
    }
    let mut result = Vec::new();
    let mut next_offset = offset;
    let mut next_content_offset = None;
    for (index, record) in records.iter().enumerate().skip(offset).take(limit) {
        let text = serde_json::to_string(record).map_err(|e| e.to_string())?;
        let start = if index == offset { content_offset } else { 0 };
        if start > text.len() || !text.is_char_boundary(start) {
            return Err("history_content_offset_invalid: expected a UTF-8 boundary".into());
        }
        let mut end = start.saturating_add(CHUNK_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        result.push(json!({"offset":index, "content_offset":start, "content":&text[start..end], "total_bytes":text.len()}));
        if end < text.len() {
            next_offset = index;
            next_content_offset = Some(end);
            break;
        }
        next_offset = index + 1;
    }
    Ok(
        json!({"source":source,"source_run_id":run,"record_count":records.len(),"records":result,
        "next_offset":(next_offset < records.len()).then_some(next_offset),"next_content_offset":next_content_offset}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = "history_source_denied: ";
    const ULID_RUN: &str = "run-01m5mazm28w62cwmpqh16nkh8s";

    fn handle(run: &str) -> HistorySource {
        HistorySource {
            source: format!("handoff:{run}"),
            source_run_id: run.into(),
            records_json: "[]".into(),
            message_count: 0,
        }
    }

    #[test]
    fn denial_without_handles_offers_only_self_and_says_there_are_none() {
        let message = denied(&format!("escalation-{ULID_RUN}"), RunId::new(1), &[]);
        assert!(message.starts_with(CODE), "{message}");
        assert!(message.contains(r#""self""#), "{message}");
        assert!(message.contains("no handoff handles"), "{message}");
        assert!(!message.contains("did you mean"), "{message}");
        assert!(!message.contains(r#""handoff:"#), "{message}");
    }

    #[test]
    fn denial_lists_handles_and_suggests_the_matching_one_for_run_or_thread_ids() {
        let caller = RunId::new(1);
        let sources = [handle(ULID_RUN), handle("run-7")];
        let escalation_thread = format!("escalation-{ULID_RUN}");
        for (source, suggestion) in [
            (ULID_RUN, Some("handoff:run-01m5mazm28w62cwmpqh16nkh8s")),
            (
                escalation_thread.as_str(),
                Some("handoff:run-01m5mazm28w62cwmpqh16nkh8s"),
            ),
            ("run-7", Some("handoff:run-7")),
            ("run-1", Some("self")),
            ("escalation-run-1", Some("self")),
            ("run-8", None),
            ("escalation-run-8", None),
            ("handoff:run-8", None),
            ("thread-3", None),
        ] {
            let message = denied(source, caller, &sources);
            assert!(message.starts_with(CODE), "{message}");
            for listed in [
                "self",
                "handoff:run-01m5mazm28w62cwmpqh16nkh8s",
                "handoff:run-7",
            ] {
                assert!(message.contains(&format!("\"{listed}\"")), "{message}");
            }
            assert!(!message.contains("no handoff handles"), "{message}");
            match suggestion {
                Some(handle) => assert!(
                    message.contains(&format!("did you mean \"{handle}\"?")),
                    "{source}: {message}"
                ),
                None => assert!(!message.contains("did you mean"), "{source}: {message}"),
            }
        }
    }

    #[test]
    fn pages_reassemble_long_utf8_evidence_and_exclude_hidden_content() {
        let mut context = crate::AgentContext::new(crate::RunId::new(1), crate::Role::Worker);
        context.push_system("hidden instructions");
        context.push_user(&"日本語証拠".repeat(1800));
        context.push_user("last evidence");
        let expected =
            serde_json::to_string(&crate::continuity::evidence_records(&context.messages)[0])
                .unwrap();
        let mut gathered = String::new();
        let mut cursor = 0;
        loop {
            let result = page(
                "self",
                "run-1",
                &crate::continuity::evidence_records(&context.messages),
                0,
                1,
                cursor,
            )
            .unwrap();
            let chunk = result["records"][0]["content"].as_str().unwrap();
            assert!(chunk.len() <= CHUNK_BYTES);
            gathered.push_str(chunk);
            match result["next_content_offset"].as_u64() {
                Some(next) => cursor = next as usize,
                None => {
                    assert_eq!(result["next_offset"], 1);
                    break;
                }
            }
        }
        assert_eq!(gathered, expected);
        assert!(!gathered.contains("hidden instructions"));
        let text_offset = expected.find('日').unwrap();
        assert!(
            page(
                "self",
                "run-1",
                &crate::continuity::evidence_records(&context.messages),
                0,
                1,
                text_offset + 1
            )
            .is_err()
        );
        assert!(
            page(
                "self",
                "run-1",
                &crate::continuity::evidence_records(&context.messages),
                usize::MAX,
                1,
                0
            )
            .is_err()
        );
        assert!(
            page(
                "self",
                "run-1",
                &crate::continuity::evidence_records(&context.messages),
                0,
                5,
                0
            )
            .is_err()
        );
    }
}
