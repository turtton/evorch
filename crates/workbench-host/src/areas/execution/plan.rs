//! Projection-backed reads. Invalidation events carry no replaceable plan state.
use super::*;
use serde::{Serialize, de::DeserializeOwned};
use workbench_protocol::namespaces::execution::plan::*;

fn wire<T: DeserializeOwned>(value: impl Serialize) -> Result<T, WorkbenchError> {
    serde_json::to_value(value)
        .and_then(serde_json::from_value)
        .map_err(|_| {
            error(
                ErrorCode::StorageUnavailable,
                "plan view could not be represented",
            )
        })
}
fn plan_error(e: storage::StorageError) -> WorkbenchError {
    use storage::conversation_plan::PlanRejection;
    match e {
        storage::StorageError::ConversationPlan(PlanRejection::StaleCursor { .. }) => {
            error(ErrorCode::Conflict, "plan changed; restart paging")
        }
        storage::StorageError::ConversationPlan(PlanRejection::InvalidCursor) => {
            error(ErrorCode::InvalidRequest, "invalid plan cursor")
        }
        storage::StorageError::ConversationPlan(
            PlanRejection::UnknownTask(_) | PlanRejection::ForeignTask(_),
        ) => error(ErrorCode::NotFound, "unknown task in this conversation"),
        storage::StorageError::Tombstoned { .. } => {
            error(ErrorCode::NotFound, "conversation was deleted")
        }
        other => storage_error(other),
    }
}
pub(super) fn query(
    runtime: &AgentRuntime,
    q: &ExecutionQuery,
) -> Result<Option<ExecutionReply>, WorkbenchError> {
    let reply = match q {
        ExecutionQuery::GetConversationPlan(q) => {
            let p = runtime
                .conversation_plan_summary(q.thread.as_str())
                .map_err(plan_error)?;
            ExecutionReply::ConversationPlan(Box::new(PlanSummary {
                thread: q.thread.clone(),
                plan_id: p.plan_id,
                revision: Revision(p.revision),
                progress: wire(p.progress)?,
            }))
        }
        ExecutionQuery::ReadConversationPlanTree(q) => {
            let p = runtime
                .conversation_plan_tree(q.thread.as_str(), q.cursor.as_deref(), q.limit)
                .map_err(plan_error)?;
            ExecutionReply::ConversationPlanTree(Box::new(PlanTreePage {
                plan_id: p.plan_id,
                revision: Revision(p.revision),
                progress: wire(p.progress)?,
                nodes: wire(p.nodes)?,
                next_cursor: p.next_cursor,
            }))
        }
        ExecutionQuery::GetConversationPlanTask(q) => {
            let p = runtime
                .conversation_plan_task(q.thread.as_str(), &q.task_id)
                .map_err(plan_error)?;
            ExecutionReply::ConversationPlanTask(Box::new(PlanTaskRead {
                plan_id: p.plan_id,
                revision: Revision(p.revision),
                node: wire(p.node)?,
            }))
        }
        ExecutionQuery::ReadConversationPlanHistory(q) => {
            let p = runtime
                .conversation_plan_history(
                    q.thread.as_str(),
                    q.after_revision.map(|r| r.0),
                    q.limit,
                )
                .map_err(plan_error)?;
            let entries = p
                .entries
                .into_iter()
                .map(|e| {
                    let change = e
                        .event
                        .map(|event| {
                            let event_bus::ConversationPlanEvent::Changed {
                                cause,
                                source_run_id,
                                changes,
                                progress,
                                ..
                            } = event;
                            Ok::<_, WorkbenchError>(PlanHistoryChange {
                                cause: wire(cause)?,
                                source_run_id,
                                changes: wire(changes)?,
                                progress: wire(progress)?,
                            })
                        })
                        .transpose()?;
                    Ok(PlanHistoryEntry {
                        revision: Revision(e.revision),
                        event_id: Sequence(u64::try_from(e.event_id).map_err(|_| {
                            error(ErrorCode::StorageUnavailable, "invalid saved plan event id")
                        })?),
                        change,
                    })
                })
                .collect::<Result<Vec<_>, WorkbenchError>>()?;
            ExecutionReply::ConversationPlanHistory(Box::new(PlanHistoryPage {
                plan_id: p.plan_id,
                revision: Revision(p.revision),
                entries,
                next_after_revision: p.next_after_revision.map(Revision),
            }))
        }
        _ => return Ok(None),
    };
    reply
        .validate(&workbench_protocol::handshake::WireLimits::default())
        .map_err(WorkbenchError::from)?;
    Ok(Some(reply))
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};
    use std::sync::Arc;
    use workbench_protocol::events::{Subscribe, SubscriptionScope};

    struct NoModel;
    #[async_trait::async_trait]
    impl AgentModel for NoModel {
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "unused".into()
        }
        async fn complete(
            &self,
            _: &AgentInvocationContext,
            _: Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, RuntimeError> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn saved_change_publishes_only_thread_revision_invalidation() {
        let bus = Arc::new(event_bus::EventBus::new(32));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            Arc::new(NoModel),
        );
        let projection = crate::projection::Projection::new(
            runtime.clone(),
            &tokio::runtime::Handle::current(),
            Default::default(),
        );
        let source = projection.source("execution").unwrap();
        let mut subscription = projection
            .subscribe_with_snapshot(&Subscribe {
                scope: SubscriptionScope::All,
                after: None,
            })
            .unwrap();
        for revision in [2, 1, 1] {
            let changed = EventKind::ConversationPlan(event_bus::ConversationPlanEvent::Changed {
                plan_id: "plan".into(),
                thread_id: "thread".into(),
                revision,
                source_run_id: Some("root".into()),
                cause: event_bus::PlanChangeCause::ModelUpdate,
                changes: vec![event_bus::PlanChange::Cleared { removed: 1 }],
                progress: Default::default(),
            });
            super::super::publish_change(&runtime, &source, &changed);
            let notification = subscription.recv().await.unwrap().event;
            assert_eq!(
                notification,
                UiEvent::Execution(ExecutionEvent::ConversationPlanChanged {
                    thread: ThreadId::new("thread").unwrap(),
                    revision: Revision(revision),
                })
            );
        }
    }

    #[tokio::test]
    async fn invalidations_are_delivered_only_to_the_subject_thread() {
        use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("plan.db"),
            ..Default::default()
        };
        let db = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(event_bus::EventBus::new(32));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            Arc::new(NoModel),
        )
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
        db.handle()
            .upsert_threads_host(
                &["thread", "other"].map(|id| storage::threads::ThreadUpsert {
                    thread_id: id.into(),
                    project_id: Some("project".into()),
                    project_root: Some(dir.path().to_string_lossy().into_owned()),
                    ..Default::default()
                }),
            )
            .unwrap();
        let projection = crate::projection::Projection::new(
            runtime,
            &tokio::runtime::Handle::current(),
            Default::default(),
        );
        projection
            .configure_projects(
                vec![(
                    workbench_protocol::ids::ProjectId::new("project").unwrap(),
                    dir.path().to_path_buf(),
                )],
                Default::default(),
            )
            .unwrap();
        let subscribe = |id| {
            projection
                .subscribe_with_snapshot(&Subscribe {
                    scope: SubscriptionScope::Thread {
                        thread: ThreadId::new(id).unwrap(),
                    },
                    after: None,
                })
                .unwrap()
        };
        let mut subject = subscribe("thread");
        let mut other = subscribe("other");
        let source = projection.source("execution").unwrap();
        let hint = |thread, revision| {
            UiEvent::Execution(ExecutionEvent::ConversationPlanChanged {
                thread: ThreadId::new(thread).unwrap(),
                revision: Revision(revision),
            })
        };
        source.publish(hint("thread", 1)).unwrap();
        // A normal second-thread notification is an ordered sentinel, not a timeout
        // or a terminal event that would discard incorrectly queued earlier hints.
        source.publish(hint("other", 2)).unwrap();
        assert_eq!(subject.recv().await.unwrap().event, hint("thread", 1));
        assert_eq!(other.recv().await.unwrap().event, hint("other", 2));
    }
}
