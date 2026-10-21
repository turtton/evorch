use std::{fs, sync::Arc};

use event_bus::{Event, EventBus, SkillDiagnosticKind};
use storage::{Database, Storage, StorageConfig, memory::Lesson};

use super::*;

struct Fixture {
    _storage: Storage,
    config: StorageConfig,
    settings: ImprovementSettings,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("test.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let settings = ImprovementSettings {
            writer: storage.handle(),
            project: "test-project".into(),
            policy: ImprovementPolicy {
                draft_dir: Some(dir.path().join("drafts")),
                ..Default::default()
            },
        };
        Self {
            _storage: storage,
            config,
            settings,
            dir,
        }
    }

    fn collector(&self) -> ImprovementCollector {
        ImprovementCollector::new(self.settings.clone())
    }

    fn candidates(&self) -> Vec<ImprovementCandidate> {
        Database::open(&self.config)
            .unwrap()
            .improvement_candidates("test-project", None, 100)
            .unwrap()
    }

    fn draft_files(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.settings.policy.draft_dir.as_ref().unwrap()) else {
            return vec![];
        };
        entries
            .map(|e| e.unwrap().path())
            .filter(|path| path.is_file())
            .collect()
    }
}

fn diagnostic(code: &str) -> DiagnosticEvent {
    DiagnosticEvent {
        source: "test".into(),
        severity: DiagnosticSeverity::Warning,
        code: code.into(),
        detail: "evidence-backed observation\nmore detail".into(),
        run_id: Some("run-7".into()),
        thread_id: Some("thread-2".into()),
        call_id: Some("call-3".into()),
    }
}

fn lesson() -> Lesson {
    Lesson {
        id: "promoted-1".into(),
        project: "test-project".into(),
        task_id: "task-1".into(),
        content: "Check evidence before proposing changes".into(),
        evidence: "{\"finding\":\"verified observation\"}".into(),
        scope: storage::memory::LessonScope::Harness,
    }
}

#[test]
fn all_real_diagnostic_codes_and_unknown_are_pinned() {
    use CandidateClass::*;
    // Full producer inventory: runtime, providers/cache, tools LSP/MCP, GUI browser.
    let cases = [
        ("IdenticalToolCalls", HarnessImprovement),
        ("DelegationRetracted", HarnessImprovement),
        ("NoProgress", HarnessImprovement),
        ("LearningPipelineFailed", HarnessImprovement),
        ("ContextSnapshotFailed", HarnessImprovement),
        ("EscalationHandoffFailed", HarnessImprovement),
        ("CrashRecovered", HarnessImprovement),
        ("AgentRunPanicked", HarnessImprovement),
        ("StorageWriterHalted", HarnessImprovement),
        ("EscalationAdmissionFailed", TransientOrExternal),
        ("BudgetWarning", TransientOrExternal),
        ("BudgetExhausted", TransientOrExternal),
        ("ProviderUnavailable", TransientOrExternal),
        ("CacheRegression", TransientOrExternal),
        ("ToolArgumentsMalformed", TransientOrExternal),
        ("CompactionFailed", TransientOrExternal),
        ("ContextCheckpointSaved", Ignored),
        ("EvaluationFinished", Ignored),
        ("escalation_review", Ignored),
        ("tool_call_access", Ignored),
        ("scope_denied", Ignored),
        ("tool_result", Ignored),
        ("publish_diagnostics", Ignored),
        ("browser.error", Ignored),
        ("browser.close_error", Ignored),
        ("browser.action_error", Ignored),
        ("browser.session", Ignored),
        ("browser.stopped", Ignored),
        ("browser.action", Ignored),
        ("browser.dom_diff", Ignored),
        ("browser.screenshot", Ignored),
        // These are tool-result codes only, not DiagnosticEvent emitters.
        ("unknown_run", Ignored),
        ("run_output_denied", Ignored),
        ("UnexpectedModelSwitch", Ignored),
        ("future-code", Ignored),
    ];
    // A new shared code needs its own pinned decision here, not the unknown fallback.
    for code in diagnostic_codes::ALL {
        assert!(known_class(code).is_some(), "{code} has no explicit class");
        assert!(
            cases.iter().any(|(pinned, _)| pinned == code),
            "{code} is not pinned"
        );
    }
    for code in ["unknown_run", "run_output_denied", "future-code"] {
        assert_eq!(known_class(code), None, "{code}");
    }
    for (code, class) in cases {
        for severity in [
            DiagnosticSeverity::Info,
            DiagnosticSeverity::Warning,
            DiagnosticSeverity::Error,
        ] {
            assert_eq!(
                classify_diagnostic(&DiagnosticEvent {
                    severity,
                    ..diagnostic(code)
                }),
                class,
                "{code}"
            );
        }
    }
}

#[test]
fn text_bound_is_byte_exact_and_multibyte_safe() {
    assert_eq!(bound_text("abc", 3), "abc");
    assert_eq!(bound_text("", 0), "");
    assert_eq!(bound_text("abcdef", 0), "");
    assert_eq!(bound_text(&"a".repeat(100), 20).len(), 20);
    let source = "日本語🦀".repeat(100);
    for limit in 0..100 {
        let text = bound_text(&source, limit);
        assert!(text.len() <= limit);
        if limit >= "…[truncated]".len() {
            assert!(text.ends_with("…[truncated]"));
        }
    }
}

#[test]
fn renders_marked_deterministic_review_only_drafts() {
    let f = Fixture::new();
    f.collector().handle_diagnostic(&diagnostic("NoProgress"));
    let c = f.candidates().remove(0);
    let issue = render_issue_draft(&c);
    let packet = render_packet_draft(&c);
    for text in [&issue, &packet] {
        assert!(text.starts_with(drafts::DRAFT_MARKER));
        assert!(text.contains("NoProgress"));
        assert!(text.contains("evidence-backed observation"));
    }
    assert_eq!(issue, render_issue_draft(&c));
    assert_eq!(packet, render_packet_draft(&c));
    for heading in [
        "Title",
        "Summary",
        "Evidence",
        "Environment",
        "Suggested next steps",
    ] {
        assert!(issue.contains(heading));
    }
    assert!(packet.contains("not a canonical packet; do not place under .intent-cli"));
    assert!(packet.contains("implementation_issue_packet:"));
    assert!(packet.contains("status: draft"));
    assert!(packet.contains("Reproduce the no-progress loop using evidence.fields"));
    assert!(!packet.contains("TBD by reviewer"));
    assert!(issue.contains("| occurrences | 1 |"));
    assert!(issue.contains("draft-generation intake snapshot"));
    assert!(packet.contains(&format!("draft-si-{}", c.id)));
}

#[test]
fn collector_persists_attaches_deduplicates_and_ignores() {
    let f = Fixture::new();
    let collector = f.collector();
    for code in [
        "ProviderUnavailable",
        "BudgetWarning",
        "BudgetExhausted",
        "tool_result",
        "unknown",
    ] {
        collector.handle_diagnostic(&diagnostic(code));
    }
    assert!(f.candidates().is_empty());
    assert!(f.draft_files().is_empty());
    collector.handle_diagnostic(&diagnostic("IdenticalToolCalls"));
    collector.handle_diagnostic(&diagnostic("IdenticalToolCalls"));
    let rows = f.candidates();
    assert_eq!(rows.len(), 1);
    let c = &rows[0];
    assert_eq!(c.code, "IdenticalToolCalls");
    assert_eq!(c.run_id.as_deref(), Some("run-7"));
    assert_eq!(c.status, ImprovementStatus::New);
    assert_eq!(c.severity, ImprovementSeverity::Warning);
    assert_eq!(
        c.draft_path.as_deref(),
        Some(format!("{}.issue.md", c.id).as_str())
    );
    assert_eq!(f.draft_files().len(), 2);
    for suffix in ["issue.md", "packet.md"] {
        let text = fs::read_to_string(
            f.settings
                .policy
                .draft_dir
                .as_ref()
                .unwrap()
                .join(format!("{}.{suffix}", c.id)),
        )
        .unwrap();
        assert!(text.starts_with(drafts::DRAFT_MARKER));
    }
    let evidence: Value = serde_json::from_str(&c.evidence).unwrap();
    assert_eq!(evidence["call_id"], "call-3");
    collector.ingest_lessons(&[lesson()], Some("run-7"));
    collector.ingest_lessons(&[lesson()], Some("run-7"));
    let rows = f.candidates();
    assert_eq!(rows.len(), 2);
    let promoted = rows.iter().find(|c| c.code == "LessonPromoted").unwrap();
    assert_eq!(promoted.source, ImprovementSource::Lesson);
    assert_eq!(promoted.run_id.as_deref(), Some("run-7"));
    assert_eq!(promoted.occurrences, 2);
    let evidence: Value = serde_json::from_str(&promoted.evidence).unwrap();
    assert_eq!(evidence["source_run_id"], "run-7");
    assert_eq!(evidence["lesson_id"], lesson().id);
    assert_eq!(evidence["content"], lesson().content);
    // Lesson evidence stays an opaque string, never reparsed into the candidate JSON.
    assert_eq!(evidence["evidence_refs"], lesson().evidence);
    assert_eq!(evidence["build"], build_info());
    assert_eq!(f.draft_files().len(), 4);
}

#[test]
fn guarded_evidence_is_warn_only_and_never_written_to_drafts() {
    const CHILD: &str = "EVORCH_RUNTIME_IMPROVEMENT_SECRET_CHILD";
    const SENTINEL: &str = "evorch-runtime-improvement-sentinel-0123456789";
    if std::env::var_os(CHILD).is_none() {
        // Match storage's subprocess pattern; never mutate the parallel runner's env.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "self_improvement::tests::guarded_evidence_is_warn_only_and_never_written_to_drafts", "--nocapture"])
            .env(CHILD, "1").env("GH_TOKEN", SENTINEL).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let f = Fixture::new();
    f.collector().handle_diagnostic(&DiagnosticEvent {
        detail: format!("safe first line\n{SENTINEL}"),
        ..diagnostic("NoProgress")
    });
    f.collector().ingest_lessons(
        &[Lesson {
            evidence: SENTINEL.into(),
            ..lesson()
        }],
        Some("run-7"),
    );
    assert!(f.candidates().is_empty());
    assert!(f.draft_files().is_empty());
}

#[test]
fn only_harness_scoped_lessons_become_candidates() {
    let f = Fixture::new();
    let scoped = |id: &str, scope| Lesson {
        id: id.into(),
        scope,
        ..lesson()
    };
    f.collector().ingest_lessons(
        &[
            scoped("project-1", storage::memory::LessonScope::Project),
            scoped("user-1", storage::memory::LessonScope::User),
            scoped("harness-1", storage::memory::LessonScope::Harness),
        ],
        Some("run-7"),
    );
    let rows = f.candidates();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].dedup_key, "lesson:harness-1");
}

#[test]
fn draft_failure_does_not_undo_persistence_or_panic() {
    let mut f = Fixture::new();
    let blocked = f.dir.path().join("blocked");
    fs::write(&blocked, "not a directory").unwrap();
    f.settings.policy.draft_dir = Some(blocked);
    f.collector().ingest_lessons(&[lesson()], Some("run-7"));
    let c = f.candidates().remove(0);
    assert_eq!(c.code, "LessonPromoted");
    assert!(c.draft_path.is_none());
}

#[test]
fn spool_drain_and_bounded_crash_intake() {
    let f = Fixture::new();
    let spool = f.dir.path().join("spool");
    fs::create_dir(&spool).unwrap();
    fs::write(spool.join("crash-12.json"), json!({
        "message": "x".repeat(100 * 1024), "location": "test.rs:4", "thread": "test", "timestamp": 12,
    }).to_string()).unwrap();
    fs::write(spool.join("crash-broken.json"), "broken").unwrap();
    fs::write(spool.join("unrelated.json"), "leave alone").unwrap();
    let crashes = drain_crash_spool(&spool);
    assert_eq!(crashes.len(), 2);
    assert_eq!(crashes[1].message, "<unreadable spool entry>");
    assert!(drain_crash_spool(&spool).is_empty());
    assert!(spool.join("unrelated.json").exists());
    f.collector().ingest_crashes(crashes);
    let rows = f.candidates();
    assert_eq!(rows.len(), 2);
    for c in rows {
        assert_eq!(c.code, "CrashRecovered");
        assert_eq!(c.severity, ImprovementSeverity::Error);
        assert!(c.title.chars().count() <= 120);
        assert!(c.evidence.len() <= 2048);
        let _: Value = serde_json::from_str(&c.evidence).unwrap();
        assert!(c.draft_path.is_some());
    }
}

#[test]
fn panic_hook_spools_in_an_isolated_process() {
    const CHILD: &str = "EVORCH_RUNTIME_CRASH_HOOK_CHILD";
    let f = Fixture::new();
    if let Some(path) = std::env::var_os(CHILD) {
        install_crash_spool(PathBuf::from(path));
        assert!(std::panic::catch_unwind(|| panic!("{}", "🦀".repeat(4096))).is_err());
        return;
    }
    let spool = f.dir.path().join("hook");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "self_improvement::tests::panic_hook_spools_in_an_isolated_process",
            // Lets the previous hook's report reach stderr instead of libtest's capture.
            "--nocapture",
        ])
        .env(CHILD, &spool)
        .output()
        .unwrap();
    assert!(output.status.success());
    // The previous (default) hook still reports to stderr.
    assert!(String::from_utf8_lossy(&output.stderr).contains("panicked at"));
    let crashes = drain_crash_spool(&spool);
    assert_eq!(crashes.len(), 1);
    assert!(crashes[0].message.len() <= 4096);
    assert!(crashes[0].message.ends_with("…[truncated]"));
    assert!(crashes[0].location.is_some());
    let backtrace = crashes[0].backtrace.as_deref().unwrap();
    assert!(
        backtrace.contains("panic_hook_spools_in_an_isolated_process"),
        "{backtrace}"
    );
    assert!(!backtrace.contains("std::panicking"), "{backtrace}");
}

#[test]
fn caught_panics_are_left_to_their_catcher_not_spooled() {
    const CHILD: &str = "EVORCH_RUNTIME_CAUGHT_PANIC_CHILD";
    let f = Fixture::new();
    if let Some(path) = std::env::var_os(CHILD) {
        install_crash_spool(PathBuf::from(path));
        let caught = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(crate::panic_capture::catch_panic(async {
                panic!("caught by the runtime")
            }))
            .unwrap_err();
        assert_eq!(caught.message, "caught by the runtime");
        assert!(caught.location.is_some());
        assert!(std::panic::catch_unwind(|| panic!("outside any run")).is_err());
        return;
    }
    let spool = f.dir.path().join("hook");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "self_improvement::tests::caught_panics_are_left_to_their_catcher_not_spooled",
        ])
        .env(CHILD, &spool)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let crashes = drain_crash_spool(&spool);
    assert_eq!(crashes.len(), 1);
    assert_eq!(crashes[0].message, "outside any run");
}

#[test]
fn spooled_harness_faults_become_candidates_and_others_are_dropped() {
    let f = Fixture::new();
    let spool = f.dir.path().join("spool");
    for _ in 0..2 {
        spool_fault(
            &spool,
            "StorageWriterHalted",
            "storage:writer_closed",
            "event writes halted: writer closed",
        )
        .unwrap();
    }
    // Not a harness code: a spooled fault must not bypass the allowlist.
    spool_fault(&spool, "CompactionFailed", "compaction", "summary failed").unwrap();
    let spooled = drain_crash_spool(&spool);
    assert_eq!(spooled.len(), 3);
    f.collector().ingest_crashes(spooled);
    let rows = f.candidates();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].code, "StorageWriterHalted");
    assert_eq!(rows[0].occurrences, 2);
    assert_eq!(
        rows[0].dedup_key,
        "spool:StorageWriterHalted:storage:writer_closed"
    );
    assert_eq!(
        rows[0].title,
        "StorageWriterHalted: event writes halted: writer closed"
    );
    let evidence: Value = serde_json::from_str(&rows[0].evidence).unwrap();
    assert_eq!(evidence["location"], "storage:writer_closed");
}

#[test]
fn diagnostic_sites_keep_occurrences_apart() {
    let f = Fixture::new();
    let panicked = |site: &str| DiagnosticEvent {
        detail: format!("boom\nsite={site}\nbacktrace:\nframe"),
        ..diagnostic("AgentRunPanicked")
    };
    let collector = f.collector();
    collector.handle_diagnostic(&panicked("crates/runtime/src/a.rs:1:1"));
    collector.handle_diagnostic(&panicked("crates/runtime/src/a.rs:1:1"));
    collector.handle_diagnostic(&panicked("crates/runtime/src/b.rs:2:2"));
    let mut rows = f.candidates();
    rows.sort_by_key(|c| c.dedup_key.clone());
    let keys: Vec<_> = rows
        .iter()
        .map(|c| (c.dedup_key.as_str(), c.occurrences))
        .collect();
    assert_eq!(
        keys,
        [
            ("diag:test:AgentRunPanicked:crates/runtime/src/a.rs:1:1", 2),
            ("diag:test:AgentRunPanicked:crates/runtime/src/b.rs:2:2", 1),
        ]
    );
}

#[test]
fn compact_backtrace_keeps_harness_frames_on_one_line_each() {
    let rendered = "   0: std::backtrace_rs::backtrace::libunwind::trace
             at /rustc/abc/library/std/src/../../backtrace/src/backtrace/libunwind.rs:116:5
   1: runtime::self_improvement::crash::install_crash_spool::{{closure}}
             at /build/crates/runtime/src/self_improvement/crash.rs:60:13
   2: std::panicking::rust_panic_with_hook
   3: runtime::agent_loop::LoopState::step
             at /nix/store/x-source/crates/runtime/src/agent_loop.rs:812:9
   4: <unknown>
   5: tokio::runtime::task::harness::poll_future
   6: serde_json::de::from_str
";
    assert_eq!(
        crash::compact_backtrace(rendered),
        "runtime::agent_loop::LoopState::step @ crates/runtime/src/agent_loop.rs:812:9\n\
         serde_json::de::from_str"
    );
    let many: String = (0..30)
        .map(|i| format!("  {i}: runtime::frame{i}\n"))
        .collect();
    let compact = crash::compact_backtrace(&many);
    assert_eq!(compact.lines().count(), 25);
    assert!(compact.ends_with("… 6 more frames"));
}

#[test]
fn crashes_at_the_same_site_fold_into_one_candidate() {
    let f = Fixture::new();
    let crash = |file: &str, location: Option<&str>| SpooledCrash {
        file_name: file.into(),
        message: "boom".into(),
        location: location.map(Into::into),
        thread: None,
        timestamp_unix: 1,
        build: None,
        backtrace: Some("runtime::agent_loop::LoopState::step".into()),
        code: None,
    };
    f.collector().ingest_crashes(vec![
        crash(
            "crash-1-1.json",
            Some("crates/runtime/src/agent_loop.rs:812:9"),
        ),
        crash(
            "crash-2-1.json",
            Some("crates/runtime/src/agent_loop.rs:812:9"),
        ),
        crash("crash-3-1.json", Some("crates/runtime/src/compose.rs:10:1")),
    ]);
    let mut rows = f.candidates();
    rows.sort_by_key(|c| c.occurrences);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].occurrences, 2);
    assert_eq!(
        rows[1].dedup_key,
        "crash:crates/runtime/src/agent_loop.rs:812:9"
    );
    let evidence: Value = serde_json::from_str(&rows[1].evidence).unwrap();
    assert_eq!(
        evidence["backtrace"],
        "runtime::agent_loop::LoopState::step"
    );
}

#[test]
fn collection_gates_and_write_limits_apply() {
    let mut f = Fixture::new();
    f.settings.policy.collect_diagnostics = false;
    f.settings.policy.collect_lessons = false;
    let collector = f.collector();
    collector.handle_diagnostic(&diagnostic("NoProgress"));
    collector.ingest_lessons(&[lesson()], Some("run-7"));
    collector.ingest_crashes(vec![SpooledCrash {
        file_name: "crash-1.json".into(),
        message: "panic".into(),
        location: None,
        thread: None,
        timestamp_unix: 1,
        build: None,
        backtrace: None,
        code: None,
    }]);
    assert!(f.candidates().is_empty());
    assert!(f.draft_files().is_empty());
    f.settings.policy.collect_diagnostics = true;
    f.settings.policy.daily_limit = 1;
    let collector = f.collector();
    collector.handle_diagnostic(&diagnostic("NoProgress"));
    collector.handle_diagnostic(&diagnostic("LearningPipelineFailed"));
    assert_eq!(f.candidates().len(), 1);
    assert_eq!(f.draft_files().len(), 2);
}

#[test]
fn escaped_json_and_metadata_fit_the_total_evidence_cap() {
    let mut f = Fixture::new();
    for limit in [0, 1, 16, 256, 2048, 65_536, u32::MAX] {
        f.settings.policy.evidence_max_bytes = limit;
        let text = f.collector().json_evidence(json!({
            "fields": {"error": "\n\"🦀".repeat(10_000)},
            "detail": "\n\"🦀".repeat(10_000), "source": "s".repeat(8000), "run_id": "r".repeat(8000),
        }));
        assert!(text.len() <= limit.min(65_536) as usize);
        if limit > 0 {
            let _: Value = serde_json::from_str(&text).unwrap();
        }
    }
}

#[test]
fn sanitized_names_cannot_escape_draft_directory() {
    let f = Fixture::new();
    let (issue, packet) = f
        .collector()
        .write_drafts("../../unsafe/🦀", "issue", "packet")
        .unwrap();
    assert_eq!(issue, ".._.._unsafe__.issue.md");
    assert_eq!(packet, ".._.._unsafe__.packet.md");
    assert_eq!(f.draft_files().len(), 2);
}

#[test]
fn titles_strip_call_and_run_ids() {
    let f = Fixture::new();
    let collector = f.collector();
    for run in ["run-123", "run-01k6z3x2q4w8e9r0t1y2u3i4o5"] {
        collector.handle_diagnostic(&DiagnosticEvent {
            detail: format!(
                "failed {run} thread-42 toolu_Abc123 call_xyz987 17 times\nsite=stable"
            ),
            ..diagnostic("NoProgress")
        });
    }
    let rows = f.candidates();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].title,
        "NoProgress: failed run-N thread-N <id> <id> N times"
    );
    assert_eq!(rows[0].dedup_key, "diag:test:NoProgress:stable");
    assert_eq!(rows[0].occurrences, 2);
}

#[test]
fn normalization_handles_identifiers_paths_whitespace_and_unicode() {
    for (raw, expected) in [
        ("run-123 thread-456", "run-N thread-N"),
        ("run-01K6Z3X2Q4W8E9R0T1Y2U3I4O5", "run-N"),
        ("toolu_abC12 call_x-y_123", "<id> <id>"),
        ("0123456789abcdef0123456789abcdef", "<id>"),
        ("550e8400-e29b-41d4-a716-446655440000", "<id>"),
        (
            "path=/home/user/file.rs:12 /var/tmp/foo",
            "path=<path> <path>",
        ),
        ("\t 日本語 123\n🦀42\u{2003}done ", "日本語 N 🦀N done"),
        ("v1.2 attempts=007", "vN.N attempts=N"),
        ("run-abc thread-abc", "run-abc thread-abc"),
        (
            "mycall_abc abcdefabcdefabcdefabcdefabcdefabcdef",
            "mycall_abc abcdefabcdefabcdefabcdefabcdefabcdef",
        ),
        ("", ""),
    ] {
        assert_eq!(normalize_variable_parts(raw), expected, "{raw}");
    }
}

#[test]
fn detail_key_value_lines_become_structured_evidence_and_summary() {
    let f = Fixture::new();
    let detail = "Repeated tool failures without progress\ntrigger=repeated_failures\ncount=5\ntool=read\npath=/tmp/example\ninput_sha256=abc123\nerror=failed: a=b\nsite=escalation:failure\n_key2=value\nempty=\nBad=ignored\n invalid=ignored\n1key=ignored\nbad-key=ignored\n=ignored";
    f.collector().handle_diagnostic(&DiagnosticEvent {
        detail: detail.into(),
        ..diagnostic("NoProgress")
    });
    let c = f.candidates().remove(0);
    let evidence: Value = serde_json::from_str(&c.evidence).unwrap();
    assert_eq!(evidence["detail"], detail);
    assert_eq!(
        evidence["fields"],
        json!({
            "trigger": "repeated_failures", "count": "5", "tool": "read", "path": "/tmp/example",
            "input_sha256": "abc123", "error": "failed: a=b", "site": "escalation:failure",
            "_key2": "value", "empty": "",
        })
    );
    assert_eq!(c.dedup_key, "diag:test:NoProgress:escalation:failure");
    let issue = render_issue_draft(&c);
    for row in [
        "| tool | read |",
        "| path | /tmp/example |",
        "| count | 5 |",
        "| trigger | repeated&#95;failures |",
        "| occurrences | 1 |",
    ] {
        assert!(issue.contains(row), "{issue}");
    }
}

#[test]
fn nested_fields_remain_valid_and_bounded_and_markdown_is_inert() {
    let f = Fixture::new();
    let encoded = f.collector().json_evidence(json!({
        "detail": "original", "fields": {"error": "🦀".repeat(3000)},
    }));
    assert!(encoded.len() <= 2048);
    let evidence: Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(evidence["detail"], "original");
    assert!(
        evidence["fields"]["error"]
            .as_str()
            .unwrap()
            .ends_with("…[truncated]")
    );
    f.collector().handle_diagnostic(&diagnostic("NoProgress"));
    let mut c = f.candidates().remove(0);
    c.evidence = json!({"fields": {"tool": "x|[click](url)<script>\n## injected"}}).to_string();
    let issue = render_issue_draft(&c);
    let summary = issue
        .split("## Summary")
        .nth(1)
        .unwrap()
        .split("## Evidence")
        .next()
        .unwrap();
    assert!(summary.contains("x&#124;&#91;click&#93;(url)&lt;script&gt; ## injected"));
    assert!(!summary.contains("\n## injected"));
}

#[test]
fn run_error_classification_uses_exclusions_and_falls_back() {
    use run_errors::classify_run_error;
    for excluded in [
        "cancelled",
        "stopped",
        "stopped: operator request",
        "model stopped: unknown",
        "model response reached length limit",
        "model response was blocked by content filter",
        "Goal cumulative budget exhausted",
        "モデル呼び出しに失敗しました: timeout",
        "provider error: rate limited",
        "panicked: index out of bounds",
        "panicked:",
    ] {
        assert_eq!(classify_run_error(excluded), None, "{excluded}");
    }
    for code in diagnostic_codes::ALL {
        assert_eq!(
            classify_run_error(&format!("{code}: detail")),
            None,
            "{code}"
        );
    }
    for (reason, code) in [
        ("database is locked", "storage_busy"),
        (
            "Cannot persist prepared context: database is locked",
            "storage_busy",
        ),
        (
            "thread has an active turn or needs a checkpoint",
            "ownership",
        ),
        ("owner or generation no longer matches", "ownership"),
        ("thread is not accepting new turns", "ownership"),
        ("owner is not claimable", "ownership"),
        ("generation exhausted", "ownership"),
        (
            "previous owner still responds or liveness could not be established",
            "ownership",
        ),
        ("run ownership generation is stale: run-4", "ownership"),
        ("Cannot persist prepared context: disk full", "persist"),
        (
            "Cannot record tool intent before execution: disk full",
            "persist",
        ),
        (
            "ContextWindowExhausted: context too large",
            "context_window",
        ),
        ("scratch setup failed: permission denied", "scratch"),
        ("runtime unavailable", "runtime"),
        ("runtime is unavailable", "runtime"),
        ("interactive inbox closed", "runtime"),
        ("unexpected failure", "unclassified"),
        ("NoProgress:without space", "unclassified"),
    ] {
        assert_eq!(
            classify_run_error(reason),
            Some(format!("RunError:{code}").as_str()),
            "{reason}"
        );
    }
}

#[tokio::test]
async fn run_errors_are_collected_deduplicated_and_gated_on_the_bus() {
    for enabled in [true, false] {
        let mut f = Fixture::new();
        f.settings.policy.collect_diagnostics = enabled;
        let bus = EventBus::new(32);
        let receiver = bus.subscribe();
        let mut observed = None;
        for (run_id, phase, reason) in [
            (
                "run-1",
                AgentRunPhase::Error,
                Some("database is locked for run-1 call_first"),
            ),
            (
                "run-2",
                AgentRunPhase::Error,
                Some("database is locked for run-2 call_second"),
            ),
            ("run-3", AgentRunPhase::Error, Some("cancelled")),
            (
                "run-4",
                AgentRunPhase::Error,
                Some("BudgetExhausted: limit reached"),
            ),
            ("run-5", AgentRunPhase::Error, Some("unknown failure 123")),
            ("run-6", AgentRunPhase::Done, Some("not an error")),
            ("run-7", AgentRunPhase::Error, None),
        ] {
            let event = Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: run_id.into(),
                from: AgentRunPhase::Running,
                to: phase,
                reason: reason.map(str::to_owned),
            });
            if run_id == "run-1" {
                observed = Some(event.meta.wall_clock);
            }
            bus.emit(event);
        }
        let _ = tokio::time::timeout(Duration::from_millis(300), f.collector().run(receiver)).await;
        let rows = f.candidates();
        if !enabled {
            assert!(rows.is_empty());
            assert!(f.draft_files().is_empty());
            continue;
        }
        assert_eq!(rows.len(), 2);
        let busy = rows
            .iter()
            .find(|c| c.code == "RunError:storage_busy")
            .unwrap();
        assert_eq!(busy.occurrences, 2);
        assert_eq!(busy.severity, ImprovementSeverity::Warning);
        assert_eq!(
            busy.dedup_key,
            "run-error:RunError:storage_busy:database is locked for run-N <id>"
        );
        assert_eq!(
            busy.title,
            "RunError:storage_busy: database is locked for run-N <id>"
        );
        let evidence: Value = serde_json::from_str(&busy.evidence).unwrap();
        assert_eq!(
            evidence["reason"],
            "database is locked for run-1 call_first"
        );
        assert_eq!(evidence["normalized"], "database is locked for run-N <id>");
        assert_eq!(evidence["run_id"], "run-1");
        assert_eq!(evidence["observed_at_ns"], unix_ns(observed.unwrap()));
        assert_eq!(evidence["build"], build_info());
        assert!(rows.iter().any(|c| c.code == "RunError:unclassified"));
    }
}

// Given: a panic reported as AgentRunPanicked followed by the run's "panicked:" Error
// When: the collector observes both on the bus
// Then: only the diagnostic becomes a candidate; the lifecycle failure is not doubled
#[tokio::test]
async fn panicked_run_error_is_not_a_second_candidate() {
    let f = Fixture::new();
    let bus = EventBus::new(32);
    let receiver = bus.subscribe();
    bus.emit(Event::new(DiagnosticEvent {
        source: "agent_runtime".into(),
        severity: DiagnosticSeverity::Error,
        code: diagnostic_codes::AGENT_RUN_PANICKED.into(),
        detail: "boom\nsite=crates/runtime/src/lib.rs:1:1".into(),
        run_id: Some("run-9".into()),
        thread_id: None,
        call_id: None,
    }));
    bus.emit(Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: "run-9".into(),
        from: AgentRunPhase::Running,
        to: AgentRunPhase::Error,
        reason: Some("panicked: boom".into()),
    }));
    let _ = tokio::time::timeout(Duration::from_millis(300), f.collector().run(receiver)).await;
    let rows = f.candidates();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].code, diagnostic_codes::AGENT_RUN_PANICKED);
    assert!(!rows.iter().any(|c| c.code.starts_with("RunError:")));
}

#[test]
fn run_error_keys_preserve_prefix_and_utf8_with_a_120_byte_cap() {
    let f = Fixture::new();
    f.collector().run_error_observed_at(
        "run-1",
        &format!("unexpected {}", "日本語🦀".repeat(300)),
        SystemTime::now(),
    );
    let c = f.candidates().remove(0);
    assert!(c.dedup_key.starts_with("run-error:RunError:unclassified:"));
    assert!(c.dedup_key.len() <= 120);
    assert!(c.evidence.len() <= 2048);
    assert!(serde_json::from_str::<Value>(&c.evidence).is_ok());
}

#[test]
fn run_error_daily_limit_is_independent_of_diagnostics() {
    let f = Fixture::new();
    let collector = f.collector();
    for reason in ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"] {
        collector.run_error_observed_at("run-1", reason, SystemTime::now());
    }
    assert_eq!(
        f.candidates().len(),
        storage::improvement::DEFAULT_RUN_ERROR_DAILY_LIMIT as usize
    );
    collector.handle_diagnostic(&diagnostic("NoProgress"));
    collector.run_error_observed_at("run-2", "alpha", SystemTime::now());
    let rows = f.candidates();
    assert_eq!(
        rows.len(),
        storage::improvement::DEFAULT_RUN_ERROR_DAILY_LIMIT as usize + 1
    );
    assert_eq!(
        rows.iter()
            .find(|c| c.dedup_key == "run-error:RunError:unclassified:alpha")
            .unwrap()
            .occurrences,
        2
    );
}

#[test]
fn reopened_candidate_rewrites_and_reattaches_latest_drafts() {
    let mut f = Fixture::new();
    f.settings.policy.duplicate_cooldown_secs = 0;
    let collector = f.collector();
    collector.handle_diagnostic(&diagnostic("NoProgress"));
    let original = f.candidates().remove(0);
    for _ in 0..2 {
        assert!(
            f.settings
                .writer
                .set_improvement_status(&original.id, ImprovementStatus::Dismissed)
                .unwrap()
        );
        for suffix in ["issue.md", "packet.md"] {
            fs::write(
                f.settings
                    .policy
                    .draft_dir
                    .as_ref()
                    .unwrap()
                    .join(format!("{}.{suffix}", original.id)),
                "obsolete draft",
            )
            .unwrap();
        }
        collector.handle_diagnostic(&DiagnosticEvent {
            detail: "latest observation\ntool=read\ncount=7".into(),
            ..diagnostic("NoProgress")
        });
        let c = f.candidates().remove(0);
        assert_eq!(c.id, original.id);
        assert_eq!(c.status, ImprovementStatus::New);
        assert_eq!(c.draft_path, original.draft_path);
        assert!(c.reopened_count > 0);
        for suffix in ["issue.md", "packet.md"] {
            let text = fs::read_to_string(
                f.settings
                    .policy
                    .draft_dir
                    .as_ref()
                    .unwrap()
                    .join(format!("{}.{suffix}", original.id)),
            )
            .unwrap();
            assert!(text.starts_with(drafts::DRAFT_MARKER));
            assert!(text.contains("latest observation"));
            assert!(!text.contains("obsolete draft"));
        }
    }
    assert_eq!(f.draft_files().len(), 2);
    assert_eq!(f.candidates()[0].occurrences, 3);
}

#[test]
fn packet_acceptance_criteria_are_specific_to_the_code() {
    let f = Fixture::new();
    f.collector().handle_diagnostic(&diagnostic("NoProgress"));
    let mut c = f.candidates().remove(0);
    for (code, criterion) in [
        (
            "NoProgress",
            "Reproduce the no-progress loop using evidence.fields",
        ),
        ("IdenticalToolCalls", "Reproduce identical tool calls"),
        ("BudgetExhausted", "Reproduce budget exhaustion"),
        (
            "RunError:storage_busy",
            "Reproduce the run failure using evidence.reason",
        ),
        (
            "RunError:unclassified",
            "Reproduce the run failure using evidence.reason",
        ),
        (
            "FutureCode",
            "Reproduce the observation using evidence.fields",
        ),
    ] {
        c.code = code.into();
        let packet = render_packet_draft(&c);
        assert!(packet.contains(criterion), "{code}: {packet}");
        assert!(!packet.contains("TBD by reviewer"));
    }
}

struct NeverModel;
#[async_trait::async_trait]
impl crate::AgentModel for NeverModel {
    fn selected_model(&self, _: crate::Role, _: Option<&str>) -> String {
        "test".into()
    }
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: crate::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        panic!("passive intake must never invoke a model")
    }
}

fn runtime(bus: Arc<EventBus>) -> crate::AgentRuntime {
    let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    crate::AgentRuntime::new(bus, executor, Arc::new(NeverModel))
}

#[test]
fn defaults_require_explicit_runtime_opt_in() {
    let policy = ImprovementPolicy::default();
    let config = config::SelfImprovementConfig::default();
    assert!(!config.enabled);
    assert_eq!(policy.evidence_max_bytes, 2048);
    assert_eq!(policy.max_candidates, 200);
    assert_eq!(policy.daily_limit, 20);
    assert_eq!(policy.duplicate_cooldown_secs, 86_400);
    assert!(policy.collect_diagnostics && policy.collect_lessons);
    assert!(policy.draft_dir.is_none());
    let resolved = ImprovementPolicy::from_config(&config, Path::new("storage"));
    assert_eq!(
        resolved.draft_dir,
        Some(PathBuf::from("storage/self-improvement/drafts"))
    );
    let runtime = runtime(Arc::new(EventBus::new(16)));
    runtime.start_self_improvement(); // No Tokio context needed for an unset no-op.
    assert_eq!(runtime.ingest_spooled_crashes(), 0);
    assert!(runtime.shared.self_improvement_task.get().is_none());
}

#[tokio::test]
async fn runtime_starts_once_and_observes_diagnostic_and_skill_fault() {
    let f = Fixture::new();
    let bus = Arc::new(EventBus::new(32));
    let runtime = runtime(bus.clone()).with_self_improvement(f.settings.clone());
    let mut replacement = f.settings.clone();
    replacement.project = "must-not-replace".into();
    let runtime = runtime.with_self_improvement(replacement);
    runtime.start_self_improvement();
    runtime.start_self_improvement();
    bus.emit(Event::new(diagnostic("NoProgress")));
    bus.emit(Event::new(FaultEvent::SkillDiagnostic {
        kind: SkillDiagnosticKind::ValidationError,
        skill: "example".into(),
        scope: "project".into(),
        detail: "invalid metadata".into(),
    }));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if f.candidates().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let rows = f.candidates();
    assert!(
        rows.iter()
            .any(|c| c.code == "SkillDiagnostic:ValidationError")
    );
    assert!(rows.iter().all(|c| c.draft_path.is_some()));
    assert_eq!(f.draft_files().len(), 4);
    let weak = Arc::downgrade(&runtime.shared);
    drop(runtime);
    assert!(weak.upgrade().is_none());
    tokio::task::yield_now().await;
}

#[test]
fn runtime_crash_recovery_uses_resolved_directory_and_respects_gate() {
    let mut f = Fixture::new();
    let spool = f
        .settings
        .policy
        .draft_dir
        .as_ref()
        .unwrap()
        .join("crash-spool");
    fs::create_dir_all(&spool).unwrap();
    fs::write(
        spool.join("crash-12.json"),
        json!({
            "message": "recovered panic", "location": null, "thread": null, "timestamp": 12,
        })
        .to_string(),
    )
    .unwrap();
    f.settings.policy.collect_diagnostics = false;
    let disabled = runtime(Arc::new(EventBus::new(16))).with_self_improvement(f.settings.clone());
    assert_eq!(disabled.ingest_spooled_crashes(), 0);
    assert!(spool.join("crash-12.json").exists());
    f.settings.policy.collect_diagnostics = true;
    let enabled = runtime(Arc::new(EventBus::new(16))).with_self_improvement(f.settings.clone());
    enabled.start_self_improvement(); // No Tokio context is a warn-only no-op.
    assert!(enabled.shared.self_improvement_task.get().is_none());
    assert_eq!(enabled.ingest_spooled_crashes(), 1);
    assert_eq!(enabled.ingest_spooled_crashes(), 0);
    assert_eq!(f.candidates()[0].code, "CrashRecovered");
}

#[test]
fn unresolved_drafts_and_closed_writer_fail_without_panicking() {
    let mut f = Fixture::new();
    f.settings.policy.draft_dir = None;
    f.collector().handle_diagnostic(&diagnostic("NoProgress"));
    assert_eq!(f.candidates().len(), 1);
    assert!(f.candidates()[0].draft_path.is_none());
    let collector = f.collector();
    drop(f._storage);
    collector.ingest_lessons(&[lesson()], Some("run-7"));
    collector.handle_diagnostic(&diagnostic("LearningPipelineFailed"));
}

#[test]
fn long_lesson_evidence_is_bounded_without_json_reparsing() {
    let f = Fixture::new();
    let evidence = format!("invalid-json {{ {}", "日本語".repeat(1000));
    f.collector().ingest_lessons(
        &[Lesson {
            evidence: evidence.clone(),
            ..lesson()
        }],
        Some("run-7"),
    );
    let c = f.candidates().remove(0);
    assert!(c.evidence.len() <= 2048);
    let parsed: Value = serde_json::from_str(&c.evidence).unwrap();
    let kept = parsed["evidence_refs"].as_str().unwrap();
    assert!(kept.ends_with("…[truncated]"));
    assert!(evidence.starts_with(kept.trim_end_matches("…[truncated]")));
    assert_eq!(parsed["source_run_id"], "run-7");
}

#[tokio::test]
async fn lag_is_recorded_once_and_the_observer_keeps_running() {
    let f = Fixture::new();
    // Room for the lag fault plus at least one retained diagnostic.
    let bus = EventBus::new(4);
    let receiver = bus.subscribe();
    for _ in 0..20 {
        bus.emit(Event::new(diagnostic("NoProgress")));
    }
    // The observer only ends with the bus; it must outlive the lag.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), f.collector().run(receiver))
            .await
            .is_err()
    );
    let candidates = f.candidates();
    let lagged: Vec<_> = candidates
        .iter()
        .filter(|c| c.code == "ObserverLagged")
        .collect();
    assert_eq!(lagged.len(), 1);
    assert_eq!(lagged[0].occurrences, 1);
    let evidence: Value = serde_json::from_str(&lagged[0].evidence).unwrap();
    assert!(evidence["skipped_events"].as_u64().unwrap() > 0);
    // The diagnostic retained after the lag is still observed.
    assert!(candidates.iter().any(|c| c.code == "NoProgress"));
}

#[tokio::test]
async fn bus_diagnostics_record_their_wall_clock_and_split_by_emitter() {
    let f = Fixture::new();
    let bus = EventBus::new(16);
    let receiver = bus.subscribe();
    let first = Event::new(diagnostic("NoProgress"));
    let observed = first.meta.wall_clock;
    bus.emit(first);
    // The same code from another emitter is a separate candidate, not a duplicate.
    bus.emit(Event::new(DiagnosticEvent {
        source: "tool_calls".into(),
        ..diagnostic("NoProgress")
    }));
    let _ = tokio::time::timeout(Duration::from_millis(300), f.collector().run(receiver)).await;
    let candidates = f.candidates();
    assert_eq!(candidates.len(), 2);
    let original = candidates
        .iter()
        .find(|c| c.dedup_key == format!("diag:{}:NoProgress", diagnostic("NoProgress").source))
        .unwrap();
    let evidence: Value = serde_json::from_str(&original.evidence).unwrap();
    assert_eq!(
        evidence["observed_at_ns"].as_u64().unwrap(),
        unix_ns(observed)
    );
    assert_eq!(evidence["build"], build_info());
    let draft = fs::read_to_string(
        f.settings
            .policy
            .draft_dir
            .as_ref()
            .unwrap()
            .join(original.draft_path.as_ref().unwrap()),
    )
    .unwrap();
    assert!(
        draft.contains(&format!("Recorded by: {}\n", build_info())),
        "{draft}"
    );
}
