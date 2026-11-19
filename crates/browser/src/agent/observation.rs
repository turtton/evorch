//! Immutable creation input from the canonical run executor. Never infer an
//! incarnation from the latest run-id registration or stamp an existing page.
pub mod source;
use std::{future::Future, sync::Arc};
#[derive(Debug, PartialEq, Eq)]
pub struct ExecutionStamp {
    pub run: String,
    pub incarnation: String,
}
tokio::task_local! {static CREATION_STAMP:Arc<ExecutionStamp>;}
/// Access is scoped to the actual tool invocation, including prepared calls.
/// A Chromium creation path captures the Arc BEFORE spawning event work and
/// carries it to that target and its popups; task-local state is not inherited
/// by spawned tasks. O-S owns that target binding, never retrospective lookup.
#[derive(Default)]
pub struct ExecutionStamps;
impl ExecutionStamps {
    pub fn for_creation(&self, run: &str) -> Option<Arc<ExecutionStamp>> {
        CREATION_STAMP
            .try_with(Arc::clone)
            .ok()
            .filter(|s| s.run == run)
    }
}
pub async fn with_execution_stamp<T>(
    stamp: Arc<ExecutionStamp>,
    work: impl Future<Output = T>,
) -> T {
    CREATION_STAMP.scope(stamp, work).await
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn concurrent_incarnations_cannot_borrow_each_others_creation_stamp() {
        let input = ExecutionStamps;
        let barrier = tokio::sync::Barrier::new(2);
        let invoke = |incarnation: &str| {
            let stamp = Arc::new(ExecutionStamp {
                run: "run".into(),
                incarnation: incarnation.into(),
            });
            let barrier = &barrier;
            let input = &input;
            with_execution_stamp(stamp.clone(), async move {
                barrier.wait().await;
                assert!(input.for_creation("another-run").is_none());
                input.for_creation("run").unwrap()
            })
        };
        let (a, b) = tokio::join!(invoke("a"), invoke("b"));
        assert_eq!(a.incarnation, "a");
        assert_eq!(b.incarnation, "b");
        assert!(input.for_creation("run").is_none());
    }
}
