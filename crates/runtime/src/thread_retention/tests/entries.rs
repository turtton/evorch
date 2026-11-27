//! The public entries that take a thread or run identity and can start, restore,
//! bind or answer work, each with the way a deleted thread or run is refused.
//!
//! A new public entry of that shape fails this test until it is added to the table
//! with its guard (or the reason none is needed). A guarded entry must call the guard
//! itself or delegate to a function that does; the behaviour is tested in
//! `thread_retention::tests::guard` and `chat_restore::tests::thread_deleted`.
//!
//! A terminal entry may remain in memory after deletion: liveness and restore
//! assumptions are not exemptions from the mutable tombstone guard.

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

const READ_ONLY: &str = "reads storage, whose readers hide tombstoned rows; changes nothing";

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
            "ownership/host.rs::owned_or_claim_abandoned",
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
        (
            "attachments.rs::attachment_read_directories",
            Guarded { via: &[] },
        ),
        ("thread_goals.rs::bind_thread_root", Guarded { via: &[] }),
        (
            "thread_goals.rs::create_thread_goal",
            Guarded {
                via: &["create_goal_inner", "create_goal_random"],
            },
        ),
        (
            "lane_delivery.rs::resume_lane_deliveries",
            Guarded { via: &[] },
        ),
        (
            "lane_instructions.rs::send_lane_instruction",
            Guarded { via: &[] },
        ),
        ("thread_goals.rs::restore_thread_goal", Guarded { via: &[] }),
        (
            "thread_read_dirs.rs::set_thread_read_only_directories",
            Guarded { via: &[] },
        ),
        ("runtime/chat_restore.rs::resume_chat", Guarded { via: &[] }),
        (
            "runtime/chat_restore.rs::resume_chat_observed",
            Guarded { via: &[] },
        ),
        (
            "runtime/chat_restore.rs::wake_chat",
            Guarded {
                via: &["resume", "continue_chat", "continue_chat_fenced"],
            },
        ),
        (
            "runtime/chat_restore.rs::continue_goal",
            Guarded {
                via: &[
                    "continue_goal_with_model_update",
                    "continue_chat",
                    "continue_chat_fenced",
                ],
            },
        ),
        (
            "runtime/chat_restore.rs::continue_goal_with_model_update",
            Guarded {
                via: &["continue_chat", "continue_chat_fenced"],
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
            "runtime/restart_handoff.rs::persist_restart_tool_snapshot",
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
                via: &[
                    "answer_user_question_if_pending_from_host",
                    "answer_question_with_mode",
                    "handoff_answer_recipients",
                ],
            },
        ),
        (
            "runtime/questions.rs::answer_user_question_if_pending_from_host",
            Guarded {
                via: &["answer_question_with_mode", "handoff_answer_recipients"],
            },
        ),
        ("runtime.rs::send_agent_message", Guarded { via: &[] }),
        (
            "runtime.rs::send_message",
            Guarded {
                via: &[
                    "send_user_input",
                    "send_inbox_message",
                    "validate_run_mutation",
                ],
            },
        ),
        (
            "runtime.rs::send_user_input",
            Guarded {
                via: &["send_inbox_message", "validate_run_mutation"],
            },
        ),
        ("runtime.rs::deliver_notice", Guarded { via: &[] }),
        (
            "runtime.rs::deliver_follow_ups_next_turn",
            Guarded {
                via: &["validate_run_mutation"],
            },
        ),
        (
            "runtime.rs::compact",
            Guarded {
                via: &["validate_run_mutation"],
            },
        ),
        (
            "runtime.rs::delegate_background_as_child",
            Guarded {
                via: &["reserve_child_run_id"],
            },
        ),
        ("runtime.rs::reserve_child_run_id", Guarded { via: &[] }),
        ("runtime.rs::spawn_reserved", Guarded { via: &[] }),
        ("runtime.rs::spawn_reserved_child", Guarded { via: &[] }),
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
        r"thread_id|thread:|ThreadGoalSnapshot|RunId|run_id|run:|child:|parent:|recipient|ThreadOwner|\bid: &str",
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

// G1..G6 runtime entries. G1 completion also registers before event publication and
// keeps pending through recorder ACK (gate behavior tests). G7 storage lane writes,
// G8/G9 pre-save admission through mirror ACK, G10 owner shutdown are inventoried
// separately at their owning crates; LIVE_ONLY is deliberately not reused here.
#[test]
fn archive_admission_entries_are_closed_and_do_not_reuse_live_only_exemptions() {
    let registered: BTreeMap<&str, &str> = BTreeMap::from([
        ("runtime.rs::delegate_background", "G1"),
        ("runtime.rs::delegate_background_as_child", "G1"),
        ("runtime.rs::reserve_child_run_id", "G1"),
        ("runtime.rs::reserve_handoff_admission", "G1"),
        ("runtime.rs::spawn_escalated_root", "G1"),
        ("runtime.rs::spawn_lane_root", "G1"),
        ("runtime.rs::spawn_prepared_handoff", "G1"),
        ("runtime.rs::spawn_reserved", "G1"),
        ("runtime.rs::spawn_reserved_child", "G1"),
        ("runtime.rs::deliver_follow_ups_next_turn", "G2"),
        ("runtime.rs::deliver_notice", "G2"),
        ("runtime.rs::record_host_chat_submission", "G2"),
        ("runtime.rs::send_agent_message", "G2"),
        ("runtime.rs::send_internal_message", "G2"),
        ("runtime.rs::send_message", "G2"),
        ("runtime.rs::send_user_input", "G2"),
        ("lane_delivery.rs::resume_lane_deliveries", "G2"),
        ("lane_instructions.rs::send_lane_instruction", "G2"),
        ("runtime.rs::compact", "G3"),
        ("runtime/chat_restore.rs::continue_goal", "G3"),
        (
            "runtime/chat_restore.rs::continue_goal_with_model_update",
            "G3",
        ),
        ("runtime/chat_restore.rs::delegate_chat", "G3"),
        ("runtime/chat_restore.rs::delegate_chat_seeded", "G3"),
        ("runtime/chat_restore.rs::resume_chat", "G3"),
        ("runtime/chat_restore.rs::resume_chat_observed", "G3"),
        ("runtime/chat_restore.rs::wake_chat", "G3"),
        (
            "runtime/restart_handoff.rs::persist_restart_tool_snapshot",
            "G3",
        ),
        ("runtime/saved_compaction.rs::compact_saved_chat", "G3"),
        ("thread_goals.rs::bind_thread_root", "G4"),
        (
            "thread_read_dirs.rs::set_thread_read_only_directories",
            "G4",
        ),
        ("thread_goals.rs::create_agent_thread_goal", "G5"),
        ("thread_goals.rs::create_thread_goal", "G5"),
        ("thread_goals.rs::handoff_thread_goal", "G5"),
        ("thread_goals.rs::handoff_thread_goal_prepared", "G5"),
        ("thread_goals.rs::restore_committed_handoff", "G5"),
        ("thread_goals.rs::restore_thread_goal", "G5"),
        ("thread_goals.rs::set_goal_checks_paused", "G5"),
        ("thread_goals.rs::set_goal_review", "G5"),
        ("runtime/questions.rs::answer_subagent_question", "G6"),
        ("runtime/questions.rs::answer_user_question", "G6"),
        (
            "runtime/questions.rs::answer_user_question_if_pending",
            "G6",
        ),
        (
            "runtime/questions.rs::answer_user_question_if_pending_from_host",
            "G6",
        ),
        ("runtime/questions.rs::inherit_user_questions", "G6"),
        ("runtime/questions.rs::request_user_question", "G6"),
        ("runtime/questions.rs::request_user_question_outcome", "G6"),
    ]);
    let exempt = BTreeMap::from([
        (
            "ownership/host.rs::start",
            "owner registry permit only; run admission is separately registered",
        ),
        (
            "ownership/host.rs::attach",
            "owner registry lease only; not run admission",
        ),
        (
            "ownership/host.rs::claim",
            "owner registry adoption only; runtime restore admission is registered",
        ),
        (
            "ownership/host.rs::owned_permit",
            "delegates to owner registry attach; not execution",
        ),
        (
            "ownership/host.rs::owned_or_claim_abandoned",
            "owner registry adoption only; runtime restore admission is separately registered",
        ),
        (
            "ownership/registry.rs::start",
            "registry implementation, not archive eligibility or run admission",
        ),
        (
            "ownership/registry.rs::attach",
            "registry implementation, not archive eligibility or run admission",
        ),
        (
            "team.rs::claim",
            "session task queue, not durable thread/run input",
        ),
        (
            "pull_requests/service.rs::inherit",
            "pull-request context inheritance only, not sidebar membership, activity or retention protection",
        ),
        (
            "pull_requests/service.rs::inherit_now",
            "pull-request context inheritance only, not sidebar membership, activity or retention protection",
        ),
        (
            "team_context.rs::persistent",
            "constructs a session-local view, not durable mutation",
        ),
        (
            "attachments.rs::attachment_read_directories",
            "read-only metadata; uploads are G9 host guarded",
        ),
        (
            "restore/diagnostics.rs::restore_diagnostics",
            "read-only diagnostics",
        ),
    ]);
    let all = sources();
    for key in table().keys() {
        assert!(
            registered.contains_key(key) || exempt.contains_key(key),
            "classify automatic-archive entry independently: {key}"
        );
    }
    for (key, category) in &registered {
        let (path, name) = key.split_once("::").unwrap();
        let text = definition(&all, path, name).unwrap_or_else(|| panic!("{key} removed"));
        let text = if text.contains("deny_deleted_spawn(") {
            definition(&all, "runtime.rs", "deny_deleted_spawn").expect("spawn admission helper")
        } else if text.contains("self.continue_goal_with_model_update(") {
            definition(&all, path, "continue_goal_with_model_update")
                .expect("input-scoped continuation admission helper")
        } else {
            text
        };
        assert!(
            text.contains("retention_admission()") || text.contains("retention_run_admission("),
            "{category} {key} must register before work"
        );
    }
    let verbs=Regex::new(r"^(restore|resume|wake|continue|bind|create|answer|compact|delegate|handoff|send|deliver|inherit|set_goal|set_thread|persist|spawn|reserve|record_host|start|claim|owned|attach|install|register)").unwrap();
    let identity=Regex::new(r"thread_id|thread:|ThreadGoalSnapshot|RunId|run_id|run:|child:|parent:|recipient|ThreadOwner|\bid: &str").unwrap();
    let entry =
        Regex::new(r"\n[ \t]*pub\s+(?:async\s+)?fn\s+([a-z_0-9]+)\s*(?:<[^>]*>)?\(").unwrap();
    for (path, text) in &all {
        for cap in entry.captures_iter(text) {
            let m = cap.get(0).unwrap();
            let name = &cap[1];
            let mut depth = 1;
            let mut end = m.end();
            for (offset, c) in text[m.end()..].char_indices() {
                depth += i32::from(c == '(') - i32::from(c == ')');
                if depth == 0 {
                    end = m.end() + offset;
                    break;
                }
            }
            if verbs.is_match(name) && identity.is_match(&text[m.end()..end]) {
                let key = format!("{path}::{name}");
                assert!(
                    registered.contains_key(key.as_str()) || exempt.contains_key(key.as_str()),
                    "new automatic-archive entry must be classified: {key}"
                );
            }
        }
    }
}

// Creation/reactivation primitives are separately closed, including private cached
// admission and internal wake/resume routes. A new caller of a primitive fails
// classification, even when it is not a public identity entry. Behaviour is also
// exercised against retained terminal entries in the real bootstrap tests.
#[test]
fn internal_activation_routes_are_closed_and_reach_the_mutable_guard() {
    let sources = sources();
    let markers = [
        ".register_run(",
        ".build_run_candidate(",
        ".try_spawn_run_preadmitted(",
        ".try_spawn_run_with_handoff(",
        ".admit_run(",
        ".start_seeded_chat(",
        ".continue_chat_fenced(",
        ".commit_chat_resume(",
    ];
    let methods = Regex::new(
        r"\n[ \t]*(?:pub(?:\([a-z:]+\))?\s+)?(?:async\s+)?fn\s+([a-z_0-9]+)\s*(?:<[^>]*>)?\(",
    )
    .unwrap();
    let mut actual = std::collections::BTreeSet::new();
    for (file, text) in &sources {
        if ![
            "runtime.rs",
            "runtime/admission.rs",
            "runtime/chat_restore.rs",
        ]
        .contains(&file.as_str())
        {
            continue;
        }
        for capture in methods.captures_iter(text) {
            let name = &capture[1];
            let content = body(text, capture.get(0).unwrap().start() + 1);
            if name == "build_run_candidate"
                || markers.iter().any(|marker| content.contains(marker))
            {
                actual.insert(format!("{file}::{name}"));
            }
        }
    }
    let routes = [
        (
            "runtime.rs::register_run",
            Some(("runtime.rs", "build_run_candidate")),
        ),
        (
            "runtime.rs::spawn_run_with_handoff",
            Some(("runtime.rs", "try_spawn_run_preadmitted")),
        ),
        (
            "runtime.rs::try_spawn_run_with_handoff",
            Some(("runtime.rs", "try_spawn_run_preadmitted")),
        ),
        (
            "runtime.rs::try_spawn_run_preadmitted",
            Some(("runtime.rs", "try_spawn_run_preadmitted")),
        ),
        (
            "runtime.rs::build_run_candidate",
            Some(("runtime.rs", "build_run_candidate")),
        ),
        (
            "runtime.rs::registration_installs_seed_without_reading_a_closed_store",
            None,
        ), // Unit fixture, no live retention producer.
        (
            "runtime/admission.rs::register_without_admission",
            Some(("runtime.rs", "build_run_candidate")),
        ),
        (
            "runtime/admission.rs::admit_run",
            Some(("runtime/admission.rs", "admit_run")),
        ),
        (
            "runtime/chat_restore.rs::resume_chat_fenced",
            Some(("runtime/chat_restore.rs", "continue_chat_fenced")),
        ),
        (
            "runtime/chat_restore.rs::continue_chat",
            Some(("runtime/chat_restore.rs", "continue_chat_fenced")),
        ),
        (
            "runtime/chat_restore.rs::continue_chat_fenced",
            Some(("runtime/chat_restore.rs", "continue_chat_fenced")),
        ),
        (
            "runtime/chat_restore.rs::delegate_chat_seeded",
            Some(("runtime/chat_restore.rs", "delegate_chat_seeded")),
        ),
    ];
    assert_eq!(
        actual,
        routes.iter().map(|(key, _)| (*key).to_string()).collect(),
        "classify every new internal activation caller"
    );
    for (key, route) in routes {
        if let Some((file, name)) = route {
            assert!(
                mentions_guard(definition(&sources, file, name).unwrap()),
                "{key} must reach the mutable guard"
            );
        }
    }
}
