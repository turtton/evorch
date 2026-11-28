//! Thread identity from the runtime's trusted root/delegation binding, not an owner token.
use crate::{AgentRuntime, RunId, runtime::RunEntry};
use std::collections::{HashMap, HashSet};

/// Identity only: this never grants mutation or execution authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrustedThreadContext(String);

impl TrustedThreadContext {
    pub(crate) fn thread_id(&self) -> &str {
        &self.0
    }
}

impl AgentRuntime {
    pub(crate) fn trusted_thread_context(&self, run: RunId) -> Option<TrustedThreadContext> {
        self.thread_of_live_run(run).map(TrustedThreadContext)
    }

    /// Registration can precede insertion into runs. The parent comes from the
    /// admitted/restored runtime entry; callers already holding runs use this form.
    pub(crate) fn registration_thread_context(
        &self,
        run: RunId,
        parent: Option<RunId>,
        runs: &HashMap<RunId, RunEntry>,
    ) -> Option<TrustedThreadContext> {
        let mut current = run;
        let mut parent = parent;
        let mut seen = HashSet::new();
        for _ in 0..=64 {
            if !seen.insert(current) {
                return None;
            }
            match parent {
                Some(next) => {
                    current = next;
                    parent = runs.get(&current)?.parent;
                }
                None => return self.goal_thread(current).map(TrustedThreadContext),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentInvocationContext, AgentModel, Role, RunConfig, RuntimeError};
    use std::sync::Arc;

    struct Model;
    #[async_trait::async_trait]
    impl AgentModel for Model {
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "test".into()
        }
        async fn complete(
            &self,
            _: &AgentInvocationContext,
            _: Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, RuntimeError> {
            Ok(providers::ChatResponse {
                usage_reported: true,
                message: providers::Message {
                    role: providers::Role::Assistant,
                    content: vec![providers::ContentBlock::Text {
                        text: "done".into(),
                    }],
                },
                finish_reason: providers::FinishReason::Stop,
                usage: providers::Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn identity_follows_bound_root_and_delegation_not_display_names_or_permits() {
        let bus = Arc::new(event_bus::EventBus::new(128));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            Arc::new(Model),
        );
        let root = runtime.reserve_run_id();
        runtime.bind_thread_root("trusted", root).unwrap();
        runtime
            .spawn_reserved(root, None, Role::Worker, "root", RunConfig::default())
            .unwrap();
        runtime.wait(root).await.unwrap();
        let child = runtime.reserve_run_id();
        runtime
            .spawn_reserved(
                child,
                Some(root),
                Role::Worker,
                "child",
                RunConfig {
                    name: Some("chat:Worker:untrusted".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        runtime.wait(child).await.unwrap();
        assert_eq!(
            runtime.trusted_thread_context(child).unwrap().thread_id(),
            "trusted"
        );
        let unbound = runtime.delegate_background(
            Role::Worker,
            "unbound".into(),
            RunConfig {
                name: Some("chat:Worker:trusted".into()),
                ..Default::default()
            },
        );
        runtime.wait(unbound).await.unwrap();
        assert!(runtime.trusted_thread_context(unbound).is_none());
        assert!(
            runtime
                .trusted_thread_context(RunId::new(u64::MAX))
                .is_none()
        );
    }

    #[tokio::test]
    async fn deleted_bound_thread_blocks_unnamed_root_and_child_before_registration() {
        for child in [false, true] {
            let bus = Arc::new(event_bus::EventBus::new(128));
            let runtime = AgentRuntime::new(
                bus.clone(),
                Arc::new(tools::ToolExecutor::new(bus)),
                Arc::new(Model),
            );
            let root = runtime.reserve_run_id();
            runtime.bind_thread_root("deleted", root).unwrap();
            if child {
                runtime
                    .spawn_reserved(root, None, Role::Worker, "root", RunConfig::default())
                    .unwrap();
                runtime.wait(root).await.unwrap();
            }
            runtime
                .set_tombstone_guard(crate::thread_retention::TombstoneGuard::new(
                    storage::thread_retention::TombstoneSet {
                        threads: HashSet::from(["deleted".to_owned()]),
                        ..Default::default()
                    },
                ))
                .unwrap();
            let run = if child {
                runtime.reserve_run_id()
            } else {
                root
            };
            assert!(matches!(
                runtime.spawn_reserved(
                    run,
                    child.then_some(root),
                    Role::Worker,
                    "must not start",
                    RunConfig::default(),
                ),
                Err(RuntimeError::ThreadDeleted(
                    crate::thread_retention::ThreadDeleted::Thread(thread)
                )) if thread == "deleted"
            ));
            assert!(runtime.inspect_agent(run).is_err());
        }
    }
}
