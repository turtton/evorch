//! `thread_read`: an overview of one visible thread, or one run's messages by ref.

use serde::Deserialize;
use serde_json::{Value, json};
use storage::RunLedgerEntry;
use storage::thread_index::{GuardedReference, GuardedResolution};
use storage::threads::{RunTerminalSummary, ThreadRow, ThreadSource};

use super::conversation::{Around, parse_around};
use super::normalize::{Block, BlockKind, NormalizedRun, Ref, UNTRUSTED_NOTE, normalize};
use super::search::lineage;
use super::visibility::Target;
use super::{
    MAX_OUTPUT_BYTES, Members, STORE_ERROR, Scope, bounded_notes, clip_bytes, clip_chars,
    kind_label, not_in_thread, role_label,
};

const DEFAULT_MAX_BYTES: usize = 6 * 1024;
const MIN_MAX_BYTES: usize = 1024;
const DEFAULT_NEIGHBOURS: usize = 2;
const MAX_NEIGHBOURS: usize = 8;
const FINAL_REPORT_BYTES: usize = 600;
/// Child runs whose saved histories are decoded for their last tool error.
const MAX_CHILD_SUMMARIES: usize = 10;
const MAX_CHILD_THREADS: usize = 10;

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum View {
    #[default]
    Overview,
    Messages,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadArgs {
    thread: String,
    #[serde(default)]
    view: View,
    #[serde(default, deserialize_with = "super::empty_as_none")]
    run: Option<String>,
    #[serde(default, deserialize_with = "super::empty_as_none")]
    around: Option<String>,
    before: Option<usize>,
    after: Option<usize>,
    max_bytes: Option<usize>,
    #[serde(default, deserialize_with = "super::empty_as_none")]
    cursor: Option<String>,
}

pub(super) fn run(scope: &Scope<'_>, args: ReadArgs) -> Result<Value, String> {
    let max_bytes = args.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    if !(MIN_MAX_BYTES..=MAX_OUTPUT_BYTES).contains(&max_bytes) {
        return Err(format!(
            "thread_read_args_invalid: max_bytes must be {MIN_MAX_BYTES}-{MAX_OUTPUT_BYTES}"
        ));
    }
    if [args.before, args.after]
        .into_iter()
        .flatten()
        .any(|count| count > MAX_NEIGHBOURS)
    {
        return Err(format!(
            "thread_read_args_invalid: before and after must be at most {MAX_NEIGHBOURS}"
        ));
    }
    if args.around.is_none() && (args.before.is_some() || args.after.is_some()) {
        return Err("thread_read_args_invalid: before and after apply with around".into());
    }
    let row = scope.resolve_argument(&args.thread)?;
    let members = scope.members(&row)?;
    match args.view {
        View::Overview => {
            if args.run.is_some() || args.around.is_some() || args.cursor.is_some() {
                return Err(
                    "thread_read_args_invalid: run, around and cursor apply to view=messages"
                        .into(),
                );
            }
            overview(scope, &row, &members, max_bytes)
        }
        View::Messages => messages(scope, &row, &members, &args, max_bytes),
    }
}

/// Text sizes per detail level; a smaller level is tried when output exceeds max_bytes.
#[derive(Clone, Copy)]
struct Detail {
    text: usize,
    note: usize,
    questions: usize,
    children: usize,
    exchanges: usize,
}

const DETAILS: [Detail; 3] = [
    Detail {
        text: FINAL_REPORT_BYTES,
        note: 1200,
        questions: 6,
        children: MAX_CHILD_SUMMARIES,
        exchanges: 2,
    },
    Detail {
        text: 300,
        note: 400,
        questions: 3,
        children: 4,
        exchanges: 1,
    },
    Detail {
        text: 120,
        note: 0,
        questions: 0,
        children: 0,
        exchanges: 0,
    },
];

struct Child {
    summary: RunTerminalSummary,
    phase: Option<String>,
    reason: Option<String>,
    last_error: Option<Value>,
}

struct Exchange {
    user: (Ref, String),
    assistant: Option<(Ref, String)>,
}

fn overview(
    scope: &Scope<'_>,
    row: &ThreadRow,
    members: &Members,
    max_bytes: usize,
) -> Result<Value, String> {
    let fail = |_| STORE_ERROR.to_owned();
    let mut notes = Vec::new();
    if row.project_root.is_none() {
        notes.push("project root unknown: visible only within its thread and lineage or when the user names it".to_owned());
    }
    if row.source == ThreadSource::Backfill {
        notes.push(
            "definition inferred from older runs (backfill), not confirmed by the GUI or runtime"
                .into(),
        );
    }
    if row.title.is_none() {
        notes.push("no title recorded: the GUI has not mirrored this thread".into());
    }
    if members.summaries.is_empty() {
        notes.push("no saved runs".into());
    }
    if let (Some(recorded), Some(primary)) = (&row.root_run_id, &members.primary)
        && recorded != primary
    {
        if members.summaries.contains_key(recorded) {
            notes.push(format!(
                "recorded root {recorded} is older than the latest saved root {primary}"
            ));
        } else {
            notes.push(format!("recorded root {recorded} has no saved history"));
        }
    }
    let root = members.primary.as_deref().map(|id| {
        let saved = members.summaries.get(id);
        let (phase, reason) = scope.status(id, saved.map(|saved| saved.terminal_phase.as_str()));
        (id.to_owned(), saved.cloned(), phase, reason)
    });
    let ids: Vec<String> = members.runs.iter().map(|run| run.run_id.clone()).collect();
    let roots: Vec<String> = members
        .runs
        .iter()
        .filter(|run| run.depth == 0)
        .map(|run| run.run_id.clone())
        .collect();
    let note: Option<RunLedgerEntry> = scope
        .store
        .latest_ledgers(&roots)
        .map_err(fail)?
        .into_iter()
        .max_by_key(|entry| entry.seq);
    let questions = scope.store.user_questions_for_runs(&ids).map_err(fail)?;
    let mut children: Vec<&RunTerminalSummary> = members
        .runs
        .iter()
        .filter(|run| run.depth > 0)
        .filter_map(|run| members.summaries.get(&run.run_id))
        .collect();
    let child_count = members.runs.iter().filter(|run| run.depth > 0).count();
    children.sort_by(|left, right| {
        (right.updated_at_ns, &right.run_id).cmp(&(left.updated_at_ns, &left.run_id))
    });
    children.truncate(MAX_CHILD_SUMMARIES);
    let child_ids: Vec<String> = children.iter().map(|child| child.run_id.clone()).collect();
    let records = scope.store.run_contexts_for(&child_ids).map_err(fail)?;
    let children: Vec<Child> = children
        .into_iter()
        .map(|summary| {
            let (phase, reason) = scope.status(&summary.run_id, Some(&summary.terminal_phase));
            let last_error = records
                .iter()
                .find(|record| record.run_id == summary.run_id)
                .and_then(|record| normalize(record, &scope.redactor).ok())
                .and_then(|run| last_tool_error(&run));
            Child {
                summary: summary.clone(),
                phase,
                reason,
                last_error,
            }
        })
        .collect();
    let exchanges = match &root {
        Some((id, Some(_), _, _)) => {
            let record = scope
                .store
                .run_contexts_for(std::slice::from_ref(id))
                .map_err(fail)?;
            match record
                .first()
                .map(|record| normalize(record, &scope.redactor))
            {
                Some(Ok(run)) => last_exchanges(&run, 2),
                Some(Err(_)) => {
                    notes.push(format!("{id} history could not be decoded"));
                    Vec::new()
                }
                None => Vec::new(),
            }
        }
        _ => Vec::new(),
    };
    let child_threads: Vec<Value> = scope
        .store
        .thread_children(&row.thread_id)
        .map_err(fail)?
        .into_iter()
        .filter(|child| scope.resolve(&child.thread_id).is_ok())
        .take(MAX_CHILD_THREADS)
        .map(|child| {
            json!({
                "thread_id": child.thread_id,
                "kind": child.lineage_kind.map(|kind| kind.as_str()),
            })
        })
        .collect();
    let notes = bounded_notes(notes);
    let render = |detail: Detail, truncated: bool| {
        json!({
            "note": UNTRUSTED_NOTE,
            "view": "overview",
            "thread": {
                "thread_id": row.thread_id,
                "title": row.title.as_deref().map(|title| clip_chars(&scope.redact(title), 200)),
                "project": {"id": row.project_id, "root": row.project_root},
                "lineage": lineage(row),
                "child_threads": child_threads,
                "archived": row.archived,
                "superseded": row.superseded,
                "source": row.source.as_str(),
                "created_at_ns": row.created_at_ns,
                "updated_at_ns": row.updated_at_ns,
            },
            "root_run": root.as_ref().map(|(id, saved, phase, reason)| json!({
                "run_id": id,
                "role": saved.as_ref().map(|saved| saved.role.clone()),
                "phase": phase,
                "terminal_reason": reason.as_deref().map(|reason| clip_bytes(reason, detail.text)),
                "final_report": saved
                    .as_ref()
                    .filter(|saved| saved.terminal_phase == "Done")
                    .and_then(|saved| saved.final_text.as_deref())
                    .map(|text| clip_bytes(&scope.redact(text), detail.text)),
            })),
            "working_note": note.as_ref().filter(|_| detail.note > 0).map(|note| json!({
                "run_id": note.run_id,
                "created_at_ns": note.created_at_ns,
                "body": clip_bytes(&scope.redact(&note.body), detail.note),
                "cleared": note.body.is_empty(),
            })),
            "user_questions": {
                "total": questions.len(),
                "latest": questions
                    .iter()
                    .rev()
                    .take(detail.questions)
                    .map(|question| json!({
                        "id": question.id,
                        "run_id": question.run_id,
                        "title": clip_chars(&scope.redact(&question.title), 200),
                        "status": if question.answer.is_some() { "answered" } else { "pending" },
                        "answer": question.answer.as_deref().map(|answer| clip_chars(&scope.redact(answer), 200)),
                        "answer_origin": question.answer_origin,
                    }))
                    .collect::<Vec<_>>(),
            },
            "child_runs": {
                "total": child_count,
                "latest": children
                    .iter()
                    .take(detail.children)
                    .map(|child| json!({
                        "run_id": child.summary.run_id,
                        "name": clip_chars(&scope.redact(&child.summary.name), 80),
                        "role": child.summary.role,
                        "phase": child.phase,
                        "terminal_reason": child.reason.as_deref().map(|reason| clip_chars(reason, 200)),
                        "last_tool_error": child.last_error,
                    }))
                    .collect::<Vec<_>>(),
            },
            "last_exchanges": exchanges
                .iter()
                .rev()
                .take(detail.exchanges)
                .rev()
                .map(|exchange| json!({
                    "user_ref": exchange.user.0.to_string(),
                    "user": clip_bytes(&exchange.user.1, detail.text),
                    "assistant_ref": exchange.assistant.as_ref().map(|(reference, _)| reference.to_string()),
                    "assistant": exchange.assistant.as_ref().map(|(_, text)| clip_bytes(text, detail.text)),
                }))
                .collect::<Vec<_>>(),
            "inconsistencies": notes,
            "truncated": truncated,
        })
    };
    for (level, detail) in DETAILS.iter().enumerate() {
        let value = scope.output(render(*detail, level > 0));
        if value.to_string().len() <= max_bytes {
            return Ok(value);
        }
    }
    Ok(scope.output(json!({
        "note": UNTRUSTED_NOTE,
        "view": "overview",
        "thread": {"thread_id": row.thread_id},
        "truncated": true,
        "inconsistencies": ["overview exceeds max_bytes; raise max_bytes or use view=messages"],
    })))
}

/// The last failed tool result, classified by its leading error code.
fn last_tool_error(run: &NormalizedRun) -> Option<Value> {
    run.messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .rfind(|block| block.kind == BlockKind::ToolResult && block.is_error == Some(true))
        .map(|block| {
            json!({
                "tool_name": block.tool_name.as_deref().map(super::identifier),
                "class": error_class(&block.text),
            })
        })
}

fn error_class(text: &str) -> String {
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text)
        && let Some(code) = map.get("code").and_then(Value::as_str)
    {
        return clip_chars(code, 64);
    }
    let code: String = text
        .chars()
        .take_while(|character| character.is_ascii_lowercase() || *character == '_')
        .take(65)
        .collect();
    if !code.is_empty() && code.len() <= 64 && text[code.len()..].starts_with(':') {
        code
    } else {
        "error".into()
    }
}

/// The last `count` human-turn texts and the final assistant text that answered each.
fn last_exchanges(run: &NormalizedRun, count: usize) -> Vec<Exchange> {
    let text_of = |blocks: &[Block]| -> Option<(usize, String)> {
        let texts: Vec<&Block> = blocks
            .iter()
            .filter(|block| block.kind == BlockKind::Text)
            .collect();
        let first = texts.first()?;
        Some((
            first.index,
            texts
                .iter()
                .map(|block| block.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    };
    let turns: Vec<usize> = run
        .messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == providers::Role::User && text_of(&message.blocks).is_some()
        })
        .map(|(position, _)| position)
        .collect();
    let reference = |position: usize, block: usize| Ref {
        run_id: run.run_id,
        ordinal: run.messages[position].ordinal,
        block,
    };
    turns
        .iter()
        .enumerate()
        .rev()
        .take(count)
        .rev()
        .filter_map(|(turn, &position)| {
            let (block, text) = text_of(&run.messages[position].blocks)?;
            let end = turns.get(turn + 1).copied().unwrap_or(run.messages.len());
            let assistant = (position + 1..end).rev().find_map(|candidate| {
                let message = &run.messages[candidate];
                if message.role != providers::Role::Assistant {
                    return None;
                }
                text_of(&message.blocks).map(|(block, text)| (reference(candidate, block), text))
            });
            Some(Exchange {
                user: (reference(position, block), text),
                assistant,
            })
        })
        .collect()
}

/// Where the next response continues: a message ordinal, an original block
/// index and a UTF-8 byte offset into that block's text.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Position {
    ordinal: usize,
    block: usize,
    offset: usize,
}

fn messages(
    scope: &Scope<'_>,
    row: &ThreadRow,
    members: &Members,
    args: &ReadArgs,
    max_bytes: usize,
) -> Result<Value, String> {
    let around = args
        .around
        .as_deref()
        .map(parse_around)
        .transpose()
        .map_err(|reason| format!("thread_ref_invalid: {reason}"))?;
    let named_run = match Target::parse(&args.thread) {
        Target::Run(id) => Some(id.to_owned()),
        Target::Thread(_) => None,
    };
    let run_id = match (&around, &args.run) {
        (Some(focus), Some(run)) if &focus.run_id() != run => {
            return Err("thread_read_args_invalid: around names a different run than run".into());
        }
        (Some(focus), _) => focus.run_id(),
        (None, Some(run)) => run.clone(),
        (None, None) => named_run
            .filter(|run| members.is_saved_member(run))
            .or_else(|| members.primary.clone())
            .ok_or_else(|| format!("thread_empty: thread {} has no saved runs", row.thread_id))?,
    };
    let is_member = if matches!(around, Some(Around::Guarded(_))) {
        // Notes and tombstones can outlive a snapshot. Thread membership still
        // authorizes first; the canonical resolver decides stale/deleted.
        members.runs.iter().any(|member| member.run_id == run_id)
    } else {
        members.is_saved_member(&run_id)
    };
    if !is_member {
        return Err(not_in_thread(&run_id, &row.thread_id));
    }
    let fetch = || {
        scope
            .store
            .run_contexts_for(std::slice::from_ref(&run_id))
            .map_err(|_| STORE_ERROR.to_owned())?
            .into_iter()
            .next()
            .ok_or_else(|| not_in_thread(&run_id, &row.thread_id))
    };
    let (record, around) = match around {
        // Authorized above; the guard is checked against the canonical
        // snapshot or note in the same read that returns it.
        Some(Around::Guarded(reference)) => {
            let text = args.around.as_deref().unwrap_or_default();
            match resolve_guarded(scope, &reference, text)? {
                Resolved::Message(record, focus) => (*record, Some(focus)),
                Resolved::Note(note) => return Ok(note_view(scope, row, &note, text, max_bytes)),
            }
        }
        Some(Around::Plain(reference)) => (fetch()?, Some(reference)),
        None => (fetch()?, None),
    };
    let run = normalize(&record, &scope.redactor)
        .map_err(|_| format!("thread_history_invalid: {run_id} history could not be decoded"))?;
    let count = run.messages.len();
    let (from, to) = match &around {
        Some(focus) => {
            if focus.ordinal >= count {
                return Err(format!(
                    "thread_ref_out_of_range: {focus} is beyond the {count} messages of {run_id}"
                ));
            }
            (
                focus
                    .ordinal
                    .saturating_sub(args.before.unwrap_or(DEFAULT_NEIGHBOURS)),
                (focus.ordinal + args.after.unwrap_or(DEFAULT_NEIGHBOURS) + 1).min(count),
            )
        }
        None => (0, count),
    };
    let start = match args.cursor.as_deref() {
        None => Position {
            ordinal: from,
            block: 0,
            offset: 0,
        },
        Some(cursor) => parse_cursor(cursor, &run, from, to)?,
    };
    let render = |messages: &[Value], next: Option<&str>| {
        scope.output(json!({
            "note": UNTRUSTED_NOTE,
            "view": "messages",
            "thread_id": row.thread_id,
            "run_id": run_id,
            "message_count": count,
            "first_ordinal": from,
            "last_ordinal": to.checked_sub(1).filter(|last| *last >= from),
            "focus": around.map(|focus| focus.to_string()),
            "messages": messages,
            "next_cursor": next,
        }))
    };
    // Room for any cursor this response could return.
    let reserve = cursor_text(
        run.run_id,
        Position {
            ordinal: usize::MAX,
            block: usize::MAX,
            offset: usize::MAX,
        },
    );
    let fits = |messages: &[Value]| render(messages, Some(&reserve)).to_string().len() <= max_bytes;
    let mut out: Vec<Value> = Vec::new();
    let mut next = None;
    'messages: for message in &run.messages[start.ordinal.min(to)..to] {
        out.push(json!({
            "ordinal": message.ordinal,
            "role": role_label(message.role),
            "blocks": [],
        }));
        if !fits(&out) {
            out.pop();
            next = Some(Position {
                ordinal: message.ordinal,
                block: 0,
                offset: 0,
            });
            break;
        }
        for block in &message.blocks {
            let resumed = message.ordinal == start.ordinal;
            if resumed && block.index < start.block {
                continue;
            }
            let offset = if resumed && block.index == start.block {
                start.offset
            } else {
                0
            };
            let reference = Ref {
                run_id: run.run_id,
                ordinal: message.ordinal,
                block: block.index,
            };
            if offset == 0 {
                blocks_of(&mut out).push(block_json(block, reference, None));
                if fits(&out) {
                    continue;
                }
                blocks_of(&mut out).pop();
            }
            // Split the text at a UTF-8 boundary to use the remaining budget.
            let rest = &block.text[offset..];
            blocks_of(&mut out).push(block_json(block, reference, Some((offset, ""))));
            let room = max_bytes.saturating_sub(render(&out, Some(&reserve)).to_string().len());
            blocks_of(&mut out).pop();
            let mut length = json_prefix(rest, room);
            loop {
                blocks_of(&mut out).push(block_json(
                    block,
                    reference,
                    Some((offset, &rest[..length])),
                ));
                if length == 0 || fits(&out) {
                    break;
                }
                blocks_of(&mut out).pop();
                length = floor_boundary(rest, length / 2);
            }
            if length == 0 {
                blocks_of(&mut out).pop();
            } else if length == rest.len() {
                continue;
            }
            next = Some(Position {
                ordinal: message.ordinal,
                block: block.index,
                offset: offset + length,
            });
            if blocks_of(&mut out).is_empty() && !message.blocks.is_empty() {
                out.pop();
            }
            break 'messages;
        }
    }
    if next == Some(start) || !fits(&out) {
        return Err(
            "thread_output_too_large: metadata cannot fit max_bytes; raise max_bytes".into(),
        );
    }
    let next = next.map(|position| cursor_text(run.run_id, position));
    Ok(render(&out, next.as_deref()))
}

enum Resolved {
    Message(Box<storage::RunContextRecord>, Ref),
    Note(RunLedgerEntry),
}

/// Check a guarded ref against the canonical snapshot or latest note. A
/// changed snapshot, a moved position or a newer or cleared note is
/// `stale_reference`; it never resolves to whatever now sits there.
fn resolve_guarded(
    scope: &Scope<'_>,
    reference: &GuardedReference,
    text: &str,
) -> Result<Resolved, String> {
    let shown = clip_chars(text, 160);
    let resolution = scope
        .store
        .with_database(|db| Ok(db.resolve_guarded_reference(reference)))
        .map_err(|_| STORE_ERROR.to_owned())?
        .map_err(|_| STORE_ERROR.to_owned())?;
    match resolution {
        GuardedResolution::Message {
            bundle,
            ordinal,
            block_index,
        } => Ok(Resolved::Message(
            Box::new(bundle.record),
            Ref {
                run_id: reference
                    .run_id
                    .parse()
                    .map_err(|_| format!("thread_ref_invalid: {shown}"))?,
                ordinal,
                block: block_index,
            },
        )),
        GuardedResolution::WorkingNote(note) => Ok(Resolved::Note(note)),
        GuardedResolution::StaleReference => Err(format!(
            "stale_reference: {shown} no longer matches the saved conversation or working note; search again for a current ref"
        )),
        GuardedResolution::SourceDeleted => Err(format!(
            "source_deleted: the run behind {shown} was deleted"
        )),
        GuardedResolution::UnsupportedVersion => Err(format!(
            "thread_ref_invalid: {shown} has an unsupported guard version"
        )),
    }
}

/// The latest working note a guarded note ref names, within `max_bytes`.
fn note_view(
    scope: &Scope<'_>,
    row: &ThreadRow,
    note: &RunLedgerEntry,
    focus: &str,
    max_bytes: usize,
) -> Value {
    let render = |body: &str, truncated: bool| {
        scope.output(json!({
            "note": UNTRUSTED_NOTE,
            "view": "working_note",
            "thread_id": row.thread_id,
            "run_id": note.run_id,
            "note_seq": note.seq,
            "focus": clip_chars(focus, 160),
            "body": body,
            "truncated": truncated,
        }))
    };
    let body = scope.redact(&note.body);
    let mut room = body.len();
    loop {
        let truncated = room < body.len();
        let value = render(&clip_bytes(&body, room), truncated);
        let size = value.to_string().len();
        if size <= max_bytes || room == 0 {
            return value;
        }
        room = room.saturating_sub(size - max_bytes);
    }
}

fn blocks_of(out: &mut [Value]) -> &mut Vec<Value> {
    out.last_mut()
        .and_then(|message| message["blocks"].as_array_mut())
        .expect("a message shell is open while its blocks are added")
}

fn block_json(block: &Block, reference: Ref, chunk: Option<(usize, &str)>) -> Value {
    let mut value = json!({
        "ref": reference.to_string(),
        "kind": kind_label(block.kind),
    });
    match (chunk, &block.input) {
        // Nested, so tool input is not escaped twice.
        (None, Some(input)) => value["input"] = input.clone(),
        (None, None) => value["text"] = json!(block.text),
        (Some((offset, text)), _) => {
            value["text"] = json!(text);
            value["text_offset"] = json!(offset);
            value["text_bytes"] = json!(block.text.len());
            value["truncated"] = json!(offset + text.len() < block.text.len());
        }
    }
    if let Some(name) = &block.tool_name {
        value["tool_name"] = json!(super::identifier(name));
    }
    if let Some(id) = &block.call_id {
        value["call_id"] = json!(super::identifier(id));
    }
    if let Some(is_error) = block.is_error {
        value["is_error"] = json!(is_error);
    }
    if block.repeat_count > 1 {
        value["repeat_count"] = json!(block.repeat_count);
    }
    value
}

/// Longest prefix (in bytes, on a char boundary) whose JSON string escape fits `room`.
fn json_prefix(text: &str, room: usize) -> usize {
    let mut used = 0;
    for (offset, character) in text.char_indices() {
        let escaped = match character {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
            character if (character as u32) < 0x20 => 6,
            character => character.len_utf8(),
        };
        if used + escaped > room {
            return offset;
        }
        used += escaped;
    }
    text.len()
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn cursor_text(run: crate::RunId, position: Position) -> String {
    let reference = Ref {
        run_id: run,
        ordinal: position.ordinal,
        block: position.block,
    };
    format!("{reference}@{}", position.offset)
}

fn parse_cursor(
    cursor: &str,
    run: &NormalizedRun,
    from: usize,
    to: usize,
) -> Result<Position, String> {
    let invalid = || {
        "thread_cursor_invalid: pass next_cursor with the same thread, run and around".to_owned()
    };
    let (reference, offset) = cursor.split_once('@').ok_or_else(invalid)?;
    let reference: Ref = reference.parse().map_err(|_| invalid())?;
    let offset: usize = offset.parse().map_err(|_| invalid())?;
    if reference.run_id != run.run_id || !(from..to).contains(&reference.ordinal) {
        return Err(invalid());
    }
    if offset > 0 {
        let block = run.messages[reference.ordinal]
            .blocks
            .iter()
            .find(|block| block.index == reference.block)
            .ok_or_else(invalid)?;
        if offset > block.text.len() || !block.text.is_char_boundary(offset) {
            return Err(invalid());
        }
    }
    Ok(Position {
        ordinal: reference.ordinal,
        block: reference.block,
        offset,
    })
}
