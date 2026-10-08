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
        ("NoProgress", HarnessImprovement),
        ("LearningPipelineFailed", HarnessImprovement),
        ("ContextSnapshotFailed", HarnessImprovement),
        ("EscalationHandoffFailed", HarnessImprovement),
        ("CrashRecovered", HarnessImprovement),
        ("BudgetWarning", TransientOrExternal),
        ("BudgetExhausted", TransientOrExternal),
        ("ProviderUnavailable", TransientOrExternal),
        ("CacheRegression", TransientOrExternal),
        ("ContextCheckpointSaved", Ignored),
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
        ("CompactionFailed", Ignored),
        ("future-code", Ignored),
    ];
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
    assert!(packet.contains("TBD by reviewer"));
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
        ])
        .env(CHILD, &spool)
        .output()
        .unwrap();
    assert!(output.status.success());
    let crashes = drain_crash_spool(&spool);
    assert_eq!(crashes.len(), 1);
    assert!(crashes[0].message.len() <= 4096);
    assert!(crashes[0].message.ends_with("…[truncated]"));
    assert!(crashes[0].location.is_some());
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
