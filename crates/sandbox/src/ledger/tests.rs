use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::sync::Arc;

use rustix::process::Pid;

use super::*;
use crate::process::{GroupChild, apply_launch_policy, has_exited};
use crate::{CommandSpec, DirectSandbox, StdioSession};

/// A child that runs until its group is killed: `cat` on a pipe that stays open.
fn blocked_child() -> tokio::process::Command {
    let mut command = std::process::Command::new("cat");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    apply_launch_policy(&mut command);
    tokio::process::Command::from(command)
}

fn spawn_child(ledger: &SpawnLedger, kind: ChildKind) -> Result<GroupChild, SpawnError> {
    let admission = ledger.admit()?;
    Ok(admission.spawn_group(kind, None, &mut blocked_child())?)
}

/// The owner side: reap the child once it is stopped, as a supervisor does.
fn own(mut child: GroupChild) -> tokio::task::JoinHandle<std::process::ExitStatus> {
    tokio::spawn(async move { child.wait().await.expect("the child is reaped") })
}

fn leader_of(ledger: &SpawnLedger, id: ChildId) -> Pid {
    let leader = ledger
        .children()
        .into_iter()
        .find(|info| info.id == id)
        .and_then(|info| info.leader)
        .expect("a registered child has a leader");
    Pid::from_raw(leader).expect("a valid pid")
}

// Given: children spawned before the fence / When: the fence closes, then the
// ledger stops and reaps / Then: all of them were listed, and all are reaped
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn children_spawned_before_the_fence_are_listed_and_reaped() {
    let ledger = SpawnLedger::new();
    let owners: Vec<_> = [
        ChildKind::McpStdio,
        ChildKind::PullRequestSource,
        ChildKind::Eval,
    ]
    .into_iter()
    .map(|kind| own(spawn_child(&ledger, kind).expect("spawn")))
    .collect();
    let listed = ledger.children();
    let pids: Vec<Pid> = listed
        .iter()
        .map(|info| Pid::from_raw(info.leader.expect("leader")).expect("pid"))
        .collect();
    assert_eq!(
        listed.iter().map(|info| info.kind).collect::<Vec<_>>(),
        [
            ChildKind::McpStdio,
            ChildKind::PullRequestSource,
            ChildKind::Eval
        ]
    );

    ledger.close_fence();
    assert_eq!(ledger.stop_all_children(), 3);
    ledger.reap_all_children().await.unwrap();

    assert!(ledger.children().is_empty());
    assert_eq!(
        ledger.child_stats(),
        ChildStats {
            registered: 3,
            reaped: 3,
            unconfirmed: 0
        }
    );
    for pid in pids {
        assert!(
            has_exited(pid).is_err(),
            "{pid:?} must be reaped, not an unreaped child"
        );
    }
    for owner in owners {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(owner.await.unwrap().signal(), Some(9));
    }
}

// Given: a closed fence / When: anything tries to start / Then: it is refused
// and nothing is registered
#[tokio::test]
async fn nothing_starts_after_the_fence() {
    let ledger = SpawnLedger::new();
    ledger.close_fence();
    ledger.close_fence();

    assert!(ledger.is_fenced());
    assert!(matches!(
        spawn_child(&ledger, ChildKind::Other),
        Err(SpawnError::Fenced)
    ));
    assert!(matches!(
        SpawnLedger::admit_optional(Some(&ledger)),
        Err(SpawnError::Fenced)
    ));
    assert!(SpawnLedger::admit_optional(None).is_ok());
    assert!(ledger.children().is_empty());
    assert_eq!(ledger.child_stats().registered, 0);
}

// Given: spawners that hold their admissions while the fence is asked to close /
// When: the fence is released only after it has begun waiting / Then: it does not
// complete while an admission lives, every admitted spawn is in the ledger and
// reaped, and a spawn begun after it is refused
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fence_waits_for_admitted_spawns_and_refuses_later_ones() {
    const SPAWNERS: usize = 4;
    let ledger = SpawnLedger::new();
    let (held_sender, held) = std::sync::mpsc::channel::<()>();
    let mut releases = Vec::new();
    let spawners: Vec<_> = (0..SPAWNERS)
        .map(|_| {
            let (release, released) = std::sync::mpsc::channel::<()>();
            releases.push(release);
            let (ledger, held_sender) = (ledger.clone(), held_sender.clone());
            tokio::task::spawn_blocking(move || {
                let admission = ledger.admit().expect("the fence is still open");
                let child = admission
                    .spawn_group(ChildKind::Other, None, &mut blocked_child())
                    .expect("spawn");
                held_sender.send(()).unwrap();
                // The admission lives until the test lets it go.
                released.recv().unwrap();
                drop(admission);
                child
            })
        })
        .collect();
    for _ in 0..SPAWNERS {
        held.recv().unwrap();
    }
    let mut fence = ledger.watch_fence();
    let closer = {
        let ledger = ledger.clone();
        tokio::task::spawn_blocking(move || ledger.close_fence())
    };

    // The closer announces that it has begun; it cannot finish while any
    // admission is alive.
    let state = *fence
        .wait_for(|state| *state != FenceState::Open)
        .await
        .unwrap();
    assert_eq!(state, FenceState::Closing);
    for release in &releases {
        release.send(()).unwrap();
    }
    let mut admitted = Vec::new();
    for spawner in spawners {
        admitted.push(own(spawner.await.unwrap()));
    }
    closer.await.unwrap();

    assert_eq!(*fence.borrow(), FenceState::Closed);
    assert_eq!(ledger.children().len(), SPAWNERS);
    assert!(matches!(
        spawn_child(&ledger, ChildKind::Other),
        Err(SpawnError::Fenced)
    ));
    ledger.stop_all_children();
    ledger.reap_all_children().await.unwrap();
    for owner in admitted {
        owner.await.unwrap();
    }
    let stats = ledger.child_stats();
    assert_eq!(stats.registered as usize, SPAWNERS);
    assert_eq!(stats.reaped, stats.registered);
}

// Given: two children / When: one is stopped by id / Then: only that one stops,
// an id that is gone is unknown, and the other is terminated separately
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_names_one_child_and_a_gone_id_is_unknown() {
    let ledger = SpawnLedger::new();
    let first = spawn_child(&ledger, ChildKind::McpStdio).unwrap();
    let second = spawn_child(&ledger, ChildKind::McpStdio).unwrap();
    let ids: Vec<ChildId> = ledger.children().iter().map(|info| info.id).collect();
    let (first_owner, second_owner) = (own(first), own(second));
    let second_leader = leader_of(&ledger, ids[1]);

    ledger.stop_child(ids[0]).unwrap();
    ledger.reap_child(ids[0]).await.unwrap();

    assert!(first_owner.await.unwrap().code().is_none());
    assert_eq!(ledger.stop_child(ids[0]), Err(UnknownChild(ids[0])));
    let remaining = ledger.children();
    assert_eq!(remaining.len(), 1);
    assert!(!remaining[0].stop_requested);
    assert!(!has_exited(second_leader).unwrap());

    ledger.terminate_child(ids[1]).unwrap();
    // One listing: the child may already have gone, or shows the request.
    let listed = ledger.children();
    assert!(listed.is_empty() || listed[0].stop_requested);
    ledger.reap_child(ids[1]).await.unwrap();
    second_owner.await.unwrap();
    assert!(ledger.children().is_empty());
}

// Given: a process the ledger does not own / When: the ledger stops everything
// it owns / Then: that process is untouched
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_process_the_ledger_does_not_own_is_never_signalled() {
    let mut bystander = std::process::Command::new("cat");
    bystander
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    apply_launch_policy(&mut bystander);
    let mut bystander = bystander.spawn().expect("spawn the bystander");
    let ledger = SpawnLedger::new();
    let owner = own(spawn_child(&ledger, ChildKind::Other).unwrap());

    ledger.close_fence();
    ledger.stop_all_children();
    ledger.reap_all_children().await.unwrap();
    owner.await.unwrap();

    // A live `cat` echoes; a killed one would only report end of file.
    let mut stdin = bystander.stdin.take().unwrap();
    writeln!(stdin, "ping").unwrap();
    let mut echoed = String::new();
    BufReader::new(bystander.stdout.take().unwrap())
        .read_line(&mut echoed)
        .unwrap();
    assert_eq!(echoed, "ping\n");
    drop(stdin);
    assert!(bystander.wait().unwrap().success());
}

// Given: an owner that drops its child without reaping / When: the ledger waits
// for every child / Then: it resolves only once the ledger's reaper has reaped
// the leader, which is gone by then
#[tokio::test]
async fn a_dropped_child_is_reaped_by_the_ledger_before_reaping_resolves() {
    let ledger = SpawnLedger::new();
    let child = spawn_child(&ledger, ChildKind::Other).unwrap();
    let leader = leader_of(&ledger, ledger.children()[0].id);

    drop(child);
    ledger.reap_all_children().await.unwrap();

    assert_eq!(
        ledger.child_stats(),
        ChildStats {
            registered: 1,
            reaped: 1,
            unconfirmed: 0
        }
    );
    // Reaped: it is not our child any more, so it cannot even be observed.
    assert!(has_exited(leader).is_err());
}

// Given: an owner that lets go of a registered child without any reap / When:
// the ledger waits / Then: the departure is reported, never taken for success
#[tokio::test]
async fn a_child_that_left_without_a_reap_is_reported_unconfirmed() {
    let ledger = SpawnLedger::new();
    // Started outside the ledger, as a shell job runtime would start its own.
    let mut child = Admission::unmanaged()
        .spawn_group(ChildKind::Other, None, &mut blocked_child())
        .unwrap();
    let entry = {
        let admission = ledger.admit().unwrap();
        admission
            .register_group(ChildKind::ShellJob, None, child.group())
            .unwrap()
    };
    let id = entry.id();

    drop(entry);

    let error = ledger.reap_child(id).await.unwrap_err();
    assert_eq!(error, UnconfirmedReap(vec![id]));
    assert_eq!(ledger.reap_all_children().await.unwrap_err(), error);
    assert_eq!(ledger.child_stats().unconfirmed, 1);
    child.kill_and_wait().await.unwrap();
}

// Given: a stdio session started in an admission / When: it is stopped by the
// ledger and dropped / Then: the session's synchronous reap leaves the ledger
#[tokio::test]
async fn a_stdio_session_is_registered_and_reaped_with_the_ledger() {
    let ledger = SpawnLedger::new();
    let session = {
        let admission = ledger.admit().unwrap();
        StdioSession::spawn_in(
            &admission,
            &DirectSandbox::new_unchecked(),
            CommandSpec {
                program: "cat".into(),
                args: Vec::new(),
                cwd: None,
                extra_env: Vec::new(),
            },
        )
        .unwrap()
    };
    let listed = ledger.children();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].kind, ChildKind::SandboxSession);

    ledger.close_fence();
    ledger.stop_all_children();
    drop(session);
    ledger.reap_all_children().await.unwrap();

    assert_eq!(ledger.child_stats().reaped, 1);
}

// Given: a task and a blocking thread / When: they are told to stop and joined /
// Then: the join returns only after their bodies and captures are gone
#[tokio::test]
async fn joining_workers_proves_the_task_and_the_thread_ended() {
    let ledger = SpawnLedger::new();
    let witness = Arc::new(());
    let (release_thread, wait_thread) = std::sync::mpsc::channel::<()>();
    let (never, parked) = tokio::sync::oneshot::channel::<()>();
    {
        let admission = ledger.admit().unwrap();
        let held = witness.clone();
        admission.spawn_worker("parked-monitor", async move {
            let _held = held;
            let _ = parked.await;
        });
        let held = witness.clone();
        admission
            .spawn_worker_thread("blocking-worker", move || {
                let _held = held;
                let _ = wait_thread.recv();
            })
            .unwrap();
    }
    assert_eq!(ledger.workers().len(), 2);
    assert_eq!(Arc::strong_count(&witness), 3);

    ledger.close_fence();
    assert_eq!(ledger.abort_workers(), 1);
    release_thread.send(()).unwrap();
    ledger.join_workers().await.unwrap();

    assert!(ledger.workers().is_empty());
    assert_eq!(Arc::strong_count(&witness), 1);
    assert_eq!(
        ledger.worker_stats(),
        WorkerStats {
            spawned: 2,
            finished: 2,
            panicked: 0
        }
    );
    drop(never);
}

// Given: a task that panics and one that finishes by itself / When: joined /
// Then: both leave the ledger and the panic is counted
#[tokio::test]
async fn workers_that_panic_or_finish_leave_the_ledger() {
    let ledger = SpawnLedger::new();
    let (done, finished) = tokio::sync::oneshot::channel::<()>();
    {
        let admission = ledger.admit().unwrap();
        admission.spawn_worker("finishes", async move {
            let _ = finished.await;
        });
        admission.spawn_worker("panics", async {
            panic!("a monitor failed");
        });
    }
    done.send(()).unwrap();

    ledger.join_workers().await.unwrap();

    let stats = ledger.worker_stats();
    assert_eq!((stats.spawned, stats.finished), (2, 2));
    assert_eq!(stats.panicked, 1);
}

// Given: a closed fence / When: a worker is requested / Then: it is refused
#[tokio::test]
async fn no_worker_starts_after_the_fence() {
    let ledger = SpawnLedger::new();
    ledger.close_fence();

    assert!(matches!(ledger.admit(), Err(SpawnError::Fenced)));
    assert!(ledger.workers().is_empty());
}

// Given: a site without a ledger / When: it spawns through an unmanaged
// admission / Then: the child runs unregistered as it always did
#[tokio::test]
async fn an_unmanaged_admission_registers_nothing() {
    let admission = Admission::unmanaged();
    let mut child = admission
        .spawn_group(ChildKind::Other, None, &mut blocked_child())
        .unwrap();

    assert!(admission.ledger().is_none());
    assert!(
        admission
            .register_group(ChildKind::Other, None, child.group())
            .is_none()
    );
    child.kill();
    child.wait().await.unwrap();
}

// Given: a blocking worker and a join that is dropped before it finishes /
// When: the worker is released and the join is called again / Then: the second
// join completes, because the ledger, not the dropped future, owns the join
#[tokio::test]
async fn a_cancelled_join_can_be_repeated() {
    let ledger = SpawnLedger::new();
    let (release, released) = std::sync::mpsc::channel::<()>();
    {
        let admission = ledger.admit().unwrap();
        admission
            .spawn_worker_thread("blocking-worker", move || {
                let _ = released.recv();
            })
            .unwrap();
    }

    // The worker is blocked, so the join is pending; the ready branch cancels it.
    tokio::select! {
        biased;
        _ = ledger.join_workers() => panic!("the worker is still blocked"),
        () = std::future::ready(()) => {}
    }
    assert_eq!(ledger.workers().len(), 1);
    release.send(()).unwrap();
    ledger.join_workers().await.unwrap();

    assert!(ledger.workers().is_empty());
    assert_eq!(ledger.worker_stats().finished, 1);
}

// Given: a blocking worker whose joiner thread cannot be started / When: the
// workers are joined / Then: the join reports the failure instead of success, the
// unfinished worker stays registered, and a later join finishes it
#[tokio::test]
async fn a_joiner_that_cannot_start_leaves_the_worker_registered() {
    let ledger = SpawnLedger::new();
    let (release, released) = std::sync::mpsc::channel::<()>();
    {
        let admission = ledger.admit().unwrap();
        admission
            .spawn_worker_thread("blocking-worker", move || {
                let _ = released.recv();
            })
            .unwrap();
    }
    ledger
        .inner
        .seams
        .fail_joiner_starts
        .store(1, std::sync::atomic::Ordering::SeqCst);

    let failed = ledger.join_workers().await;

    assert_eq!(failed, Err(JoinFailed { unjoined: 1 }));
    assert_eq!(ledger.workers().len(), 1);
    assert_eq!(ledger.worker_stats().finished, 0);
    release.send(()).unwrap();
    ledger.join_workers().await.unwrap();
    assert!(ledger.workers().is_empty());
    assert_eq!(ledger.worker_stats().finished, 1);
}

// Given: a worker thread that has ended but whose handle is not yet published /
// When: the handle is published after the join has started its joins and before
// it looks at what is left / Then: the join is woken by the publication and
// completes, instead of waiting for a change that was already missed
#[tokio::test]
async fn a_handle_published_during_a_join_wakes_it() {
    let ledger = SpawnLedger::new();
    let (publish, hold) = std::sync::mpsc::channel::<()>();
    *locked(&ledger.inner.seams.hold_publish) = Some(hold);
    let spawner = {
        let ledger = ledger.clone();
        std::thread::spawn(move || {
            let admission = ledger.admit().unwrap();
            admission
                .spawn_worker_thread("quick-worker", || {})
                .unwrap();
        })
    };
    // The worker has ended, so its own notification is behind us; its handle is
    // held back, and only publishing it can wake the join.
    ledger
        .watch_workers()
        .wait_for(|_| ledger.worker_stats().finished == 1)
        .await
        .unwrap();
    *locked(&ledger.inner.seams.join_window) = Some(Box::new(move || {
        publish.send(()).unwrap();
        spawner.join().unwrap();
    }));

    ledger.join_workers().await.unwrap();

    assert!(ledger.workers().is_empty());
    assert_eq!(ledger.worker_stats().finished, 1);
}
