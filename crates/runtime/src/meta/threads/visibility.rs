//! Fail-closed visibility; stored conversation text never grants authority.

use std::collections::HashSet;
use std::path::Path;

use storage::threads::{ThreadLineageKind, ThreadRow};

use crate::{AgentRuntime, RunId, RunStore};

/// Trusted invocation identity, constructed from LoopState, never tool arguments.
pub(crate) struct Caller {
    pub(crate) run_id: RunId,
    pub(crate) thread_id: Option<String>,
    pub(crate) project_root: Option<String>,
}

impl Caller {
    /// Pass `state.caller_run_id()`, the run's thread and the run config's
    /// project root. An uncanonicalizable root grants no project visibility.
    pub(crate) fn new(
        run_id: RunId,
        thread_id: Option<String>,
        project_root: Option<&Path>,
    ) -> Self {
        Self {
            run_id,
            thread_id,
            project_root: project_root
                .and_then(|root| root.canonicalize().ok())
                .map(|root| root.to_string_lossy().into_owned()),
        }
    }
}

/// A tool's `thread` argument: a syntactically valid run ID names that exact run.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Target<'a> {
    Thread(&'a str),
    Run(&'a str),
}

impl<'a> Target<'a> {
    /// Classify without canonicalizing the user's spelling (also used in denials).
    pub(crate) fn parse(id: &'a str) -> Self {
        if id.parse::<RunId>().is_ok() {
            Self::Run(id)
        } else {
            Self::Thread(id)
        }
    }

    fn id(self) -> &'a str {
        match self {
            Self::Thread(id) | Self::Run(id) => id,
        }
    }
}

/// The only denial text: identical whether the target is missing or hidden.
pub(crate) fn denied(id: &str) -> String {
    format!(
        "thread_denied: {id} is not visible from this run; a thread outside your project becomes readable only when the user names its ID in this conversation"
    )
}

/// Resolve a thread only after checking ADR 0041's ordered visibility rules.
/// Missing, unauthorized and corrupt/unreadable storage all fail with the same denial.
pub(crate) fn resolve(
    store: &RunStore,
    runtime: &AgentRuntime,
    caller: &Caller,
    target: Target<'_>,
) -> Result<ThreadRow, String> {
    resolve_inner(store, runtime, caller, target)
        .ok()
        .flatten()
        .ok_or_else(|| denied(target.id()))
}

fn resolve_inner(
    store: &RunStore,
    runtime: &AgentRuntime,
    caller: &Caller,
    target: Target<'_>,
) -> Result<Option<ThreadRow>, storage::StorageError> {
    let target_thread = match target {
        Target::Thread(id) => store.thread(id)?,
        Target::Run(id) => match store.thread_of_run(id)? {
            Some(binding) => store.thread(&binding.thread_id)?,
            None => None,
        },
    };
    let Some(row) = target_thread else {
        return Ok(None);
    };
    // 1. The caller's own conversation.
    if caller.thread_id.as_deref() == Some(&row.thread_id) {
        return Ok(Some(row));
    }
    // 2. The same canonical project; an unknown root on either side never matches.
    if caller
        .project_root
        .as_ref()
        .zip(row.project_root.as_ref())
        .is_some_and(|(caller, target)| caller == target)
    {
        return Ok(Some(row));
    }
    // CLI roots have no conversation: even their own initial prompt grants nothing.
    let Some(thread_id) = &caller.thread_id else {
        return Ok(None);
    };
    // 3. Lineage: the fork/rewind/escalation parent, and escalations in both directions.
    let caller_thread = store.thread(thread_id)?;
    if caller_thread
        .as_ref()
        .is_some_and(|thread| thread.parent_thread_id.as_deref() == Some(&row.thread_id))
        || (row.lineage_kind == Some(ThreadLineageKind::Escalation)
            && row.parent_thread_id.as_ref() == Some(thread_id))
    {
        return Ok(Some(row));
    }
    let thread_runs = store.runs_of_thread(thread_id)?;
    if let Some(source) = &row.escalation_source_run_id {
        let in_thread = source == &caller.run_id.to_string()
            || thread_runs.iter().any(|run| &run.run_id == source)
            || store
                .thread_of_run(source)?
                .is_some_and(|run| run.thread_id == *thread_id)
            || source
                .parse()
                .ok()
                .and_then(|source| runtime.thread_of_live_run(source))
                .as_ref()
                == Some(thread_id);
        if in_thread {
            return Ok(Some(row));
        }
    }
    if let Some(source) = caller_thread
        .as_ref()
        .and_then(|thread| thread.escalation_source_run_id.as_ref())
        && store
            .thread_of_run(source)?
            .is_some_and(|run| run.thread_id == row.thread_id)
    {
        return Ok(Some(row));
    }
    // 4. IDs a human named in this conversation's trusted evidence.
    let mut evidence_runs: HashSet<String> =
        thread_runs.into_iter().map(|run| run.run_id).collect();
    let mut current = caller.run_id;
    let root = {
        let runs = crate::runtime::lock_runs(&runtime.shared.runs);
        let mut seen = HashSet::new();
        let mut root = None;
        for _ in 0..=64 {
            if !seen.insert(current) {
                break;
            }
            evidence_runs.insert(current.to_string());
            match runs.get(&current) {
                Some(entry) => match entry.parent {
                    Some(parent) => current = parent,
                    None => {
                        root = Some(current);
                        break;
                    }
                },
                None => break,
            }
        }
        root
    };
    evidence_runs.insert(caller.run_id.to_string());
    evidence_runs.extend(store.review_lineage_ancestors(&caller.run_id.to_string())?);
    let matches = |text: &str| {
        contains_id(text, &row.thread_id)
            || row
                .root_run_id
                .as_ref()
                .is_some_and(|id| contains_id(text, id))
            || matches!(target, Target::Run(id) if contains_id(text, id))
    };
    if root
        .and_then(|root| runtime.trusted_thread_request(root))
        .is_some_and(|text| matches(&text))
    {
        return Ok(Some(row));
    }
    let ids: Vec<_> = evidence_runs.into_iter().collect();
    let evidence = store.review_evidence_for_lineage(&ids)?;
    // Working notes, tool results and agent messages are deliberately not queried.
    if evidence.iter().any(|item| {
        matches!(
            item.kind.as_str(),
            "initial_prompt" | "host_chat" | "user_answer"
        ) && matches(&item.text)
    }) {
        return Ok(Some(row));
    }
    Ok(None)
}

/// Whole-token occurrence: no prefix, suffix or partial ID matches.
fn contains_id(text: &str, id: &str) -> bool {
    fn token(character: char) -> bool {
        character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
    }
    !id.is_empty()
        && text.match_indices(id).any(|(offset, _)| {
            !text[..offset].chars().next_back().is_some_and(token)
                && !text[offset + id.len()..].chars().next().is_some_and(token)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentModel, AgentRuntime, Role, RunConfig, RuntimeError};
    use providers::{ChatResponse, ContentBlock, FinishReason, Message, Usage};
    use std::sync::Arc;
    use storage::threads::ThreadUpsert;

    struct Model;
    #[async_trait::async_trait]
    impl AgentModel for Model {
        async fn complete(
            &self,
            _: &crate::AgentInvocationContext,
            _: Role,
            _: &[Message],
            _: &[providers::ToolSpec],
        ) -> Result<ChatResponse, RuntimeError> {
            Ok(ChatResponse {
                usage_reported: true,
                message: Message {
                    role: providers::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                },
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "test".into()
        }
    }

    struct Fixture {
        runtime: AgentRuntime,
        _storage: storage::Storage,
        dir: tempfile::TempDir,
        other: tempfile::TempDir,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            let config = storage::StorageConfig {
                db_path: dir.path().join("threads.db"),
                ..Default::default()
            };
            let storage = storage::Storage::open(config.clone()).unwrap();
            let bus = Arc::new(event_bus::EventBus::new(128));
            let runtime = AgentRuntime::new(
                bus.clone(),
                Arc::new(tools::ToolExecutor::new(bus)),
                Arc::new(Model),
            )
            .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
            Self {
                runtime,
                _storage: storage,
                dir,
                other,
            }
        }
        fn store(&self) -> &RunStore {
            self.runtime.shared.run_store.get().unwrap()
        }
        fn row(&self, id: &str, root: Option<&Path>) {
            self.rooted(id, root, "run-900");
        }
        fn rooted(&self, id: &str, root: Option<&Path>, run: &str) {
            self.store()
                .handle
                .upsert_thread_runtime(&ThreadUpsert {
                    thread_id: id.into(),
                    project_root: root
                        .map(|p| p.canonicalize().unwrap().to_string_lossy().into_owned()),
                    root_run_id: Some(run.into()),
                    ..Default::default()
                })
                .unwrap();
        }
        async fn caller(&self, text: &str, thread: bool) -> Caller {
            let config = RunConfig {
                project_root: Some(self.dir.path().into()),
                ..Default::default()
            };
            let run = if thread {
                self.runtime
                    .delegate_chat("caller", Role::Worker, text.into(), config)
                    .unwrap()
            } else {
                self.runtime
                    .delegate_background(Role::Worker, text.into(), config)
            };
            self.runtime.wait(run).await.unwrap();
            Caller::new(
                run,
                if thread { Some("caller".into()) } else { None },
                Some(self.dir.path()),
            )
        }
        fn resolve(&self, caller: &Caller, id: &str) -> Result<ThreadRow, String> {
            resolve(self.store(), &self.runtime, caller, Target::parse(id))
        }
    }

    #[tokio::test]
    async fn projects_unknowns_and_denial_are_fail_closed() {
        let f = Fixture::new();
        let caller = f.caller("hello", true).await;
        f.row("other", Some(f.other.path()));
        let expected = "thread_denied: other is not visible from this run; a thread outside your project becomes readable only when the user names its ID in this conversation";
        assert_eq!(f.resolve(&caller, "other").unwrap_err(), expected);
        assert_eq!(
            f.resolve(&caller, "missing").unwrap_err(),
            expected.replace("other is", "missing is")
        );
        f.row("same", Some(f.dir.path()));
        assert!(f.resolve(&caller, "same").is_ok());
        f.row("unknown", None);
        assert!(f.resolve(&caller, "unknown").is_err());
        assert!(f.resolve(&caller, "caller").is_ok());
        let null_caller = Caller::new(caller.run_id, caller.thread_id.clone(), None);
        assert!(f.resolve(&null_caller, "unknown").is_err());
    }

    #[tokio::test]
    async fn only_human_exact_tokens_grant_cross_project_visibility() {
        let f = Fixture::new();
        let caller = f.caller("hello", true).await;
        f.row("target", Some(f.other.path()));
        // Notes and assistant tool/response text are deliberately not queried.
        f.store()
            .handle
            .append_run_ledger(&caller.run_id.to_string(), "target")
            .unwrap();
        assert!(f.resolve(&caller, "target").is_err());
        for text in ["target-suffix", "prefix_target", "target0", "xtarget"] {
            let row = crate::run_store::evidence_row(
                &caller.run_id.to_string(),
                &caller.run_id.to_string(),
                "host_chat",
                text,
            );
            f.store()
                .handle
                .append_review_evidence_with_reference_consumption(&[row], &[], None)
                .unwrap();
            assert!(f.resolve(&caller, "target").is_err(), "{text}");
        }
        let row = crate::run_store::evidence_row(
            &caller.run_id.to_string(),
            &caller.run_id.to_string(),
            "host_chat",
            "read (target), please",
        );
        f.store()
            .handle
            .append_review_evidence_with_reference_consumption(&[row], &[], None)
            .unwrap();
        assert!(f.resolve(&caller, "target").is_ok());
        f.row("trusted", None);
        f.runtime.remember_thread_request(caller.run_id, "trusted");
        assert!(f.resolve(&caller, "trusted").is_ok());
    }

    #[tokio::test]
    async fn trusted_requests_never_authorize_a_truncated_id_prefix() {
        let f = Fixture::new();
        let caller = f.caller("hello", true).await;
        f.row("thread-1", Some(f.other.path()));
        f.row("thread-10", Some(f.other.path()));
        // 8192 would end in thread-1, and the goal continuation's original
        // and latest display slices would do the same at 4096 and 4064.
        for boundary in [8192, 4096, 4064] {
            let text = format!("{} thread-10", "a".repeat(boundary - 9));
            f.runtime.remember_thread_request(caller.run_id, &text);
            assert!(f.resolve(&caller, "thread-1").is_err(), "{boundary}");
            assert!(f.resolve(&caller, "thread-10").is_ok(), "{boundary}");
            f.runtime.goal_user_input(caller.run_id, &text);
            assert!(f.resolve(&caller, "thread-1").is_err(), "{boundary}");
            assert!(f.resolve(&caller, "thread-10").is_ok(), "{boundary}");
            for combined in [
                crate::thread_goals::continued_thread_request(&text, "latest"),
                crate::thread_goals::continued_thread_request("original", &text),
            ] {
                f.runtime.remember_thread_request(caller.run_id, &combined);
                assert!(f.resolve(&caller, "thread-1").is_err(), "{boundary}");
                assert!(f.resolve(&caller, "thread-10").is_ok(), "{boundary}");
            }
        }
    }

    #[tokio::test]
    async fn fresh_restore_does_not_promote_legacy_truncated_pending_requests() {
        for legacy in [true, false] {
            for followup in [None, Some("read thread-20")] {
                let f = Fixture::new();
                let original = format!("{} thread-10", "a".repeat(8183));
                let source = f.caller(&original, true).await.run_id;
                for id in ["thread-1", "thread-10", "thread-20"] {
                    f.row(id, Some(f.other.path()));
                }
                let target = f.runtime.reserve_run_id();
                crate::restore::persist_escalation_seed(
                    &f.runtime,
                    target,
                    source,
                    "worker memo",
                    &RunConfig::default(),
                    false,
                    &Default::default(),
                    None,
                )
                .unwrap();
                let mut saved = f.store().restore_record(target).unwrap().unwrap();
                if legacy {
                    let mut config: serde_json::Value =
                        serde_json::from_str(&saved.config_json).unwrap();
                    let pending = config["pending_escalation"].as_object_mut().unwrap();
                    pending.remove("trusted_request_complete");
                    pending.insert(
                        "trusted_request".into(),
                        serde_json::json!(original.chars().take(8192).collect::<String>()),
                    );
                    saved.config_json = config.to_string();
                    f.store().handle.upsert_run_context(&saved).unwrap();
                }
                let config = storage::StorageConfig {
                    db_path: f.dir.path().join("threads.db"),
                    ..Default::default()
                };
                let bus = Arc::new(event_bus::EventBus::new(128));
                let fresh = AgentRuntime::new(
                    bus.clone(),
                    Arc::new(tools::ToolExecutor::new(bus)),
                    Arc::new(Model),
                )
                .with_run_store(RunStore::open(&config, f._storage.handle()).unwrap());
                if let Some(prompt) = followup {
                    fresh
                        .continue_goal(target, prompt.into(), RunConfig::default())
                        .unwrap();
                } else {
                    fresh.resume_chat(target, RunConfig::default()).unwrap();
                }
                let saved = fresh
                    .shared
                    .run_store
                    .get()
                    .unwrap()
                    .restore_record(target)
                    .unwrap()
                    .unwrap();
                let descriptor: crate::restore::RunRestoreDescriptor =
                    serde_json::from_str(&saved.config_json).unwrap();
                let pending = descriptor.pending_escalation.unwrap();
                assert_eq!(
                    pending.trusted_request_complete,
                    !legacy || followup.is_some()
                );
                fresh.wait_admission(target).await.unwrap();
                let expected = match (legacy, followup) {
                    (true, None) => None,
                    (true, Some(prompt)) => Some(prompt.to_owned()),
                    (false, None) => Some(original.clone()),
                    (false, Some(prompt)) => Some(crate::thread_goals::continued_thread_request(
                        &original, prompt,
                    )),
                };
                assert_eq!(fresh.trusted_thread_request(target), expected);
                let caller = Caller::new(
                    target,
                    Some(event_bus::escalation_thread_id(&target.to_string())),
                    None,
                );
                let store = fresh.shared.run_store.get().unwrap();
                assert_eq!(
                    resolve(store, &fresh, &caller, Target::Thread("thread-1")).unwrap_err(),
                    denied("thread-1")
                );
                assert_eq!(
                    resolve(store, &fresh, &caller, Target::Thread("thread-20")).is_ok(),
                    followup.is_some()
                );
                fresh.stop(target, crate::StopScope::SelfOnly).unwrap();
                fresh.wait(target).await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn escalation_both_directions_and_fork_parent_are_visible() {
        let f = Fixture::new();
        let caller = f.caller("hello", true).await;
        f.store()
            .handle
            .upsert_thread_runtime(&ThreadUpsert {
                thread_id: "escalated".into(),
                parent_thread_id: Some("caller".into()),
                lineage_kind: Some(ThreadLineageKind::Escalation),
                ..Default::default()
            })
            .unwrap();
        assert!(f.resolve(&caller, "escalated").is_ok());
        let child = Caller::new(caller.run_id, Some("escalated".into()), None);
        assert!(f.resolve(&child, "caller").is_ok());
        f.store()
            .handle
            .upsert_thread_runtime(&ThreadUpsert {
                thread_id: "source-only".into(),
                escalation_source_run_id: Some(caller.run_id.to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(f.resolve(&caller, "source-only").is_ok());
        let source_only = Caller::new(caller.run_id, Some("source-only".into()), None);
        assert!(
            f.resolve(&source_only, "caller").is_ok(),
            "an escalation without a recorded parent still sees its source thread"
        );
        f.store()
            .handle
            .upsert_thread_runtime(&ThreadUpsert {
                thread_id: "fork".into(),
                parent_thread_id: Some("caller".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            f.resolve(&caller, "fork").is_err(),
            "fork visibility is only towards its parent"
        );
        let fork = Caller::new(caller.run_id, Some("fork".into()), None);
        assert!(f.resolve(&fork, "caller").is_ok());
    }

    #[tokio::test]
    async fn cli_without_thread_sees_only_project_even_with_human_evidence() {
        let f = Fixture::new();
        // The initial prompt names both the foreign thread and its root run.
        let caller = f.caller("outside run-901", false).await;
        f.rooted("outside", Some(f.other.path()), "run-901");
        f.rooted("inside", Some(f.dir.path()), "run-902");
        f.runtime.remember_thread_request(caller.run_id, "outside");
        assert!(f.resolve(&caller, "outside").is_err());
        assert!(f.resolve(&caller, "run-901").is_err());
        assert!(f.resolve(&caller, "inside").is_ok());
        assert!(f.resolve(&caller, "run-902").is_ok());
    }

    #[test]
    fn token_boundaries_and_target_classification() {
        for text in ["target", "(target)", "日本語target。", "\ntarget\n"] {
            assert!(contains_id(text, "target"));
        }
        for text in ["target-suffix", "target_", "0target", "targetz", "targett"] {
            assert!(!contains_id(text, "target"));
        }
        assert!(!contains_id("anything", ""));
        assert!(matches!(Target::parse("run-1"), Target::Run("run-1")));
        assert!(matches!(
            Target::parse("thread-1"),
            Target::Thread("thread-1")
        ));
    }
}
