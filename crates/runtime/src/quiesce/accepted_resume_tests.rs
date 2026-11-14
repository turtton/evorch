//! Accepted manual continuations are not rejected/retried commands.
use super::{fixture::Fixture, *};
use crate::{RunConfig, StopScope};
use std::sync::atomic::Ordering;

async fn accepted_resume() -> (Fixture, RunId, RestartAttempt) {
    let mut f = Fixture::new(1);
    let run = f.start();
    f.model.entered.notified().await;
    f.model.release.add_permits(1);
    assert_eq!(f.entered.recv().await, Some(0));
    f.runtime.stop(run, StopScope::SelfOnly).unwrap();
    f.runtime
        .resume_chat(
            run,
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(f.runtime.accepted_chat_resumes().contains_key(&run));
    let attempt = f.runtime.begin_restart_quiesce().unwrap();
    f.release[0].add_permits(1);
    f.runtime.wait(run).await.unwrap();
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
    (f, run, attempt)
}

#[tokio::test]
async fn accepted_resume_blocks_seal_and_replays_once_after_restart_cancel() {
    let (f, run, attempt) = accepted_resume().await;
    assert!(!f.runtime.complete_restart_safe_points().unwrap());
    assert!(
        f.runtime
            .restart_blockers()
            .iter()
            .any(|blocker| blocker.run == run
                && blocker.kind == RestartBlockerKind::AcceptedResumePending)
    );
    {
        use std::{
            future::{Future, poll_fn},
            task::Poll,
        };
        let mut seal = std::pin::pin!(f.runtime.fence_dispatch_restart());
        poll_fn(|cx| {
            assert!(seal.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }
    assert!(f.runtime.accepted_chat_resumes().contains_key(&run));
    f.runtime.cancel_restart_quiesce(&attempt).unwrap();
    f.model.entered.notified().await;
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 2);
    assert!(!f.runtime.accepted_chat_resumes().contains_key(&run));
    // A duplicate terminal callback must not claim the accepted authority twice.
    f.runtime.replay_accepted_chat_resumes();
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 2);
    f.cleanup(run).await;
}

#[tokio::test]
async fn explicit_stop_discards_accepted_resume_without_launching_it() {
    let (f, run, attempt) = accepted_resume().await;
    f.runtime.stop(run, StopScope::SelfOnly).unwrap();
    assert!(!f.runtime.accepted_chat_resumes().contains_key(&run));
    f.runtime.wait_quiesced().await.unwrap();
    f.runtime.cancel_restart_quiesce(&attempt).unwrap();
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
    assert!(!f.intent(run).resume_requested);
}

#[tokio::test]
async fn shutdown_releases_accepted_resume_without_launching_it() {
    let (f, run, attempt) = accepted_resume().await;
    let fence = f
        .runtime
        .fence_dispatch_shutdown(|| async { Ok::<_, crate::RuntimeError>(()) })
        .await
        .unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    assert!(!f.runtime.accepted_chat_resumes().contains_key(&run));
    assert!(f.runtime.cancel_restart_quiesce(&attempt).is_err());
    assert!(!f.intent(run).resume_requested);
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
}
