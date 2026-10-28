//! Host-side thread search (GUI search modal).
//!
//! The model tool `thread_search` authorizes every thread against the calling
//! run. The host is not a run: it acts for the user on the project they have
//! open, so it searches as a caller whose only visibility grant is that
//! project's canonical root (ADR 0041 rule 2). Body matching, redaction and the
//! bounded scan are the tool's own, so a host query can never see more than a
//! run working in that project could. Only the first page additionally ranks
//! thread titles with a fuzzy (subsequence) match; the model-facing tool keeps
//! its literal meaning.

use std::path::Path;

use secret_guard::SecretRedactor;
use serde_json::{Value, json};

use storage::threads::ThreadRow;

use super::visibility::Caller;
use super::{Scope, search};
use crate::{AgentRuntime, RunId};

/// Project threads whose titles the fuzzy pass reads, newest first.
const FUZZY_MAX_THREADS: usize = 2000;
const FUZZY_PAGE: usize = 100;
/// Continuation point meaning "titles are done, start the body search": returned
/// when the title matches end exactly on a full page.
const BODY_START: &str = "host:body-start";
/// Continuation point inside the ranked title matches: `host:titles:<already returned>`.
const TITLES_PREFIX: &str = "host:titles:";

/// Most results one host search returns (the tool's page limit).
pub const HOST_SEARCH_MAX_LIMIT: usize = 20;

/// One thread that matched a host search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostThreadHit {
    pub thread_id: String,
    pub title: Option<String>,
    /// Where the first matching text was found: `title`, `user`,
    /// `working_note`, `final_report` or `conversation`. `None` for a listing
    /// without a query.
    pub snippet_kind: Option<String>,
    /// Redacted text around the first match, on one line.
    pub snippet: Option<String>,
    pub archived: bool,
    pub updated_at_ns: i64,
}

/// What the conversation index covered for the searched threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostSearchIndex {
    /// Whether body indexing is switched on. `None` when it could not be read.
    pub enabled: Option<bool>,
    /// Runs whose bodies are not searchable yet (pending, evicted or missing).
    pub unindexed: u64,
    pub backfill_in_progress: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostThreadSearch {
    pub hits: Vec<HostThreadHit>,
    /// Pass back as `cursor` to continue after the threads already examined.
    /// `Some` means unexamined threads remain, so an empty `hits` is not
    /// "no match" yet.
    pub next_cursor: Option<String>,
    pub index: HostSearchIndex,
    /// Fuzzy title matching read only the newest threads of a larger project,
    /// so older titles matched exact words only.
    pub title_scan_limited: bool,
}

impl AgentRuntime {
    /// Searches the threads of `project` by title and body. A blank `query`
    /// lists the most recent threads. Titles are matched fuzzily and listed
    /// first, best match first, page by page; bodies match literally, newest
    /// first, once the titles are used up. One call
    /// examines a bounded number of threads: while `next_cursor` is set, call
    /// again with it (same `project` and `query`) to continue. Blocking storage
    /// I/O: call it off the UI thread.
    ///
    /// # Errors
    /// Returns a message when the run store is unavailable, the query is
    /// rejected, or the saved records cannot be read.
    pub fn search_threads_for_host(
        &self,
        project: &Path,
        query: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<HostThreadSearch, String> {
        let Some(store) = self.shared.run_store.get() else {
            return Err("thread search needs the persistent run store".into());
        };
        // A sentinel ID: the host names no run, and a project-root grant needs none.
        let caller = Caller::new(RunId::new(0), None, Some(project));
        if caller.project_root.is_none() {
            return Err("the project folder is not available".into());
        }
        let scope = Scope::new(store, self, caller, SecretRedactor::from_env());
        let limit = limit.clamp(1, HOST_SEARCH_MAX_LIMIT);
        let query = query.trim();
        let enabled = store
            .handle
            .thread_index_job_status()
            .ok()
            .map(|status| status.settings.enabled);
        let mut args = json!({"limit": limit});
        if !query.is_empty() {
            args["query"] = json!(query);
        }
        // Titles are ranked first: their pages come before any body page, and
        // the cursor says where in the ranking the next page starts.
        let title_offset = match cursor {
            None if !query.is_empty() => Some(0),
            Some(cursor) if !query.is_empty() => cursor
                .strip_prefix(TITLES_PREFIX)
                .map(|offset| {
                    offset.parse::<usize>().map_err(|_| {
                        "thread_cursor_invalid: repeat the search without cursor".to_owned()
                    })
                })
                .transpose()?,
            _ => None,
        };
        let mut titles = None;
        if let Some(offset) = title_offset {
            let scan = scan_titles(&scope, query, offset, limit)?;
            let end = offset + scan.page.len();
            if end < scan.total || scan.page.len() >= limit {
                // More titles are left, or they filled the page; bodies follow them.
                let next = if end < scan.total {
                    format!("{TITLES_PREFIX}{end}")
                } else {
                    BODY_START.to_owned()
                };
                return Ok(HostThreadSearch {
                    hits: scan.page.iter().map(|(_, hit)| hit.clone()).collect(),
                    next_cursor: Some(next),
                    index: HostSearchIndex {
                        enabled,
                        ..HostSearchIndex::default()
                    },
                    title_scan_limited: scan.limited,
                });
            }
            args["limit"] = json!(limit - scan.page.len());
            titles = Some(scan);
        } else if let Some(cursor) = cursor.filter(|cursor| *cursor != BODY_START) {
            args["cursor"] = json!(cursor);
        }
        let output = search::run(
            &scope,
            serde_json::from_value(args).map_err(|error| error.to_string())?,
        )?;
        let mut search = parse(&output, enabled);
        if let Some(titles) = titles {
            rank_titles(&mut search, titles, query);
        }
        Ok(search)
    }
}

/// The ranked title matches of a project and one page of them.
struct TitleScan {
    /// This page, best first.
    page: Vec<(u32, HostThreadHit)>,
    /// Thread IDs of every title match, on this page or not.
    all: Vec<String>,
    total: usize,
    /// The project has threads the scan did not read.
    limited: bool,
}

fn scan_titles(
    scope: &Scope<'_>,
    query: &str,
    offset: usize,
    limit: usize,
) -> Result<TitleScan, String> {
    let mut matches: Vec<(u32, HostThreadHit)> = Vec::new();
    let mut limited = false;
    if let Some(root) = scope.caller.project_root.as_deref() {
        let mut cursor = None;
        let mut seen = 0;
        loop {
            let page = scope
                .store
                .threads_in_project(root, cursor.as_ref(), FUZZY_PAGE)
                .map_err(|_| super::STORE_ERROR.to_owned())?;
            seen += page.threads.len();
            for row in &page.threads {
                let title = row.title.as_deref().map(|title| scope.redact(title));
                if let Some(score) = title.as_deref().and_then(|title| title_score(title, query)) {
                    matches.push((score, title_hit(row, title)));
                }
            }
            match page.next_cursor {
                Some(next) if seen < FUZZY_MAX_THREADS => cursor = Some(next),
                Some(_) => {
                    limited = true;
                    break;
                }
                None => break,
            }
        }
    }
    sort_ranked(&mut matches);
    let all = matches
        .iter()
        .map(|(_, hit)| hit.thread_id.clone())
        .collect();
    let total = matches.len();
    let page = matches.into_iter().skip(offset).take(limit).collect();
    Ok(TitleScan {
        page,
        all,
        total,
        limited,
    })
}

/// Best score first, newest first among equals, then by ID so the order, and
/// with it a cursor into it, is the same on every call.
fn sort_ranked(hits: &mut [(u32, HostThreadHit)]) {
    hits.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| right.updated_at_ns.cmp(&left.updated_at_ns))
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
}

/// Puts every thread whose title matches ahead of body-only hits, best title
/// first. The result never exceeds the page the literal search was sized for,
/// and a thread already listed on another title page is not listed again.
fn rank_titles(search: &mut HostThreadSearch, titles: TitleScan, query: &str) {
    search.title_scan_limited = titles.limited;
    let mut ranked = titles.page;
    let mut body_only = Vec::new();
    for hit in std::mem::take(&mut search.hits) {
        // The literal hit knows where the text matched, so it replaces the bare title hit.
        if let Some(entry) = ranked
            .iter_mut()
            .find(|(_, seen)| seen.thread_id == hit.thread_id)
        {
            entry.1 = hit;
            continue;
        }
        if titles.all.contains(&hit.thread_id) {
            continue;
        }
        match hit
            .title
            .as_deref()
            .and_then(|title| title_score(title, query))
        {
            Some(score) => ranked.push((score, hit)),
            None => body_only.push(hit),
        }
    }
    sort_ranked(&mut ranked);
    search.hits = ranked
        .into_iter()
        .map(|(_, hit)| hit)
        .chain(body_only)
        .collect();
}

fn title_hit(row: &ThreadRow, title: Option<String>) -> HostThreadHit {
    HostThreadHit {
        thread_id: row.thread_id.clone(),
        title,
        snippet_kind: Some("title".into()),
        snippet: None,
        archived: row.archived,
        updated_at_ns: row.updated_at_ns,
    }
}

/// Fuzzy score of `title` for `query`, or `None` when some word of the query
/// is not even a subsequence of the title. Case-insensitive. A literal
/// occurrence (best at the start, then at a word start) always outranks a
/// scattered subsequence, which scores higher the fewer characters it skips.
fn title_score(title: &str, query: &str) -> Option<u32> {
    let title = title.to_lowercase();
    let mut total = 0;
    let mut words = 0;
    for word in query.split_whitespace() {
        total += word_score(&title, &word.to_lowercase())?;
        words += 1;
    }
    (words > 0).then_some(total)
}

fn word_score(title: &str, word: &str) -> Option<u32> {
    if let Some(at) = title.find(word) {
        let word_start = title[..at]
            .chars()
            .next_back()
            .is_none_or(|before| !before.is_alphanumeric());
        return Some(match (at, word_start) {
            (0, _) => 1000,
            (_, true) => 800,
            _ => 600,
        });
    }
    let mut wanted = word.chars().peekable();
    let (mut skipped, mut last) = (0u32, None::<usize>);
    for (index, found) in title.chars().enumerate() {
        if wanted.peek() == Some(&found) {
            if let Some(last) = last {
                skipped =
                    skipped.saturating_add(u32::try_from(index - last - 1).unwrap_or(u32::MAX));
            }
            last = Some(index);
            wanted.next();
        }
    }
    wanted.peek().is_none().then(|| 400 - skipped.min(300))
}

fn parse(output: &Value, enabled: Option<bool>) -> HostThreadSearch {
    let text = |value: &Value| value.as_str().map(str::to_owned);
    let hits = output["threads"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|thread| {
            let first = thread["snippets"].get(0);
            Some(HostThreadHit {
                thread_id: text(&thread["thread_id"])?,
                title: text(&thread["title"]),
                snippet_kind: first.and_then(|snippet| text(&snippet["kind"])),
                snippet: first.and_then(|snippet| text(&snippet["text"])),
                archived: thread["status"]["archived"].as_bool().unwrap_or(false),
                updated_at_ns: thread["updated_at_ns"].as_i64().unwrap_or_default(),
            })
        })
        .collect();
    let index = &output["index"];
    HostThreadSearch {
        hits,
        next_cursor: output["next_cursor"].as_str().map(str::to_owned),
        index: HostSearchIndex {
            enabled,
            unindexed: index["unindexed"].as_u64().unwrap_or_default(),
            backfill_in_progress: index["backfill_in_progress"].as_bool().unwrap_or(false),
        },
        title_scan_limited: false,
    }
}
