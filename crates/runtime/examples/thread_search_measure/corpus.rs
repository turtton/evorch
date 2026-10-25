use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use secret_guard::SecretRedactor;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use storage::Database;
use storage::thread_index::{IndexBlockKind, IndexState, extract_snapshot_bundle};
use storage::threads::{ThreadCursor, ThreadRow};

use super::{PAGE_LIMIT, Result, require};

pub type HitSet = BTreeMap<String, u64>;

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn set_hash(set: &HitSet) -> String {
    hash(&serde_json::to_vec(set).expect("string map"))
}

pub struct Case {
    pub class: &'static str,
    query: String,
    mode: &'static str,
    tool: bool,
}

impl Case {
    #[cfg(test)]
    pub fn fixture(query: &str, mode: &'static str) -> Self {
        Self {
            class: "fixture",
            query: query.into(),
            mode,
            tool: false,
        }
    }
    pub fn args(&self) -> Value {
        let kinds = if self.tool {
            vec![
                "title",
                "user",
                "working_note",
                "conversation",
                "tool_use",
                "tool_result",
            ]
        } else {
            vec!["title", "user", "working_note", "conversation"]
        };
        json!({
            "query": self.query, "match": self.mode, "kinds": kinds,
            "include_children": true, "limit": PAGE_LIMIT,
        })
    }

    pub fn hash(&self) -> String {
        hash(&serde_json::to_vec(&self.args()).expect("query args"))
    }

    fn matches(&self, text: &str) -> Result<bool> {
        let expressions = if self.mode == "words" {
            self.query
                .split_whitespace()
                .map(regex::escape)
                .collect::<Vec<_>>()
        } else if self.mode == "regex" {
            vec![self.query.clone()]
        } else {
            vec![regex::escape(&self.query)]
        };
        for expression in expressions {
            if !Regex::new(&format!("(?i){expression}"))?.is_match(text) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

struct Item {
    text: String,
    tool: bool,
    conversation: bool,
}

pub struct Corpus {
    pub threads: Vec<ThreadRow>,
    pub visible_runs: BTreeSet<String>,
    pub source_runs: BTreeSet<String>,
    pub other_projects: Vec<String>,
    pub other_indexable: usize,
    pub classification: Value,
    items: BTreeMap<String, Vec<Item>>,
    indexable: BTreeSet<String>,
}

fn read_only(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

impl Corpus {
    pub fn load(db: &Database, path: &Path, project: &Path) -> Result<Self> {
        let root = project.to_str().ok_or("non_utf8_project")?;
        let mut threads = Vec::new();
        let mut cursor: Option<ThreadCursor> = None;
        loop {
            let page = db.threads_in_project(root, cursor.as_ref(), 100)?;
            threads.extend(page.threads);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        require(!threads.is_empty(), "empty_visible_project")?;
        let redactor = SecretRedactor::from_env();
        let conn = read_only(path)?;
        let ids: Vec<String> = conn.prepare(
            "SELECT run_id FROM run_contexts UNION SELECT run_id FROM run_ledger ORDER BY run_id"
        )?.query_map([], |row| row.get(0))?.collect::<std::result::Result<_, _>>()?;
        let mut indexable = BTreeSet::new();
        let mut extracted = BTreeMap::new();
        let mut pending = 0;
        let mut oversized = 0;
        for run in &ids {
            let input = db
                .snapshot_bundle(run)
                .map_err(|_| "canonical_snapshot_read_failed")?
                .and_then(|bundle| extract_snapshot_bundle(&bundle, &redactor).ok());
            match input {
                Some(input) if input.text_bytes <= 512 << 10 && input.blocks.len() <= 2_048 => {
                    indexable.insert(run.clone());
                    extracted.insert(run.clone(), input);
                }
                Some(_) => oversized += 1,
                None => pending += 1,
            }
        }
        let classification = json!({
            "total": ids.len(), "indexable": indexable.len(),
            "pending_or_unprovable": pending, "oversized_step": oversized,
            "extractor_version": storage::thread_index::EXTRACTOR_VERSION,
            "accounting_version": storage::thread_index::ACCOUNTING_VERSION,
        });
        println!(
            "{}",
            json!({"stage": "classification", "counts": classification})
        );
        require(
            ids.len() == 1857 && indexable.len() == 690 && pending == 1154 && oversized == 13,
            "strict_classification_mismatch",
        )?;

        let mut visible_runs = BTreeSet::new();
        let mut items = BTreeMap::new();
        for thread in &threads {
            let runs = db.runs_of_thread(&thread.thread_id)?;
            let ids = runs
                .iter()
                .map(|run| run.run_id.clone())
                .collect::<Vec<_>>();
            visible_runs.extend(ids.iter().cloned());
            let summaries = db.terminal_summaries(&ids)?;
            let primary = runs
                .iter()
                .filter(|run| run.depth == 0)
                .filter_map(|run| summaries.iter().find(|s| s.run_id == run.run_id))
                .max_by_key(|s| (s.updated_at_ns, &s.run_id))
                .map(|s| s.run_id.as_str())
                .or(thread.root_run_id.as_deref());
            let mut thread_items = Vec::new();
            let mut add = |text: &str, tool, conversation| {
                thread_items.push(Item {
                    text: redactor.redact(text).text,
                    tool,
                    conversation,
                })
            };
            if let Some(title) = &thread.title {
                add(title, false, false);
            }
            for evidence in db.review_evidence_for_lineage(&ids)? {
                if matches!(
                    evidence.kind.as_str(),
                    "initial_prompt" | "host_chat" | "user_answer"
                ) {
                    add(&evidence.text, false, false);
                }
            }
            for note in db.latest_ledgers(&ids)? {
                if !note.body.is_empty() {
                    add(&note.body, false, false);
                }
            }
            for run in &runs {
                if !(run.depth > 0 || Some(run.run_id.as_str()) == primary)
                    || !summaries.iter().any(|s| s.run_id == run.run_id)
                {
                    continue;
                }
                if let Some(input) = extracted.get(&run.run_id) {
                    for block in &input.blocks {
                        match block.kind {
                            IndexBlockKind::Text => add(&block.text, false, true),
                            IndexBlockKind::ToolUse
                            | IndexBlockKind::ToolResult
                            | IndexBlockKind::ToolError => add(&block.text, true, true),
                            IndexBlockKind::WorkingNote => {}
                        }
                    }
                }
            }
            items.insert(thread.thread_id.clone(), thread_items);
        }
        require(
            visible_runs.intersection(&indexable).count() > 0,
            "no_indexable_visible_runs",
        )?;
        // Use the same canonical project binding that request submission uses.
        // SELECT only; no hand-built index/state/provenance mutations.
        let bindings: Vec<(String, String)> = conn
            .prepare(
                "SELECT DISTINCT t.project_root, tr.run_id FROM thread_runs tr \
             JOIN threads t ON t.thread_id=tr.thread_id \
             WHERE t.project_root IS NOT NULL AND t.project_root <> ?1 \
             ORDER BY t.project_root, tr.run_id",
            )?
            .query_map([root], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let other_runs = bindings
            .iter()
            .map(|(_, run)| run.clone())
            .collect::<BTreeSet<_>>();
        require(
            other_runs.is_disjoint(&visible_runs),
            "cross_project_visible_backfill_overlap",
        )?;
        let other_indexable = other_runs.intersection(&indexable).count();
        println!(
            "{}",
            json!({
                "stage": "scope_counts", "visible_threads": threads.len(),
                "visible_runs": visible_runs.len(),
                "visible_indexable": visible_runs.intersection(&indexable).count(),
                "other_project_targets": other_runs.len(), "other_project_indexable": other_indexable,
            })
        );
        require(
            other_indexable >= super::REPETITIONS,
            "insufficient_other_project_indexable_runs",
        )?;
        let other_projects = bindings
            .into_iter()
            .map(|(project, _)| project)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok(Self {
            threads,
            visible_runs,
            source_runs: ids.into_iter().collect(),
            other_projects,
            other_indexable,
            classification,
            items,
            indexable,
        })
    }

    pub fn verify_seed(&self, db: &Database) -> Result<()> {
        let mut indexed = 0;
        for run in self.visible_runs.intersection(&self.indexable) {
            let status = db
                .thread_index_run_status(run)?
                .ok_or("missing_seed_status")?;
            require(
                status.state == IndexState::Indexed
                    && status.indexed_revision == Some(status.source.target.source_revision),
                "visible_seed_incomplete",
            )?;
            indexed += 1;
        }
        require(indexed > 0, "empty_index_false_success")
    }

    pub fn expected(&self, case: &Case) -> Result<HitSet> {
        let mut result = HitSet::new();
        for (thread, items) in &self.items {
            let mut count = 0;
            for item in items.iter().filter(|item| case.tool || !item.tool) {
                if case.matches(&item.text)? {
                    count += 1;
                }
            }
            if count > 0 {
                result.insert(thread.clone(), count);
            }
        }
        Ok(result)
    }

    pub fn cases(&self) -> Result<Vec<Case>> {
        let text = self
            .items
            .values()
            .flatten()
            .filter(|item| item.conversation && !item.tool)
            .map(|item| item.text.as_str());
        let ascii = Regex::new("[A-Za-z]{6,24}")?;
        let word = text
            .clone()
            .flat_map(|s| ascii.find_iter(s).map(|m| m.as_str().to_owned()))
            .next()
            .ok_or("no_ascii_case")?;
        let unicode = text
            .clone()
            .find_map(|s| {
                let chars = s
                    .chars()
                    .filter(|c| !c.is_ascii() && c.is_alphabetic())
                    .take(1)
                    .collect::<String>();
                (!chars.is_empty()).then_some(chars)
            })
            .ok_or("no_unicode_case")?;
        let phrase_regex = Regex::new(r"[A-Za-z]{3,16} [A-Za-z]{3,16}")?;
        let phrase = text
            .clone()
            .find_map(|s| phrase_regex.find(s).map(|m| m.as_str().to_owned()))
            .ok_or("no_phrase_case")?;
        let tool = self
            .items
            .values()
            .flatten()
            .filter(|item| item.tool)
            .find_map(|item| ascii.find(&item.text).map(|m| m.as_str().to_owned()))
            .ok_or("no_tool_case")?;
        let make = |class, query, mode, tool| Case {
            class,
            query,
            mode,
            tool,
        };
        let cases = vec![
            make("ascii_trigram", word.clone(), "words", false),
            make("short", word[..2].to_owned(), "words", false),
            make("unicode", unicode, "words", false),
            make("phrase", phrase, "phrase", false),
            make(
                "regex",
                format!("(?:{})", regex::escape(&word)),
                "regex",
                false,
            ),
            make("tool", tool, "words", true),
            make(
                "nohit",
                "s8_no_hit_7f59ccce960740858fb57524973ca455".into(),
                "words",
                false,
            ),
        ];
        for case in &cases {
            require(
                self.expected(case)?.is_empty() == (case.class == "nohit"),
                "case_expectation_invalid",
            )?;
        }
        Ok(cases)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_matcher_handles_short_unicode_phrase_regex() {
        for (query, mode, text, expected) in [
            ("ab", "words", "xxAByy", true),
            ("日本", "words", "日本語", true),
            ("alpha beta", "words", "beta ... alpha", true),
            ("alpha beta", "phrase", "alpha ... beta", false),
            ("a.+b", "regex", "AxxxB", true),
        ] {
            let case = Case {
                class: "test",
                query: query.into(),
                mode,
                tool: false,
            };
            assert_eq!(case.matches(text).unwrap(), expected);
        }
    }
}
