//! `thread_grep`: linear scan of one visible thread's normalized conversation.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::{Value, json};

use super::normalize::{BlockKind, Ref, UNTRUSTED_NOTE, collapse_repeats, normalize};
use super::{
    MAX_OUTPUT_BYTES, STORE_ERROR, Scope, bounded_notes, expression, fit, kind_label, literal,
    not_in_thread, role_label, snippet,
};

const DEFAULT_CONTEXT_CHARS: usize = 120;
const MAX_CONTEXT_CHARS: usize = 400;
const DEFAULT_MAX_HITS: usize = 20;
const MAX_HITS: usize = 50;
const MAX_RUNS: usize = 64;
/// Characters of the matched text itself shown in a hit.
const MATCH_CHARS: usize = 200;

/// Block classes the `kinds` filter selects; text is split by its author.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GrepKind {
    User,
    Assistant,
    ToolUse,
    ToolResult,
    Image,
}

impl GrepKind {
    fn of(role: providers::Role, kind: BlockKind) -> Self {
        match kind {
            BlockKind::Text if role == providers::Role::Assistant => Self::Assistant,
            BlockKind::Text => Self::User,
            BlockKind::ToolUse => Self::ToolUse,
            BlockKind::ToolResult => Self::ToolResult,
            BlockKind::ImageNote => Self::Image,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrepArgs {
    thread: String,
    pattern: String,
    #[serde(default)]
    regex: bool,
    runs: Option<Vec<String>>,
    kinds: Option<Vec<GrepKind>>,
    context_chars: Option<usize>,
    max_hits: Option<usize>,
    #[serde(default = "super::yes")]
    collapse_repeats: bool,
    cursor: Option<String>,
}

pub(super) fn run(scope: &Scope<'_>, args: GrepArgs) -> Result<Value, String> {
    let context = args.context_chars.unwrap_or(DEFAULT_CONTEXT_CHARS);
    if context > MAX_CONTEXT_CHARS {
        return Err(format!(
            "thread_grep_args_invalid: context_chars must be at most {MAX_CONTEXT_CHARS}"
        ));
    }
    let max_hits = args.max_hits.unwrap_or(DEFAULT_MAX_HITS);
    if !(1..=MAX_HITS).contains(&max_hits) {
        return Err(format!(
            "thread_grep_args_invalid: max_hits must be 1-{MAX_HITS}"
        ));
    }
    let pattern = if args.regex {
        expression(&args.pattern)?
    } else {
        literal(&args.pattern)?
    };
    let start = match args.cursor.as_deref() {
        None => 0,
        Some(cursor) => cursor
            .parse::<usize>()
            .map_err(|_| "thread_cursor_invalid: pass next_cursor unchanged".to_owned())?,
    };
    let row = scope.resolve_argument(&args.thread)?;
    let members = scope.members(&row)?;
    let selected = match args.runs {
        None => members.default_scan(),
        Some(runs) => {
            if runs.is_empty() || runs.len() > MAX_RUNS {
                return Err(format!(
                    "thread_grep_args_invalid: runs must list 1-{MAX_RUNS} run IDs"
                ));
            }
            if let Some(run) = runs.iter().find(|run| !members.is_saved_member(run)) {
                return Err(not_in_thread(run, &row.thread_id));
            }
            runs
        }
    };
    let kinds: Option<HashSet<GrepKind>> = args.kinds.map(|kinds| kinds.into_iter().collect());
    let records = scope
        .store
        .run_contexts_for(&selected)
        .map_err(|_| STORE_ERROR.to_owned())?;
    let mut notes = Vec::new();
    for run in &selected {
        if !records.iter().any(|record| &record.run_id == run) {
            notes.push(format!("{run} has no saved history"));
        }
    }
    // Ranked: conversation text and tool calls first, tool results after them.
    let mut hits = Vec::new();
    let mut result_hits = Vec::new();
    for record in &records {
        let Ok(mut normalized) = normalize(record, &scope.redactor) else {
            notes.push(format!("{} history could not be decoded", record.run_id));
            continue;
        };
        if args.collapse_repeats {
            collapse_repeats(&mut normalized);
        }
        for message in &normalized.messages {
            for block in &message.blocks {
                let class = GrepKind::of(message.role, block.kind);
                if kinds.as_ref().is_some_and(|kinds| !kinds.contains(&class)) {
                    continue;
                }
                let mut matches = pattern.find_iter(&block.text);
                let Some(first) = matches.next() else {
                    continue;
                };
                let reference = Ref {
                    run_id: normalized.run_id,
                    ordinal: message.ordinal,
                    block: block.index,
                };
                let mut hit = json!({
                    "ref": reference.to_string(),
                    "role": role_label(message.role),
                    "kind": kind_label(block.kind),
                    "match_count": 1 + matches.count(),
                    "snippet": snippet(&block.text, first.start(), first.end(), context, MATCH_CHARS),
                });
                if let Some(name) = &block.tool_name {
                    hit["tool_name"] = json!(super::identifier(name));
                }
                if let Some(is_error) = block.is_error {
                    hit["is_error"] = json!(is_error);
                }
                if block.repeat_count > 1 {
                    hit["repeat_count"] = json!(block.repeat_count);
                }
                if block.kind == BlockKind::ToolResult {
                    result_hits.push(hit);
                } else {
                    hits.push(hit);
                }
            }
        }
    }
    hits.extend(result_hits);
    let notes = bounded_notes(notes);
    let total = hits.len();
    if start > total {
        return Err("thread_cursor_invalid: pass next_cursor unchanged".into());
    }
    let page: Vec<Value> = hits.into_iter().skip(start).take(max_hits).collect();
    let shown = page.len();
    fit(MAX_OUTPUT_BYTES, shown, |count| {
        let next = start + count;
        scope.output(json!({
            "note": UNTRUSTED_NOTE,
            "thread_id": row.thread_id,
            "runs_scanned": selected.len(),
            "total_hits": total,
            "hits": &page[..count],
            "next_cursor": (next < total).then(|| next.to_string()),
            "notes": notes,
        }))
    })
}
