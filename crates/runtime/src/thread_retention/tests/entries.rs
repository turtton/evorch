//! The public entries that take a thread or run identity and can start, restore,
//! bind or answer work, each with the way a deleted thread or run is refused.
//!
//! A new public entry of that shape fails this test until it is added to the table
//! with its guard (or the reason none is needed). A guarded entry must call the guard
//! itself or delegate to a function that does; the behaviour is tested in
//! `thread_retention::tests::guard` and `chat_restore::tests::thread_deleted`.
//!
//! Internal (`pub(crate)` / `pub(super)`) functions are not listed: they are reachable
//! only through these entries, from a live run, or from storage, whose readers hide
//! tombstoned rows.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regex::Regex;

enum Guard {
    /// The body calls the deleted-thread guard, or calls `via[0]`, whose body calls
    /// `via[1]` and so on, the last of which calls it.
    Guarded { via: &'static [&'static str] },
    /// No guard needed, with the reason.
    Exempt(&'static str),
}

use Guard::{Exempt, Guarded};

const LIVE_ONLY: &str = "acts on a live run of this process; a deleted run is never live";
const READ_ONLY: &str = "reads storage, whose readers hide tombstoned rows; changes nothing";
const RESTORE_PATH: &str =
    "a run that is not live is restored through restore_queued_and_deliver, which is guarded";

fn table() -> BTreeMap<&'static str, Guard> {
    BTreeMap::from([
        ("ownership/host.rs::start", Guarded { via: &[] }),
        ("ownership/host.rs::attach", Guarded { via: &[] }),
        ("ownership/host.rs::claim", Guarded { via: &[] }),
        (
            "ownership/host.rs::owned_permit",
            Guarded { via: &["attach"] },
        ),
        (
            "ownership/registry.rs::start",
            Exempt("low-level registry write; OwnerHost is the entry and guards"),
        ),
        (
            "team.rs::claim",
            Exempt("in-memory team task queue of this session; not keyed by threads or runs"),
        ),
        (
            "ownership/registry.rs::attach",
            Exempt("low-level registry read; OwnerHost is the entry and guards"),
        ),
        ("thread_goals.rs::bind_thread_root", Guarded { via: &[] }),
        (
            "thread_goals.rs::create_thread_goal",
            Guarded {
                via: &["create_goal_inner"],
            },
        ),
        (
            "lane_delivery.rs::resume_lane_deliveries",
            Guarded { via: &[] },
        ),
        ("thread_goals.rs::restore_thread_goal", Guarded { via: &[] }),
        ("thread_todos.rs::restore_thread_todo", Guarded { via: &[] }),
        (
            "thread_read_dirs.rs::set_thread_read_only_directories",
            Guarded { via: &[] },
        ),
        (
            "runtime/chat_restore.rs::resume_chat",
            Guarded {
                via: &["resume", "continue_chat"],
            },
        ),
        (
            "runtime/chat_restore.rs::wake_chat",
            Guarded {
                via: &["resume", "continue_chat"],
            },
        ),
        (
            "runtime/chat_restore.rs::continue_goal",
            Guarded {
                via: &["continue_chat"],
            },
        ),
        (
            "runtime/chat_restore.rs::delegate_chat",
            Guarded {
                via: &["delegate_chat_seeded"],
            },
        ),
        (
            "runtime/chat_restore.rs::delegate_chat_seeded",
            Guarded { via: &[] },
        ),
        (
            "runtime/saved_compaction.rs::compact_saved_chat",
            Guarded { via: &[] },
        ),
        (
            "runtime/questions.rs::answer_subagent_question",
            Guarded {
                via: &["persist_subagent_answer"],
            },
        ),
        (
            "runtime/questions.rs::answer_user_question",
            Guarded {
                via: &[
                    "answer_question",
                    "answer_question_with_mode",
                    "handoff_answer_recipients",
                ],
            },
        ),
        (
            "runtime/questions.rs::answer_user_question_if_pending",
            Guarded {
                via: &["answer_question_with_mode", "handoff_answer_recipients"],
            },
        ),
        ("runtime.rs::send_agent_message", Exempt(RESTORE_PATH)),
        ("runtime.rs::send_message", Exempt(RESTORE_PATH)),
        ("runtime.rs::send_message_with_images", Exempt(RESTORE_PATH)),
        ("runtime.rs::deliver_notice", Exempt(LIVE_ONLY)),
        (
            "runtime.rs::deliver_follow_ups_next_turn",
            Exempt(LIVE_ONLY),
        ),
        ("runtime.rs::compact", Exempt(LIVE_ONLY)),
        (
            "runtime.rs::delegate_background_as_child",
            Exempt(LIVE_ONLY),
        ),
        ("runtime.rs::reserve_child_run_id", Exempt(LIVE_ONLY)),
        (
            "runtime.rs::spawn_reserved",
            Exempt("a run ID reserved by this process's counter, not an identity from outside"),
        ),
        ("runtime.rs::spawn_reserved_child", Exempt(LIVE_ONLY)),
        (
            "restore/diagnostics.rs::restore_diagnostics",
            Exempt(READ_ONLY),
        ),
        (
            "team_context.rs::persistent",
            Exempt("in-memory team context of this session"),
        ),
    ])
}

fn sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                if name != "tests" && name != "thread_retention" {
                    walk(&path, out);
                }
            } else if name.ends_with(".rs") && name != "tests.rs" && !name.ends_with("_tests.rs") {
                out.push(path);
            }
        }
    }
    let root =
        Path::new(&std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            (relative, std::fs::read_to_string(path).unwrap())
        })
        .collect()
}

/// The text of the function `name` that starts at `start` in `text`: up to the next
/// item at the same indentation.
fn body(text: &str, start: usize) -> &str {
    let after = &text[start..];
    let first = after.lines().next().unwrap_or_default();
    let pad = " ".repeat(first.len() - first.trim_start().len());
    let closing = format!("{pad}}}");
    let deeper = format!("{pad} ");
    let end = after
        .match_indices('\n')
        .map(|(at, _)| at + 1)
        .find(|at| after[*at..].starts_with(&closing) && !after[*at..].starts_with(&deeper))
        .map_or(after.len(), |at| at + closing.len());
    &after[..end]
}

fn mentions_guard(text: &str) -> bool {
    text.contains("deny_deleted") || text.contains("check_thread") || text.contains("check_run")
}

/// Resolve same-file calls, except the uniquely named handoff authority helper.
/// Generic names like `resume` must never match another module accidentally.
fn definition<'a>(sources: &'a [(String, String)], file: &str, name: &str) -> Option<&'a str> {
    let pattern = Regex::new(&format!(
        r"\n[ \t]*(?:pub(?:\([a-z:]+\))?\s+)?(?:async\s+)?fn\s+{name}\s*[<(]"
    ))
    .unwrap();
    sources
        .iter()
        .filter(|(path, _)| {
            path == if name == "handoff_answer_recipients" {
                "escalation/questions.rs"
            } else {
                file
            }
        })
        .find_map(|(_, text)| {
            pattern
                .find(text)
                .map(|found| body(text, found.start() + 1))
        })
}

// Given: every public entry of the runtime that takes a thread or run identity and starts,
// restores, binds or answers something
// When: compared with the table / Then: the sets are equal (a new entry or a removed one
// fails here), every guarded entry reaches the guard, and every exemption has a reason.
#[test]
fn every_public_identity_entry_is_guarded_or_exempt_with_a_reason() {
    let verbs = Regex::new(
        r"^(restore|resume|wake|continue|bind|create|answer|compact|delegate|handoff|send|deliver|renew|attach|start|claim|owned|set_thread|persist|spawn|reserve|register|install)",
    )
    .unwrap();
    let identity = Regex::new(
        r"thread_id|thread:|ThreadGoalSnapshot|ThreadTodoSnapshot|RunId|run_id|run:|child:|parent:|recipient|ThreadOwner|\bid: &str",
    )
    .unwrap();
    let entry =
        Regex::new(r"\n[ \t]*pub\s+(?:async\s+)?fn\s+([a-z_0-9]+)\s*(?:<[^>]*>)?\(").unwrap();
    let sources = sources();

    let mut found = BTreeMap::new();
    for (path, text) in &sources {
        for captures in entry.captures_iter(text) {
            let whole = captures.get(0).unwrap();
            let name = &captures[1];
            let mut depth = 1;
            let params_start = whole.end();
            let mut end = params_start;
            for (offset, character) in text[params_start..].char_indices() {
                depth += i32::from(character == '(') - i32::from(character == ')');
                if depth == 0 {
                    end = params_start + offset;
                    break;
                }
            }
            if verbs.is_match(name) && identity.is_match(&text[params_start..end]) {
                found.insert(format!("{path}::{name}"), whole.start() + 1);
            }
        }
    }

    let table = table();
    let listed = table
        .keys()
        .map(|key| (*key).to_owned())
        .collect::<Vec<_>>();
    let scanned = found.keys().cloned().collect::<Vec<_>>();
    assert_eq!(
        scanned, listed,
        "a public entry was added or removed: decide its guard and update the table"
    );
    for (key, guard) in &table {
        match guard {
            Exempt(reason) => assert!(!reason.is_empty(), "{key}"),
            Guarded { via } => {
                let (path, _) = key.split_once("::").unwrap();
                let text = &sources.iter().find(|(file, _)| file == path).unwrap().1;
                let mut current = body(text, found[*key]);
                for next in *via {
                    assert!(current.contains(next), "{key} must call {next}");
                    current = definition(&sources, path, next)
                        .unwrap_or_else(|| panic!("{key}: no function {next}"));
                }
                assert!(
                    mentions_guard(current),
                    "{key} must reach the deleted-thread guard"
                );
            }
        }
    }
}
