use super::*;
use std::sync::Arc;
use tokio::sync::oneshot;

#[tokio::test]
async fn drain_joins_children_spawned_by_a_draining_producer() {
    let tasks = Arc::new(ServiceTasks::default());
    let children = tasks.clone();
    let (stop, stopped) = oneshot::channel();
    let (joined, completed) = oneshot::channel();
    tasks.spawn(async move {
        stopped.await.unwrap();
        children.spawn(async move {
            joined.send(()).unwrap();
        });
    });
    stop.send(()).unwrap();
    tasks.drain().await.unwrap();
    completed.await.unwrap();
    tasks.drain().await.unwrap();
}

#[tokio::test]
async fn task_failure_does_not_skip_healthy_tasks() {
    let tasks = ServiceTasks::default();
    let (entered, ready) = oneshot::channel();
    tasks.spawn(async move {
        entered.send(()).unwrap();
        panic!("producer failed");
    });
    ready.await.unwrap();
    let (drained, completed) = oneshot::channel();
    tasks.spawn(async move {
        drained.send(()).unwrap();
    });
    assert!(tasks.drain().await.unwrap_err().is_panic());
    completed.await.unwrap();
}
