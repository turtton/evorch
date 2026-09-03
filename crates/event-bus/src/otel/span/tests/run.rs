use crate::event::{AgentRunPhase, LifecycleEvent};

use super::super::{SpanAction, SpanKey, SpanKind, SpanMapper, SpanStatus};
use super::{BASE_TIME, action_attributes, event, i64_attr, start_run, str_attr};

#[test]
fn agent_run_start_emits_run_then_agent_with_stable_attributes() {
    // Given: a known parent run.
    let mut mapper = SpanMapper::new();
    mapper.ingest(&start_run("parent", None, 1));
    // When: a child run is registered.
    let actions = mapper.ingest(&event(
        LifecycleEvent::AgentRunStarted {
            run_id: "child".to_owned(),
            parent_run_id: Some("parent".to_owned()),
            agent_name: "researcher".to_owned(),
            role: "explorer".to_owned(),
        },
        2,
    ));
    // Then: run precedes agent and both use the specified tree and attr order.
    assert_eq!(
        actions,
        vec![
            SpanAction::Start {
                key: SpanKey::Run {
                    run_id: "child".to_owned()
                },
                parent: Some(SpanKey::Agent {
                    run_id: "parent".to_owned()
                }),
                name: "evorch.run researcher".to_owned(),
                kind: SpanKind::Internal,
                start_time: BASE_TIME + std::time::Duration::from_secs(2),
                attributes: vec![
                    str_attr("evorch.agent_run.id", "child"),
                    str_attr("evorch.parent_agent_run.id", "parent"),
                    str_attr("evorch.agent.name", "researcher"),
                    str_attr("evorch.delegation.role", "explorer"),
                    i64_attr("evorch.delegation.depth", 1)
                ]
            },
            SpanAction::Start {
                key: SpanKey::Agent {
                    run_id: "child".to_owned()
                },
                parent: Some(SpanKey::Run {
                    run_id: "child".to_owned()
                }),
                name: "invoke_agent researcher".to_owned(),
                kind: SpanKind::Client,
                start_time: BASE_TIME + std::time::Duration::from_secs(2),
                attributes: vec![
                    str_attr("gen_ai.operation.name", "invoke_agent"),
                    str_attr("gen_ai.provider.name", "evorch"),
                    str_attr("gen_ai.agent.name", "researcher"),
                    str_attr("evorch.agent_run.id", "child"),
                    str_attr("evorch.delegation.role", "explorer"),
                    i64_attr("evorch.delegation.depth", 1)
                ]
            }
        ]
    );
}

#[test]
fn delegation_depth_is_checked_and_capped_at_99() {
    // Given: a chain whose computed depth reaches and exceeds the cap.
    let mut mapper = SpanMapper::new();
    let mut parent = "run-0".to_owned();
    mapper.ingest(&start_run(&parent, None, 0));
    // When: one hundred descendants are registered.
    let mut depth_one = Vec::new();
    let mut capped = Vec::new();
    for index in 1_u64..=100 {
        let child = format!("run-{index}");
        let actions = mapper.ingest(&start_run(&child, Some(&parent), index));
        if index == 1 {
            depth_one = actions;
        } else if index == 100 {
            capped = actions;
        }
        parent = child;
    }
    // Then: the first child is depth 1 and overflow beyond 99 remains 99.
    assert_eq!(
        action_attributes(&depth_one[0]).last(),
        Some(&i64_attr("evorch.delegation.depth", 1))
    );
    assert_eq!(
        action_attributes(&capped[0]).last(),
        Some(&i64_attr("evorch.delegation.depth", 99))
    );
}

#[test]
fn terminal_run_state_closes_agent_before_run_for_success_and_error() {
    // Given: two open runs.
    let mut mapper = SpanMapper::new();
    mapper.ingest(&start_run("done", None, 1));
    mapper.ingest(&start_run("error", None, 2));
    // When: one completes and one errors.
    let done = mapper.ingest(&event(
        LifecycleEvent::AgentRunStateChanged {
            run_id: "done".to_owned(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        },
        3,
    ));
    let failed = mapper.ingest(&event(
        LifecycleEvent::AgentRunStateChanged {
            run_id: "error".to_owned(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Error,
            reason: Some("raw run reason".to_owned()),
        },
        4,
    ));
    // Then: agent End precedes run End, with stable error classification only.
    assert!(matches!(
        done[0],
        SpanAction::End {
            key: SpanKey::Agent { .. },
            status: SpanStatus::Unset,
            ..
        }
    ));
    assert!(matches!(
        done[1],
        SpanAction::End {
            key: SpanKey::Run { .. },
            status: SpanStatus::Unset,
            ..
        }
    ));
    assert!(matches!(
        failed[0],
        SpanAction::End {
            key: SpanKey::Agent { .. },
            status: SpanStatus::Error,
            ..
        }
    ));
    assert!(matches!(
        failed[1],
        SpanAction::End {
            key: SpanKey::Run { .. },
            status: SpanStatus::Error,
            ..
        }
    ));
    assert!(failed.iter().all(|action| action_attributes(action).last()
        == Some(&str_attr("error.type", "agent_run_error"))));
    assert!(!format!("{failed:?}").contains("raw run reason"));
}

#[test]
fn delegated_event_does_not_mutate_run_topology() {
    // Given: a parent run and its child.
    let mut mapper = SpanMapper::new();
    mapper.ingest(&start_run("parent", None, 1));
    mapper.ingest(&start_run("child", Some("parent"), 2));
    // When: an unrelated delegated event is observed, then a grandchild starts.
    assert!(
        mapper
            .ingest(&event(
                LifecycleEvent::Delegated {
                    session_id: "unrelated-session".to_owned(),
                    target: "different-target".to_owned()
                },
                3
            ))
            .is_empty()
    );
    let grandchild = mapper.ingest(&start_run("grandchild", Some("child"), 4));
    // Then: the established parent graph is unchanged.
    assert_eq!(
        action_attributes(&grandchild[0]).last(),
        Some(&i64_attr("evorch.delegation.depth", 2))
    );
}

#[test]
fn background_task_id_is_added_to_run_final_attributes() {
    // Given: an open run whose ID equals a background task ID.
    let mut mapper = SpanMapper::new();
    mapper.ingest(&start_run("task-1", None, 1));
    // When: the task starts and the run completes.
    assert!(
        mapper
            .ingest(&event(
                LifecycleEvent::BackgroundTaskStarted {
                    task_id: "task-1".to_owned()
                },
                2
            ))
            .is_empty()
    );
    let actions = mapper.ingest(&event(
        LifecycleEvent::AgentRunStateChanged {
            run_id: "task-1".to_owned(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        },
        3,
    ));
    // Then: the in-flight task attribute appears only in the run End attributes.
    assert!(
        !action_attributes(&actions[0])
            .iter()
            .any(|attr| attr.key == "evorch.task.id")
    );
    assert_eq!(
        action_attributes(&actions[1]).last(),
        Some(&str_attr("evorch.task.id", "task-1"))
    );
}
