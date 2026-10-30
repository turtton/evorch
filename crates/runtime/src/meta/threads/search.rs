//! `thread_search`: find visible threads by metadata and, with Phase B, by
//! the persisted conversation index (ADR 0041 M5).
//!
//! Visibility is settled before any conversation record is read: only runs of
//! authorized threads are passed to storage, in bounded batches. Index text is
//! re-redacted and verified by the same matcher as metadata, so SQL routing
//! never decides a hit. Work per call is bounded; an interrupted thread is
//! reported as `partial` and its continuation is carried by `next_cursor`.

use std::collections::{BTreeSet, HashSet};
use std::ops::Range;

use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use storage::thread_index::{
    GUARDED_REFERENCE_VERSION, GuardedReference, GuardedSource, IndexBlockKind, IndexRole,
};
use storage::thread_search::{
    ConversationCandidate, IndexCoverageSummary, MAX_SEARCH_BATCH_RUNS, RunCoverage,
};
use storage::threads::{ThreadCursor, ThreadRow};

use super::conversation::{ScanStats, Stream, guarded, guarded_text, scan_runs};
use super::cursor::{self, CursorError, Position, Resume};
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
/// Conversation blocks examined per call, whether or not they match. Counted
/// over visible runs only, so `partial` and the cursor never depend on runs
/// the caller cannot see.
pub(super) const MAX_SCANNED_BLOCKS: usize = 2000;
/// Index text bytes examined per call.
pub(super) const MAX_SCANNED_BYTES: usize = 4 * 1024 * 1024;

/// How much conversation one call may examine.
#[derive(Debug, Clone, Copy)]
pub(super) struct SearchLimits {
    pub(super) max_blocks: usize,
    pub(super) max_bytes: usize,
}

impl SearchLimits {
    const PRODUCTION: Self = Self {
        max_blocks: MAX_SCANNED_BLOCKS,
        max_bytes: MAX_SCANNED_BYTES,
    };
}

const CURSOR_INVALID: &str = "thread_cursor_invalid: pass next_cursor unchanged; cursors expire with the process and older formats are not accepted, so repeat the search without cursor";
const CURSOR_MISMATCH: &str = "thread_cursor_mismatch: the cursor belongs to a search with a different query, match, kinds, include_children or updated_after; pass those unchanged or repeat the search without cursor";
const CURSOR_STALE: &str = "thread_cursor_stale: the conversation this cursor continues was re-indexed or changed; repeat the search without cursor";

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

impl MatchMode {
    fn label(self) -> &'static str {
        match self {
            Self::Words => "words",
            Self::Phrase => "phrase",
            Self::Regex => "regex",
        }
    }
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SearchKind {
    Title,
    User,
    WorkingNote,
    FinalReport,
    Conversation,
    ToolUse,
    ToolResult,
}

/// B-1: metadata and user/assistant text; tool calls and results only on request.
const DEFAULT_KINDS: [SearchKind; 5] = [
    SearchKind::Title,
    SearchKind::User,
    SearchKind::WorkingNote,
    SearchKind::FinalReport,
    SearchKind::Conversation,
];

impl SearchKind {
    fn label(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::User => "user",
            Self::WorkingNote => "working_note",
            Self::FinalReport => "final_report",
            Self::Conversation => "conversation",
            Self::ToolUse => "tool_use",
            Self::ToolResult => "tool_result",
        }
    }

    /// Index block kinds behind a public kind; `tool_result` includes errors.
    fn index_kinds(self) -> &'static [IndexBlockKind] {
        match self {
            Self::Conversation => &[IndexBlockKind::Text],
            Self::ToolUse => &[IndexBlockKind::ToolUse],
            Self::ToolResult => &[IndexBlockKind::ToolResult, IndexBlockKind::ToolError],
            Self::Title | Self::User | Self::WorkingNote | Self::FinalReport => &[],
        }
    }

    fn of_block(kind: IndexBlockKind) -> Self {
        match kind {
            IndexBlockKind::ToolUse => Self::ToolUse,
            IndexBlockKind::ToolResult | IndexBlockKind::ToolError => Self::ToolResult,
            IndexBlockKind::Text | IndexBlockKind::WorkingNote => Self::Conversation,
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
    #[serde(default, deserialize_with = "super::empty_as_none")]
    cursor: Option<String>,
}

/// OR of alternatives; an alternative matches when all of its patterns do.
pub(super) struct Matcher {
    alternatives: Vec<Vec<Regex>>,
    /// Normalized terms, the same shape as `alternatives`.
    terms: Vec<Vec<String>>,
}

impl Matcher {
    pub(super) fn new(query: Option<Query>, mode: MatchMode) -> Result<Option<Self>, String> {
        let raw = match query {
            None => return Ok(None),
            Some(Query::One(term)) => vec![term],
            Some(Query::Any(terms)) => terms,
        };
        if raw.is_empty() || raw.len() > MAX_TERMS {
            return Err(format!(
                "thread_query_invalid: query must be a string or 1-{MAX_TERMS} strings"
            ));
        }
        let terms: Vec<Vec<String>> = raw
            .iter()
            .map(|term| match mode {
                MatchMode::Words => term.split_whitespace().map(str::to_owned).collect(),
                MatchMode::Phrase | MatchMode::Regex => vec![term.clone()],
            })
            .collect();
        let alternatives = raw
            .iter()
            .zip(&terms)
            .map(|(term, words)| match mode {
                MatchMode::Words => words.iter().map(|word| literal(word)).collect(),
                MatchMode::Phrase => literal(term).map(|regex| vec![regex]),
                MatchMode::Regex => expression(term).map(|regex| vec![regex]),
            })
            .collect::<Result<Vec<Vec<Regex>>, String>>()?;
        if alternatives.iter().any(Vec::is_empty) {
            return Err("thread_query_invalid: query terms must not be blank".into());
        }
        Ok(Some(Self {
            alternatives,
            terms,
        }))
    }

    /// Span of the first pattern of the first satisfied alternative.
    pub(super) fn find(&self, text: &str) -> Option<(usize, usize)> {
        self.alternatives.iter().find_map(|patterns| {
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
    note_seq: Option<u64>,
}

/// A matched position, metadata or conversation.
struct Hit {
    kind: SearchKind,
    run_id: Option<String>,
    text: String,
    span: (usize, usize),
    source: HitSource,
}

enum HitSource {
    Metadata { note_seq: Option<u64> },
    Conversation(Box<ConversationCandidate>),
}

/// A thread examined by this call, in scan order.
struct Examined {
    /// Keyset position of the thread: the cursor's own token for a resumed thread.
    token: String,
    /// The thread and its runs, if it passed visibility.
    visible: Option<(ThreadRow, Members)>,
    resumed: bool,
    slots: Range<usize>,
    revision: String,
}

/// Blocks and bytes examined so far. A block is charged once it has been
/// examined, matching or not, so a query that matches nothing is bounded too.
struct Budget {
    limits: SearchLimits,
    blocks: usize,
    bytes: usize,
}

impl Budget {
    const fn new(limits: SearchLimits) -> Self {
        Self {
            limits,
            blocks: 0,
            bytes: 0,
        }
    }

    const fn exhausted(&self) -> bool {
        self.blocks >= self.limits.max_blocks || self.bytes >= self.limits.max_bytes
    }

    const fn remaining_blocks(&self) -> usize {
        self.limits.max_blocks.saturating_sub(self.blocks)
    }

    const fn remaining_bytes(&self) -> usize {
        self.limits.max_bytes.saturating_sub(self.bytes)
    }

    const fn charge(&mut self, candidate: &ConversationCandidate) {
        self.blocks += 1;
        self.bytes += candidate.text.len();
    }
}

pub(super) fn run(scope: &Scope<'_>, args: SearchArgs) -> Result<Value, String> {
    run_with_limits(scope, args, SearchLimits::PRODUCTION).map(|(output, _)| output)
}

/// [`run`] with explicit work limits, also reporting what the scan fetched.
pub(super) fn run_with_limits(
    scope: &Scope<'_>,
    args: SearchArgs,
    limits: SearchLimits,
) -> Result<(Value, ScanStats), String> {
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(format!(
            "thread_search_limit_invalid: limit must be 1-{MAX_LIMIT}"
        ));
    }
    let kinds: BTreeSet<SearchKind> = args.kinds.map_or_else(
        || DEFAULT_KINDS.into_iter().collect(),
        |kinds| kinds.into_iter().collect(),
    );
    let matcher = Matcher::new(args.query, args.mode)?;
    let index_kinds: Vec<IndexBlockKind> = kinds
        .iter()
        .flat_map(|kind| kind.index_kinds())
        .copied()
        .collect();
    let conversation = matcher.as_ref().filter(|_| !index_kinds.is_empty());
    let caller = caller_key(scope);
    let conditions = conditions_key(
        matcher.as_ref(),
        args.mode,
        &kinds,
        args.include_children,
        args.updated_after,
    );
    let position = args
        .cursor
        .as_deref()
        .map(|token| decode(scope, token, &caller, &conditions))
        .transpose()?;
    let after = position.as_ref().map(|(cursor, _, _)| cursor);
    let found: Vec<ThreadRow> = candidates(scope, after.map(|(thread, _)| thread))?
        .into_iter()
        .filter(|row| {
            args.updated_after
                .is_none_or(|after| row.updated_at_ns > after)
        })
        .collect();
    let more_threads = found.len() > MAX_SCAN;

    // Settle visibility of every thread this call may examine before reading
    // any conversation record.
    let mut examined = Vec::new();
    let mut slots: Vec<String> = Vec::new();
    let mut resume_block = None;
    if let Some(((_, token), resumed, resume)) = &position
        && let Some(resume) = resume
    {
        if conversation.is_none() {
            return Err(CURSOR_INVALID.into());
        }
        let members = scope.search_members(resumed)?;
        let runs = scan_runs(&members, args.include_children);
        if !runs.contains(&resume.run) {
            return Err(CURSOR_STALE.into());
        }
        if revisions_key(&runs, &coverage(scope, &runs)?) != resume.thread_revision {
            return Err(CURSOR_STALE.into());
        }
        let start = slots.len();
        slots.extend(runs.into_iter().filter(|run| *run >= resume.run));
        resume_block = Some(resume.from_block);
        examined.push(Examined {
            token: token.clone(),
            visible: Some((resumed.clone(), members)),
            resumed: true,
            slots: start..slots.len(),
            revision: resume.thread_revision.clone(),
        });
    }
    for candidate in found.iter().take(MAX_SCAN) {
        let visible = match scope.resolve(&candidate.thread_id) {
            // Gathered candidates are re-checked by the single visibility gate.
            Ok(row) => Some({
                let members = scope.search_members(&row)?;
                (row, members)
            }),
            Err(_) => None,
        };
        let start = slots.len();
        if conversation.is_some()
            && let Some((_, members)) = &visible
        {
            slots.extend(scan_runs(members, args.include_children));
        }
        examined.push(Examined {
            token: thread_token(scope, candidate)?,
            visible,
            resumed: false,
            slots: start..slots.len(),
            revision: String::new(),
        });
    }
    let coverage = coverage(scope, &slots)?;
    for item in examined.iter_mut().filter(|item| !item.resumed) {
        item.revision = revisions_key(&slots[item.slots.clone()], &coverage[item.slots.clone()]);
    }
    if let (Some((_, _, Some(resume))), Some(first)) = (&position, coverage.first())
        && *first != (RunCoverage::Indexed, Some(resume.revision))
    {
        return Err(CURSOR_STALE.into());
    }
    let mut stream = conversation.map(|matcher| {
        (
            matcher,
            Stream::new(scope, slots, index_kinds.clone(), resume_block),
        )
    });

    let mut budget = Budget::new(limits);
    let mut summary = IndexCoverageSummary::default();
    let mut results = Vec::new();
    let mut positions = Vec::new();
    let mut last_examined = None;
    let mut next = None;
    let mut stopped = false;
    let mut cut_any = false;
    for item in &examined {
        if results.len() == limit {
            stopped = true;
            break;
        }
        last_examined = Some(Position {
            thread: item.token.clone(),
            resume: None,
        });
        let Some((row, members)) = &item.visible else {
            continue;
        };
        item.slots
            .clone()
            .for_each(|slot| summary.add(coverage[slot].0));
        let mut hits = Vec::new();
        if let Some(matcher) = &matcher
            && !item.resumed
        {
            for meta in metadata(scope, row, members, args.include_children, &kinds)? {
                if let Some(span) = matcher.find(&meta.text) {
                    if meta.kind == SearchKind::FinalReport
                        && kinds.contains(&SearchKind::Conversation)
                        && indexed_final_report(scope, &meta, matcher)?
                    {
                        continue;
                    }
                    hits.push(Hit {
                        kind: meta.kind,
                        run_id: meta.run_id,
                        text: meta.text,
                        span,
                        source: HitSource::Metadata {
                            note_seq: meta.note_seq,
                        },
                    });
                }
            }
        }
        let mut cut = None;
        if let Some((matcher, stream)) = &mut stream {
            while let Some((slot, candidate)) =
                stream.peek(budget.remaining_blocks(), budget.remaining_bytes())?
            {
                if !item.slots.contains(slot) {
                    break;
                }
                if budget.exhausted() {
                    cut = Some(Resume {
                        run: candidate.run_id.clone(),
                        from_block: candidate.position.block_id,
                        revision: candidate.source_revision,
                        thread_revision: item.revision.clone(),
                    });
                    break;
                }
                let (_, candidate) = stream.pop().expect("peeked");
                budget.charge(&candidate);
                // Index text was redacted when extracted; redact again with
                // this caller's redactor so a pattern cannot probe a secret.
                let text = scope.redact(&candidate.text);
                if let Some(span) = matcher.find(&text) {
                    hits.push(Hit {
                        kind: SearchKind::of_block(candidate.kind),
                        run_id: Some(candidate.run_id.clone()),
                        text,
                        span,
                        source: HitSource::Conversation(Box::new(candidate)),
                    });
                }
            }
        }
        let partial = cut.is_some();
        if matcher.is_none() || !hits.is_empty() {
            results.push(entry(scope, row, members, &hits, partial)?);
            positions.push(Position {
                thread: item.token.clone(),
                resume: None,
            });
        }
        if let Some(cut) = cut {
            next = Some(Position {
                thread: item.token.clone(),
                resume: Some(cut),
            });
            cut_any = true;
            break;
        }
    }
    if !cut_any && (stopped || more_threads) {
        next = last_examined.clone();
    }
    let total = results.len();
    let index = stream.as_ref().map(|_| {
        json!({
            "indexed": summary.indexed,
            "pending": summary.pending,
            "evicted": summary.evicted,
            "missing": summary.missing,
            "unindexed": summary.unindexed(),
            "backfill_in_progress": summary.backfill_in_progress(),
        })
    });
    let stats = stream
        .as_ref()
        .map(|(_, stream)| stream.stats())
        .unwrap_or_default();
    let output = fit(MAX_OUTPUT_BYTES, total, |count| {
        // A trimmed page resumes after the last thread it still shows.
        let next_cursor = if count < total {
            count
                .checked_sub(1)
                .and_then(|last| positions.get(last))
                .map(|position| cursor::encode(&caller, &conditions, position))
                .or_else(|| args.cursor.clone())
        } else {
            next.as_ref()
                .map(|position| cursor::encode(&caller, &conditions, position))
        };
        let mut output = json!({
            "note": UNTRUSTED_NOTE,
            "scope": "own thread, its lineage and threads of this project; metadata (titles, human messages, working notes, final reports) and indexed conversation text (tool calls and results only when requested)",
            "threads": &results[..count],
            "partial": cut_any && count == total,
            "next_cursor": next_cursor,
        });
        if let Some(index) = &index {
            output["index"] = index.clone();
        }
        scope.output(output)
    })?;
    Ok((output, stats))
}

/// Decide ownership before paging, not from this page's hits. Only a verified
/// native text at the report's canonical position can replace metadata; equal
/// text elsewhere, copies and unavailable indexes cannot suppress the fallback.
/// Coverage also enforces the binary's extractor version, even before an owner
/// has reached the run in its deferred version-adjustment steps.
fn indexed_final_report(scope: &Scope<'_>, item: &Item, matcher: &Matcher) -> Result<bool, String> {
    let Some(run) = &item.run_id else {
        return Ok(false);
    };
    let current = coverage(scope, std::slice::from_ref(run))?;
    let Some((RunCoverage::Indexed, Some(revision))) = current.first() else {
        return Ok(false);
    };
    let bundle = scope
        .store
        .with_database(|db| Ok(db.snapshot_bundle(run)))
        .map_err(|_| STORE_ERROR.to_owned())?
        .map_err(|_| STORE_ERROR.to_owned())?;
    let Some(bundle) = bundle.filter(|bundle| bundle.source_revision == *revision) else {
        return Ok(false);
    };
    let Ok(input) = storage::thread_index::extract_snapshot_bundle(&bundle, &scope.redactor) else {
        return Ok(false);
    };
    let Some(last) = bundle
        .provenance
        .as_ref()
        .and_then(|p| p.map.messages.last())
    else {
        return Ok(false);
    };
    Ok(input.blocks.iter().any(|block| {
        block.kind == IndexBlockKind::Text && block.role == IndexRole::Assistant && matches!(
            block.locator,
            storage::thread_index::BlockLocator::Message { ordinal, .. } if ordinal == last.ordinal
        ) && matcher.find(&scope.redact(&block.text)).is_some()
    }))
}

/// Bind metadata/conversation ownership to every authorized run, including
/// later runs not yet visited by the bounded stream. A digest keeps cursor
/// size constant even for threads with thousands of roots or delegated runs.
/// `Indexed -> Pending` on an obsolete extractor invalidates this key even
/// when its source revision has not changed.
fn revisions_key(runs: &[String], coverage: &[(RunCoverage, Option<u64>)]) -> String {
    let parts: Vec<String> = runs
        .iter()
        .zip(coverage)
        .map(|(run, (state, revision))| format!("{run}:{state:?}:{revision:?}"))
        .collect();
    cursor::digest(&parts.iter().map(String::as_str).collect::<Vec<_>>())
}

type DecodedPosition = ((ThreadCursor, String), ThreadRow, Option<Resume>);

fn decode(
    scope: &Scope<'_>,
    token: &str,
    caller: &str,
    conditions: &str,
) -> Result<DecodedPosition, String> {
    let position = cursor::decode(token, caller, conditions).map_err(|error| match error {
        CursorError::Invalid => CURSOR_INVALID.to_owned(),
        CursorError::Mismatch => CURSOR_MISMATCH.to_owned(),
    })?;
    let thread = scope
        .store
        .thread_cursor_from_token(&position.thread)
        .map_err(|_| CURSOR_INVALID.to_owned())?
        .ok_or_else(|| CURSOR_INVALID.to_owned())?;
    // A position is untrusted input, not evidence of access. Missing and
    // invisible positions must fail identically before scanning begins.
    let row = scope
        .resolve(&thread.thread_id)
        .map_err(|_| CURSOR_INVALID.to_owned())?;
    Ok(((thread, position.thread), row, position.resume))
}

/// Bind to the exact invoking run as well as its thread/project scope.
pub(super) fn caller_key(scope: &Scope<'_>) -> String {
    let caller = &scope.caller;
    let project = caller.project_root.as_deref().unwrap_or("");
    cursor::digest(&[
        &caller.run_id.to_string(),
        caller.thread_id.as_deref().unwrap_or(""),
        project,
    ])
}

/// Every argument that changes which hits exist or their order.
pub(super) fn conditions_key(
    matcher: Option<&Matcher>,
    mode: MatchMode,
    kinds: &BTreeSet<SearchKind>,
    include_children: bool,
    updated_after: Option<i64>,
) -> String {
    let conditions = json!({
        "match": mode.label(),
        "terms": matcher.map(|matcher| &matcher.terms),
        "kinds": kinds.iter().map(|kind| kind.label()).collect::<Vec<_>>(),
        "include_children": include_children,
        "updated_after": updated_after,
    });
    cursor::digest(&[&conditions.to_string()])
}

/// Coverage of every scanned run, in order, by batch.
fn coverage(scope: &Scope<'_>, runs: &[String]) -> Result<Vec<(RunCoverage, Option<u64>)>, String> {
    let mut coverage = Vec::with_capacity(runs.len());
    for batch in runs.chunks(MAX_SEARCH_BATCH_RUNS) {
        let rows = scope
            .store
            .with_database(|db| db.thread_index_coverage(batch))
            .map_err(|_| STORE_ERROR.to_owned())?;
        coverage.extend(
            rows.into_iter()
                .map(|row| (row.coverage, row.source_revision)),
        );
    }
    Ok(coverage)
}

fn thread_token(scope: &Scope<'_>, row: &ThreadRow) -> Result<String, String> {
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
    // Related candidates can include fork children, which are not necessarily
    // visible. Never issue a continuation position the caller cannot resolve.
    rows.retain(|row| scope.resolve(&row.thread_id).is_ok());
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
    kinds: &BTreeSet<SearchKind>,
) -> Result<Vec<Item>, String> {
    let fail = |_| STORE_ERROR.to_owned();
    let wanted = |kind| kinds.contains(&kind);
    let mut items = Vec::new();
    if wanted(SearchKind::Title)
        && let Some(title) = &row.title
    {
        items.push(Item {
            kind: SearchKind::Title,
            run_id: None,
            text: scope.redact(title),
            note_seq: None,
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
                    note_seq: None,
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
                    note_seq: Some(note.seq),
                });
            }
        }
    }
    if wanted(SearchKind::FinalReport) {
        // Keep all requested runs and the entire final report, but do not parse
        // histories merely to select a root or for threads beyond this page's
        // result limit. These summaries are local to this metadata examination.
        let done: Vec<String> = ids
            .iter()
            .filter(|id| {
                members
                    .headers
                    .get(*id)
                    .is_some_and(|header| header.terminal_phase == "Done")
            })
            .cloned()
            .collect();
        for summary in scope.store.terminal_summaries(&done).map_err(fail)? {
            if summary.terminal_phase == "Done"
                && let Some(text) = &summary.final_text
            {
                items.push(Item {
                    kind: SearchKind::FinalReport,
                    run_id: Some(summary.run_id),
                    text: scope.redact(text),
                    note_seq: None,
                });
            }
        }
    }
    Ok(items)
}

/// Up to [`MAX_SNIPPETS`], metadata first but keeping one place for
/// conversation when it matched, since only conversation hits carry a ref.
fn snippets(scope: &Scope<'_>, hits: &[Hit]) -> Result<Vec<Value>, String> {
    let conversation = hits
        .iter()
        .filter(|hit| matches!(hit.source, HitSource::Conversation(_)));
    let metadata = hits
        .iter()
        .filter(|hit| matches!(hit.source, HitSource::Metadata { .. }));
    let reserved = usize::from(conversation.clone().next().is_some());
    let chosen: Vec<&Hit> = metadata
        .take(MAX_SNIPPETS - reserved)
        .chain(conversation)
        .take(MAX_SNIPPETS)
        .collect();
    let note_runs: Vec<String> = chosen
        .iter()
        .filter_map(|hit| match hit.source {
            HitSource::Metadata { note_seq: Some(_) } => hit.run_id.clone(),
            _ => None,
        })
        .collect();
    let revisions = if note_runs.is_empty() {
        Vec::new()
    } else {
        scope
            .store
            .with_database(|db| db.thread_index_coverage(&note_runs))
            .map_err(|_| STORE_ERROR.to_owned())?
    };
    Ok(chosen
        .into_iter()
        .map(|hit| {
            let (start, end) = hit.span;
            let text = snippet(&hit.text, start, end, SNIPPET_CHARS / 3, SNIPPET_CHARS / 3)
                .replace(['\n', '\r'], " ");
            let mut value = json!({
                "kind": hit.kind.label(),
                "run_id": hit.run_id,
                "text": clip_chars(&text, SNIPPET_CHARS),
            });
            match &hit.source {
                HitSource::Metadata {
                    note_seq: Some(note_seq),
                } => {
                    let revision = revisions
                        .iter()
                        .find(|row| Some(&row.run_id) == hit.run_id.as_ref())
                        .and_then(|row| row.source_revision);
                    if let (Some(run_id), Some(source_revision)) = (&hit.run_id, revision) {
                        value["ref"] = json!(guarded_text(&GuardedReference {
                            version: GUARDED_REFERENCE_VERSION,
                            run_id: run_id.clone(),
                            source_revision,
                            source: GuardedSource::WorkingNote {
                                note_seq: *note_seq
                            },
                        }));
                    }
                }
                HitSource::Metadata { note_seq: None } => {}
                HitSource::Conversation(candidate) => {
                    value["ref"] = json!(guarded_text(&guarded(candidate)));
                    value["role"] = json!(match candidate.role {
                        IndexRole::User => "user",
                        IndexRole::Assistant => "assistant",
                        IndexRole::WorkingNote => "working_note",
                    });
                    if let Some(tool) = &candidate.tool_name {
                        value["tool_name"] = json!(clip_chars(tool, 80));
                    }
                    if hit.kind == SearchKind::ToolResult {
                        value["is_error"] = json!(candidate.kind == IndexBlockKind::ToolError);
                    }
                    value["truncated"] = json!(candidate.truncated);
                    value["source_revision"] = json!(candidate.source_revision);
                }
            }
            value
        })
        .collect())
}

fn entry(
    scope: &Scope<'_>,
    row: &ThreadRow,
    members: &Members,
    hits: &[Hit],
    partial: bool,
) -> Result<Value, String> {
    let root = members.primary.as_deref();
    let (phase, _) = root
        .map(|root| {
            scope.status(
                root,
                members
                    .headers
                    .get(root)
                    .map(|saved| saved.terminal_phase.as_str()),
            )
        })
        .unwrap_or_default();
    let mut runs: Vec<&str> = Vec::new();
    for hit in hits {
        if let Some(run) = hit.run_id.as_deref()
            && !runs.contains(&run)
        {
            runs.push(run);
        }
    }
    runs.truncate(MAX_MATCHED_RUNS);
    let mut value = json!({
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
        "hit_count": hits.len(),
        "matched_runs": runs,
        "snippets": snippets(scope, hits)?,
    });
    if partial {
        value["partial"] = json!(true);
    }
    Ok(value)
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
