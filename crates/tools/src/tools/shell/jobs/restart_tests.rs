//! Restart uses terminal outcomes, never discard/drain or observation alone.
use super::*;
use crate::{Shell, ShellJobState, Tool};
use sandbox::{
    DirectSandbox,
    ledger::{ChildKind, SpawnLedger},
};
use serde_json::json;

#[tokio::test]
async fn restart_pipe_and_pty_wait_interrupt_reap_and_preserve_bounded_results() {
    for interactive in [false, true] {
        for interrupt in [false, true] {
            let ledger = SpawnLedger::new();
            let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()))
                .with_spawn_ledger(ledger.clone());
            let mut children = ledger.watch_children();
            let ctx = ToolExecutionContext {
                run_id: "restart-owner".into(),
                call_id: Some("original-shell-call".into()),
                ..Default::default()
            };
            let script = r#"printf 'ready\n'; read value; head -c 180000 /dev/zero | tr '\000' x; printf '\nfinished\n'"#;
            shell
                .execute_with_context(
                    &ctx,
                    json!({"command":script,"interactive":interactive,"yield_ms":0}),
                )
                .await
                .unwrap();
            while ledger.children().is_empty() {
                children.changed().await.unwrap();
            }
            assert_eq!(ledger.children()[0].kind, ChildKind::ShellJob);
            let snapshot = shell.shell_job_snapshots(&ctx.run_id).pop().unwrap();
            let identity = snapshot.identity;
            assert_eq!(snapshot.call_id.as_deref(), Some("original-shell-call"));
            assert!(shell.shell_job_result(&identity).is_err());
            assert!(shell.acknowledge_shell_job_result(&identity).is_err());
            let job = shell.jobs.jobs.lock().unwrap()[identity.id()].clone();
            {
                let mut waiting = Box::pin(shell.wait_shell_job_terminal(&identity));
                assert!(futures_util::poll!(waiting.as_mut()).is_pending());
            }
            assert!(!*job.cancel.borrow(), "Wait must never signal");
            ledger.close_fence();
            assert!(
                shell
                    .execute_with_context(&ctx, json!({"command":"true","yield_ms":0}))
                    .await
                    .is_err()
            );
            if interrupt {
                shell.interrupt_shell_job(&identity).unwrap();
                shell.interrupt_shell_job(&identity).unwrap();
            } else {
                let (result, received) = oneshot::channel();
                job.input
                    .send(Input {
                        bytes: b"continue\n".to_vec(),
                        close: false,
                        result,
                    })
                    .await
                    .unwrap();
                received.await.unwrap().unwrap();
            }
            let terminal = shell.wait_shell_job_terminal(&identity).await.unwrap();
            assert_eq!(terminal.state, ShellJobState::Terminal);
            assert!(
                !terminal.observed,
                "terminal observation is not durable acknowledgement"
            );
            ledger.reap_all_children().await.unwrap();
            ledger.join_workers().await.unwrap();
            assert_eq!(ledger.child_stats().reaped, 1);
            assert_eq!(ledger.child_stats().unconfirmed, 0);
            let outcome = shell.shell_job_result(&identity).unwrap();
            assert!(outcome.result.content.len() < 20000);
            assert_eq!(
                outcome.result,
                shell.shell_job_result(&identity).unwrap().result
            );
            assert!(shell.has_unobserved_shell_jobs(&ctx.run_id));
            if interrupt {
                assert!(outcome.result.content.contains("cancelled"));
            } else {
                assert_eq!(terminal.exit_code, Some(0));
                assert!(outcome.result.detail.as_ref().unwrap()["output_artifact"].is_object());
            }
            shell.acknowledge_shell_job_result(&identity).unwrap();
            shell.acknowledge_shell_job_result(&identity).unwrap();
            assert!(!shell.has_unobserved_shell_jobs(&ctx.run_id));
            assert!(shell.jobs.take_notifications(&ctx.run_id).is_empty());
            // Neither a stale identity nor a changed owner can stop a newer job.
            let mut foreign = identity.clone();
            foreign.owner = "other-owner".into();
            assert!(shell.interrupt_shell_job(&foreign).is_err());
            shell.release_shell_jobs(&ctx.run_id).unwrap();
            assert!(shell.interrupt_shell_job(&identity).is_err());
            assert!(shell.wait_shell_job_terminal(&identity).await.is_err());
        }
    }
}

#[tokio::test]
async fn restart_unconfirmed_teardown_never_becomes_an_acknowledged_result() {
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()));
    let ctx = ToolExecutionContext {
        run_id: "failed-owner".into(),
        call_id: Some("failed-call".into()),
        ..Default::default()
    };
    // An invalid spawn reaches a terminal error but cannot manufacture a
    // confirmed-teardown receipt from a supervisor error.
    shell
        .execute_with_context(&ctx, json!({"command":"invalid\0command", "yield_ms":0}))
        .await
        .unwrap();
    let identity = shell
        .shell_job_snapshots(&ctx.run_id)
        .pop()
        .unwrap()
        .identity;
    let job = shell.jobs.jobs.lock().unwrap()[identity.id()].clone();
    job.until_terminal().await;
    assert!(shell.wait_shell_job_terminal(&identity).await.is_err());
    assert!(shell.shell_job_result(&identity).is_err());
    assert!(shell.acknowledge_shell_job_result(&identity).is_err());
    assert!(!shell.shell_job_snapshots(&ctx.run_id)[0].teardown_confirmed);
    assert!(shell.has_unobserved_shell_jobs(&ctx.run_id));
}
