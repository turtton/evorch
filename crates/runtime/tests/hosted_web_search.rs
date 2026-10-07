//! Offline runtime contracts for invocation isolation, gates and usage sidecars.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use event_bus::{AgentRunPhase, EventBus, ThreadGoalPhase};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, ModelPreference, Role, RunConfig,
    RuntimeError,
};
use serde_json::json;
use tokio::sync::{Barrier, Notify};
use tools::{SearchError, SearchOptions, SearchProvider, SearchResults, ToolExecutor, WebSearch};

struct SearchModel {
    calls: AtomicUsize,
    started: Notify,
    release: Notify,
    lookups: Mutex<Vec<AgentInvocationContext>>,
    search_count: Arc<AtomicUsize>,
    fail: bool,
    completed: Option<Arc<Barrier>>,
}

impl SearchModel {
    fn new(fail: bool, completed: Option<Arc<Barrier>>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            started: Notify::new(),
            release: Notify::new(),
            lookups: Mutex::new(Vec::new()),
            search_count: Arc::new(AtomicUsize::new(0)),
            fail,
            completed,
        })
    }
}

struct Search {
    sink: Arc<dyn Fn(Usage) + Send + Sync>,
    calls: Arc<AtomicUsize>,
    fail: bool,
    completed: Option<Arc<Barrier>>,
}

#[async_trait::async_trait]
impl SearchProvider for Search {
    fn name(&self) -> &str {
        "codex"
    }
    async fn search(&self, _: &str, _: &SearchOptions) -> Result<SearchResults, SearchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let usage = Usage {
            input_tokens: 21,
            output_tokens: 4,
            ..Usage::default()
        };
        (self.sink)(usage);
        if let Some(completed) = &self.completed {
            completed.wait().await;
            std::future::pending::<()>().await;
        }
        if self.fail {
            return Err(SearchError::Protocol(
                "completed response has no citations".into(),
            ));
        }
        Ok(SearchResults {
            content: "Search result".into(),
            result_count: 1,
            request_id: Some("search-id".into()),
            usage: Some(json!(usage)),
        })
    }
}

struct UnexpectedSearch;
#[async_trait::async_trait]
impl SearchProvider for UnexpectedSearch {
    fn name(&self) -> &str {
        "unexpected"
    }
    async fn search(&self, _: &str, _: &SearchOptions) -> Result<SearchResults, SearchError> {
        panic!("legacy search or fallback must not execute")
    }
}

#[async_trait::async_trait]
impl AgentModel for SearchModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
    fn web_search_provider(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        _: &[ToolSpec],
        sink: Arc<dyn Fn(Usage) + Send + Sync>,
    ) -> Result<Option<Arc<dyn SearchProvider>>, RuntimeError> {
        self.lookups.lock().unwrap().push(invocation.clone());
        Ok(Some(Arc::new(Search {
            sink,
            calls: self.search_count.clone(),
            fail: self.fail,
            completed: self.completed.clone(),
        })))
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        if first {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: if first {
                    (0..2)
                        .map(|index| ContentBlock::ToolUse {
                            id: format!("search-{index}"),
                            name: "web_search".into(),
                            input: json!({"query":format!("query {index}")}),
                        })
                        .collect()
                } else {
                    vec![ContentBlock::Text {
                        text: "done".into(),
                    }]
                },
            },
            finish_reason: if first {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                ..Usage::default()
            },
        })
    }
}

fn runtime(model: Arc<dyn AgentModel>, enabled: bool, deny: bool) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(256));
    let mut executor = ToolExecutor::new(bus.clone());
    executor
        .register(Arc::new(
            WebSearch::for_providers(Arc::new(UnexpectedSearch), Arc::new(UnexpectedSearch))
                .with_hosted_search_fallback(Arc::new(UnexpectedSearch)),
        ))
        .unwrap();
    if deny {
        executor.set_policy(
            sandbox::ApprovalPolicy::allow_all()
                .with_override("web_search", sandbox::PolicyDecision::Deny),
        );
    }
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model);
    runtime.set_web_tools_enabled(enabled);
    runtime
}

#[tokio::test]
async fn denied_web_global_role_and_tool_gates_never_resolve_or_execute_hosted_search() {
    for (enabled, role, deny) in [
        (false, Role::WebResearcher, false),
        (true, Role::Worker, false),
        (true, Role::WebResearcher, true),
    ] {
        let model = SearchModel::new(false, None);
        let runtime = runtime(model.clone(), enabled, deny);
        let run = runtime.delegate_background(role, "query".into(), RunConfig::default());
        model.started.notified().await;
        model.release.notify_one();
        assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
        assert!(model.lookups.lock().unwrap().is_empty());
        assert_eq!(model.search_count.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn parallel_completed_search_usage_survives_success_errors_and_exhaustion() {
    for fail in [false, true] {
        let model = SearchModel::new(fail, None);
        let runtime = runtime(model.clone(), true, false);
        let run = runtime.delegate_background(
            Role::WebResearcher,
            "query".into(),
            RunConfig {
                budget: runtime::budget_tracker::BudgetSettings {
                    max_tokens: Some(30),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        model.started.notified().await;
        runtime.bind_thread_root("thread", run).unwrap();
        let mut goal = runtime
            .create_thread_goal(
                "thread",
                run,
                "Research the query".into(),
                vec!["Return supported evidence".into()],
            )
            .unwrap();
        goal.max_tokens = Some(30);
        runtime.restore_thread_goal(goal).unwrap();
        model.release.notify_one();
        assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
        assert_eq!(model.search_count.load(Ordering::SeqCst), 2);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        let goal = runtime.thread_goal("thread").unwrap();
        assert_eq!(goal.usage.input_tokens, 43);
        assert_eq!(goal.usage.output_tokens, 9);
        assert_eq!(goal.phase, ThreadGoalPhase::Blocked);
    }
}

#[tokio::test]
async fn cancellation_after_completed_usage_preserves_every_parallel_call() {
    let completed = Arc::new(Barrier::new(3));
    let model = SearchModel::new(false, Some(completed.clone()));
    let runtime = runtime(model.clone(), true, false);
    let run =
        runtime.delegate_background(Role::WebResearcher, "query".into(), RunConfig::default());
    model.started.notified().await;
    runtime.bind_thread_root("thread", run).unwrap();
    runtime
        .create_thread_goal(
            "thread",
            run,
            "Research the query".into(),
            vec!["Return supported evidence".into()],
        )
        .unwrap();
    model.release.notify_one();
    completed.wait().await;
    runtime.cancel(run).unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
    let goal = runtime.thread_goal("thread").unwrap();
    assert_eq!(goal.usage.input_tokens, 43);
    assert_eq!(goal.usage.output_tokens, 9);
}

#[tokio::test]
async fn streaming_preference_and_project_changes_apply_only_after_the_tool_wave() {
    let old = SearchModel::new(false, None);
    let replacement = SearchModel::new(false, None);
    // The replacement should provide the final response immediately.
    replacement.calls.store(1, Ordering::SeqCst);
    let switchable = Arc::new(runtime::compose::SwitchableModel::new(old.clone()));
    let runtime = runtime(switchable.clone(), true, false);
    let old_preference = ModelPreference {
        profile: "old-account".into(),
        model: Some("old-model".into()),
        reasoning_effort: Some("low".into()),
    };
    let run = runtime.delegate_background(
        Role::WebResearcher,
        "query".into(),
        RunConfig {
            model_preference: Some(old_preference.clone()),
            ..Default::default()
        },
    );
    old.started.notified().await;
    runtime
        .set_model_preference(
            run,
            Some(ModelPreference {
                profile: "new-account".into(),
                model: Some("new-model".into()),
                reasoning_effort: Some("high".into()),
            }),
        )
        .unwrap();
    switchable.replace(replacement.clone());
    old.release.notify_one();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(old.search_count.load(Ordering::SeqCst), 2);
    assert!(
        old.lookups
            .lock()
            .unwrap()
            .iter()
            .all(|invocation| invocation.model_preference == Some(old_preference.clone()))
    );
    assert!(replacement.lookups.lock().unwrap().is_empty());
    assert_eq!(replacement.calls.load(Ordering::SeqCst), 2);
}
