//! `thread_search`: find visible threads by metadata (Phase A has no body index).

use std::collections::HashSet;

use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use storage::threads::{ThreadCursor, ThreadRow};

use super::normalize::UNTRUSTED_NOTE;
use super::{
    MAX_OUTPUT_BYTES, Members, STORE_ERROR, Scope, clip_chars, expression, fit, literal, snippet,
};

const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 20;
const MAX_TERMS: usize = 8;
/// Candidate threads examined per call; `next_cursor` continues after them.
const MAX_SCAN: usize = 100;
const SNIPPET_CHARS: usize = 200;
const MAX_SNIPPETS: usize = 3;
const MAX_MATCHED_RUNS: usize = 10;

#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum Query {
    One(String),
    Any(Vec<String>),
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MatchMode {
    #[default]
    Words,
    Phrase,
    Regex,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SearchKind {
    Title,
    User,
    WorkingNote,
    FinalReport,
}

impl SearchKind {
    fn label(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::User => "user",
            Self::WorkingNote => "working_note",
            Self::FinalReport => "final_report",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchArgs {
    query: Option<Query>,
    #[serde(rename = "match", default)]
    mode: MatchMode,
    #[serde(default = "super::yes")]
    include_children: bool,
    kinds: Option<Vec<SearchKind>>,
    updated_after: Option<i64>,
    limit: Option<usize>,
    cursor: Option<String>,
}

/// OR of alternatives; an alternative matches when all of its patterns do.
struct Matcher(Vec<Vec<Regex>>);

impl Matcher {
    fn new(query: Option<Query>, mode: MatchMode) -> Result<Option<Self>, String> {
        let terms = match query {
            None => return Ok(None),
            Some(Query::One(term)) => vec![term],
            Some(Query::Any(terms)) => terms,
        };
        if terms.is_empty() || terms.len() > MAX_TERMS {
            return Err(format!(
                "thread_query_invalid: query must be a string or 1-{MAX_TERMS} strings"
            ));
        }
        let alternatives = terms
            .iter()
            .map(|term| match mode {
                MatchMode::Words => term.split_whitespace().map(literal).collect(),
                MatchMode::Phrase => literal(term).map(|regex| vec![regex]),
                MatchMode::Regex => expression(term).map(|regex| vec![regex]),
            })
            .collect::<Result<Vec<Vec<Regex>>, String>>()?;
        if alternatives.iter().any(Vec::is_empty) {
            return Err("thread_query_invalid: query terms must not be blank".into());
        }
        Ok(Some(Self(alternatives)))
    }

    /// Span of the first pattern of the first satisfied alternative.
    fn find(&self, text: &str) -> Option<(usize, usize)> {
        self.0.iter().find_map(|patterns| {
            let mut spans = patterns.iter().map(|pattern| pattern.find(text));
            let first = spans.next()??;
            spans
                .all(|span| span.is_some())
                .then_some((first.start(), first.end()))
        })
    }
}

/// Searchable metadata: never assistant text, tool calls or tool results.
struct Item {
    kind: SearchKind,
    run_id: Option<String>,
    text: String,
}

pub(super) fn run(scope: &Scope<'_>, args: SearchArgs) -> Result<Value, String> {
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(format!(
            "thread_search_limit_invalid: limit must be 1-{MAX_LIMIT}"
        ));
    }
    let matcher = Matcher::new(args.query, args.mode)?;
    let after = args
        .cursor
        .as_deref()
        .map(|cursor| {
            scope
                .store
                .thread_cursor_from_token(cursor)
                .map_err(|_| STORE_ERROR.to_owned())?
                .ok_or_else(|| "thread_cursor_invalid: pass next_cursor unchanged".to_owned())
        })
        .transpose()?;
    let kinds: Option<HashSet<SearchKind>> = args.kinds.map(|kinds| kinds.into_iter().collect());
    let wanted = |kind: SearchKind| kinds.as_ref().is_none_or(|kinds| kinds.contains(&kind));
    let candidates: Vec<ThreadRow> = candidates(scope, after.as_ref())?
        .into_iter()
        .filter(|row| {
            args.updated_after
                .is_none_or(|after| row.updated_at_ns > after)
        })
        .collect();
    let mut results = Vec::new();
    let mut positions = Vec::new();
    let mut last_examined = None;
    let mut exhausted = true;
    for (examined, candidate) in candidates.iter().enumerate() {
        if results.len() == limit || examined == MAX_SCAN {
            exhausted = false;
            break;
        }
        last_examined = Some(position(scope, candidate)?);
        // Gathered candidates are re-checked by the single visibility gate.
        let Ok(row) = scope.resolve(&candidate.thread_id) else {
            continue;
        };
        let members = scope.members(&row)?;
        let (hit_count, matched_runs, snippets) = match &matcher {
            None => (0, Vec::new(), Vec::new()),
            Some(matcher) => {
                let items = metadata(scope, &row, &members, args.include_children, &wanted)?;
                let hits: Vec<_> = items
                    .iter()
                    .filter_map(|item| matcher.find(&item.text).map(|span| (item, span)))
                    .collect();
                if hits.is_empty() {
                    continue;
                }
                let mut runs: Vec<&str> = Vec::new();
                for (item, _) in &hits {
                    if let Some(run) = item.run_id.as_deref()
                        && !runs.contains(&run)
                    {
                        runs.push(run);
                    }
                }
                let snippets = hits
                    .iter()
                    .take(MAX_SNIPPETS)
                    .map(|(item, (start, end))| {
                        let text = snippet(
                            &item.text,
                            *start,
                            *end,
                            SNIPPET_CHARS / 3,
                            SNIPPET_CHARS / 3,
                        )
                        .replace(['\n', '\r'], " ");
                        json!({
                            "kind": item.kind.label(),
                            "run_id": item.run_id,
                            "text": clip_chars(&text, SNIPPET_CHARS),
                        })
                    })
                    .collect();
                (
                    hits.len(),
                    runs.into_iter()
                        .take(MAX_MATCHED_RUNS)
                        .map(str::to_owned)
                        .collect(),
                    snippets,
                )
            }
        };
        positions.push(position(scope, &row)?);
        results.push(entry(
            scope,
            &row,
            &members,
            hit_count,
            matched_runs,
            snippets,
        ));
    }
    let next = (!exhausted).then(|| last_examined.clone()).flatten();
    let total = results.len();
    fit(MAX_OUTPUT_BYTES, total, |count| {
        // A trimmed page resumes after the last thread it still shows.
        let next_cursor = if count < total {
            count
                .checked_sub(1)
                .and_then(|last| positions.get(last))
                .cloned()
                .or_else(|| args.cursor.clone())
        } else {
            next.clone()
        };
        scope.output(json!({
            "note": UNTRUSTED_NOTE,
            "scope": "own thread, its lineage and threads of this project; metadata only (titles, human messages, working notes, final reports)",
            "threads": &results[..count],
            "next_cursor": next_cursor,
        }))
    })
}

fn position(scope: &Scope<'_>, row: &ThreadRow) -> Result<String, String> {
    scope
        .store
        .thread_cursor_token(&ThreadCursor {
            updated_at_ns: row.updated_at_ns,
            thread_id: row.thread_id.clone(),
        })
        .map_err(|_| STORE_ERROR.to_owned())
}

/// Own thread, its recorded lineage and the project's threads, newest first.
fn candidates(scope: &Scope<'_>, after: Option<&ThreadCursor>) -> Result<Vec<ThreadRow>, String> {
    let store = scope.store;
    let fail = |_| STORE_ERROR.to_owned();
    let mut rows = Vec::new();
    if let Some(own) = &scope.caller.thread_id
        && let Some(row) = store.thread(own).map_err(fail)?
    {
        let mut related = Vec::new();
        related.extend(row.parent_thread_id.clone());
        if let Some(source) = &row.escalation_source_run_id
            && let Some(binding) = store.thread_of_run(source).map_err(fail)?
        {
            related.push(binding.thread_id);
        }
        for id in related {
            rows.extend(store.thread(&id).map_err(fail)?);
        }
        rows.extend(store.thread_children(own).map_err(fail)?);
        rows.push(row);
    }
    if let Some(root) = &scope.caller.project_root {
        // Bound work per invocation, not the total searchable history. One
        // lookahead candidate distinguishes an exhausted scan from a full page.
        let page = store
            .threads_in_project(root, after, MAX_SCAN + 1)
            .map_err(fail)?;
        rows.extend(page.threads);
    }
    let mut seen = HashSet::new();
    rows.retain(|row| {
        seen.insert(row.thread_id.clone())
            && after.is_none_or(|after| {
                (row.updated_at_ns, row.thread_id.as_str())
                    < (after.updated_at_ns, after.thread_id.as_str())
            })
    });
    rows.sort_by(|left, right| {
        (right.updated_at_ns, &right.thread_id).cmp(&(left.updated_at_ns, &left.thread_id))
    });
    rows.truncate(MAX_SCAN + 1);
    Ok(rows)
}

fn metadata(
    scope: &Scope<'_>,
    row: &ThreadRow,
    members: &Members,
    include_children: bool,
    wanted: &dyn Fn(SearchKind) -> bool,
) -> Result<Vec<Item>, String> {
    let fail = |_| STORE_ERROR.to_owned();
    let mut items = Vec::new();
    if wanted(SearchKind::Title)
        && let Some(title) = &row.title
    {
        items.push(Item {
            kind: SearchKind::Title,
            run_id: None,
            text: scope.redact(title),
        });
    }
    let ids: Vec<String> = members
        .runs
        .iter()
        .filter(|run| include_children || run.depth == 0)
        .map(|run| run.run_id.clone())
        .collect();
    if ids.is_empty() {
        return Ok(items);
    }
    if wanted(SearchKind::User) {
        for evidence in scope
            .store
            .review_evidence_for_lineage(&ids)
            .map_err(fail)?
        {
            if matches!(
                evidence.kind.as_str(),
                "initial_prompt" | "host_chat" | "user_answer"
            ) {
                items.push(Item {
                    kind: SearchKind::User,
                    run_id: Some(evidence.target_run_id),
                    text: scope.redact(&evidence.text),
                });
            }
        }
    }
    if wanted(SearchKind::WorkingNote) {
        for note in scope.store.latest_ledgers(&ids).map_err(fail)? {
            if !note.body.is_empty() {
                items.push(Item {
                    kind: SearchKind::WorkingNote,
                    run_id: Some(note.run_id),
                    text: scope.redact(&note.body),
                });
            }
        }
    }
    if wanted(SearchKind::FinalReport) {
        for id in &ids {
            if let Some(summary) = members.summaries.get(id)
                && summary.terminal_phase == "Done"
                && let Some(text) = &summary.final_text
            {
                items.push(Item {
                    kind: SearchKind::FinalReport,
                    run_id: Some(id.clone()),
                    text: scope.redact(text),
                });
            }
        }
    }
    Ok(items)
}

fn entry(
    scope: &Scope<'_>,
    row: &ThreadRow,
    members: &Members,
    hit_count: usize,
    matched_runs: Vec<String>,
    snippets: Vec<Value>,
) -> Value {
    let root = members.primary.as_deref();
    let (phase, _) = root
        .map(|root| scope.status(root, members.summaries.get(root)))
        .unwrap_or_default();
    json!({
        "thread_id": row.thread_id,
        "title": row.title.as_deref().map(|title| clip_chars(&scope.redact(title), SNIPPET_CHARS)),
        "project": {"id": row.project_id, "root": row.project_root},
        "status": {
            "root_run_id": root,
            "phase": phase,
            "archived": row.archived,
            "superseded": row.superseded,
            "run_count": members.runs.len(),
        },
        "lineage": lineage(row),
        "updated_at_ns": row.updated_at_ns,
        "hit_count": hit_count,
        "matched_runs": matched_runs,
        "snippets": snippets,
    })
}

/// How this thread branched, or null for an original conversation.
pub(super) fn lineage(row: &ThreadRow) -> Value {
    if row.parent_thread_id.is_none()
        && row.lineage_kind.is_none()
        && row.escalation_source_run_id.is_none()
        && row.fork_point_run_id.is_none()
    {
        return Value::Null;
    }
    json!({
        "parent_thread_id": row.parent_thread_id,
        "kind": row.lineage_kind.map(|kind| kind.as_str()),
        "fork_point_run_id": row.fork_point_run_id,
        "fork_point_context_len": row.fork_point_context_len,
        "escalation_source_run_id": row.escalation_source_run_id,
    })
}
