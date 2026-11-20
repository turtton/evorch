//! Numeric/observation/lifetime units, not the production-composition positives.
use crate::lifecycle::shutdown_watchdog::test_clock::{self, Decision, TestClock};
use std::sync::{Arc, Barrier};
#[path = "shutdown_watchdog_sink_tests.rs"]
mod sink_tests;

#[test]
fn deadline_is_ten_seconds_once_and_overflow_fails_closed() {
    let state = Decision::new();
    state.arm(17);
    assert_eq!(state.get(), 10_000_000_017);
    state.arm(99_000_000_000);
    assert_eq!(state.get(), 10_000_000_017);
    assert!(!state.expire(10_000_000_016));
    assert!(state.expire(10_000_000_017));
    assert!(!state.complete(0));
    for now in [
        u64::MAX,
        u64::MAX - 10_000_000_000,
        u64::MAX - 10_000_000_001,
    ] {
        let state = Decision::new();
        state.arm(now);
        assert_eq!(state.get(), 1);
        assert!(state.expire(now));
    }
}
#[test]
fn completion_at_9999ms_wins_but_at_10000ms_cannot() {
    let before = Decision::new();
    before.arm(0);
    assert!(before.complete(9_999_000_000));
    assert!(!before.expire(10_000_000_000));
    assert_eq!(before.get(), test_clock::COMPLETED_STATE);
    let at = Decision::new();
    at.arm(0);
    assert!(!at.complete(10_000_000_000));
    assert!(at.expire(10_000_000_000));
    assert_eq!(at.get(), test_clock::EXPIRED_STATE);
}
#[test]
fn terminal_cas_race_has_exactly_one_winner() {
    let state = Arc::new(Decision::new());
    state.arm(0);
    let start = Arc::new(Barrier::new(3));
    let a = state.clone();
    let ba = start.clone();
    let complete = std::thread::spawn(move || {
        ba.wait();
        a.complete(9_999_000_000)
    });
    let b = state.clone();
    let bb = start.clone();
    let expire = std::thread::spawn(move || {
        bb.wait();
        b.expire(10_000_000_000)
    });
    start.wait();
    assert_ne!(complete.join().unwrap(), expire.join().unwrap());
    assert!([test_clock::COMPLETED_STATE, test_clock::EXPIRED_STATE].contains(&state.get()));
}
#[test]
fn capture_and_signal_are_order_independent_and_duplicate_does_not_extend() {
    for signal_first in [false, true] {
        let clock = TestClock::default();
        let observer = clock.observer();
        if signal_first {
            test_clock::signal(&observer);
        } else {
            observer.capture_started();
        }
        assert_eq!(test_clock::deadline(&observer), test_clock::DORMANT_STATE);
        clock.advance_to(700, &observer);
        if signal_first {
            observer.capture_started();
        } else {
            test_clock::signal(&observer);
        }
        assert_eq!(test_clock::deadline(&observer), 10_000_000_700);
        clock.advance_to(900, &observer);
        test_clock::signal(&observer);
        observer.capture_started();
        assert_eq!(test_clock::deadline(&observer), 10_000_000_700);
    }
}
#[test]
fn simultaneous_observation_cannot_lose_both_bits() {
    let clock = TestClock::default();
    let observer = clock.observer();
    let start = Arc::new(Barrier::new(3));
    let a = observer.clone();
    let ba = start.clone();
    let capture = std::thread::spawn(move || {
        ba.wait();
        a.capture_started();
    });
    let b = observer.clone();
    let bb = start.clone();
    let signal = std::thread::spawn(move || {
        bb.wait();
        test_clock::signal(&b);
    });
    start.wait();
    capture.join().unwrap();
    signal.join().unwrap();
    assert_eq!(test_clock::deadline(&observer), 10_000_000_000);
}
#[test]
fn before_capture_completion_wakes_parked_thread_and_reports_missing_wiring() {
    let result = TestClock::default()
        .run((), |(), _, _| Ok(()))
        .unwrap_or_else(|failure| panic!("setup: {}", failure.error));
    assert_eq!(
        result.unwrap_err().to_string(),
        "planned shutdown watchdog wiring incomplete"
    );
}
#[test]
fn wiring_receipts_never_replace_the_original_error() {
    let result: std::io::Result<()> = TestClock::default()
        .run((), |(), _, _| Err(std::io::Error::from_raw_os_error(17)))
        .unwrap_or_else(|failure| panic!("setup: {}", failure.error));
    assert_eq!(result.unwrap_err().raw_os_error(), Some(17));
}
#[test]
fn runtime_unwind_still_completes_and_joins_after_drop() {
    let result = std::panic::catch_unwind(|| {
        let _: Result<std::io::Result<()>, _> =
            TestClock::default().run((), |(), _, _| panic!("entry panic"));
    });
    assert!(result.is_err());
}
#[test]
fn runtime_drop_unit_child_is_killed_only_by_os_watchdog() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "restart::shutdown_watchdog_tests::runtime_drop_unit_child",
            "--ignored",
            "--nocapture",
        ])
        .env("TOKIO_WORKER_THREADS", "2")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(74),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("planned shutdown exceeded 10s"));
}
#[test]
#[ignore = "subprocess-only unit: deliberately terminates with _exit(74)"]
fn runtime_drop_unit_child() {
    struct DropEvent(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for DropEvent {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }
    let clock = TestClock::default();
    let controlled = clock.clone();
    let (drop_entered, drop_seen) = std::sync::mpsc::channel();
    let (worker_entered, worker_seen) = std::sync::mpsc::channel();
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    let _: std::io::Result<()> = clock
        .run((), |(), runtime, observer| {
            let (task_entered, task_seen) = std::sync::mpsc::channel();
            runtime.spawn(async move {
                let _on_drop = DropEvent(Some(drop_entered));
                task_entered.send(()).unwrap();
                std::future::pending::<()>().await;
            });
            runtime.spawn_blocking(move || {
                worker_entered.send(()).unwrap();
                blocked.recv().unwrap();
            });
            task_seen.recv().unwrap();
            worker_seen.recv().unwrap();
            observer.capture_started();
            test_clock::signal(&observer);
            assert_eq!(test_clock::deadline(&observer), 10_000_000_000);
            std::thread::spawn(move || {
                let _keep_worker_blocked = release;
                drop_seen.recv().unwrap();
                controlled.advance_to(10_000_000_000, &observer);
                // Keep the worker blocked until process exit, including after advance.
                loop {
                    std::thread::park();
                }
            });
            Ok(())
        })
        .unwrap_or_else(|failure| panic!("setup: {}", failure.error));
    panic!("runtime Drop returned despite the blocked worker");
}

fn signal_hook_child(case: &str) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "restart::shutdown_watchdog_tests::real_inbox_hook_child",
            "--ignored",
            "--nocapture",
        ])
        .env("EVORCH_MASKED_TEST_CHILD", "1")
        .env("EVORCH_WATCHDOG_UNIT_CASE", case)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{case}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn real_inbox_received_and_stopped_hooks_cover_both_event_orders() {
    for case in [
        "received-signal-first",
        "received-capture-first",
        "stopped-signal-first",
        "stopped-capture-first",
    ] {
        signal_hook_child(case);
    }
}
#[test]
fn real_signal_between_before_hook_and_read_is_not_lost_before_capture() {
    signal_hook_child("between-hook-and-read");
}
#[test]
#[ignore = "subprocess-only: existing entry shim blocks real process signals"]
fn real_inbox_hook_child() {
    use crate::lifecycle::OperatorSignals;
    use std::os::fd::AsRawFd;
    let case = std::env::var("EVORCH_WATCHDOG_UNIT_CASE").unwrap();
    let mask = crate::signal_test_entry::mask();
    let inbox = mask.inbox().unwrap();
    let probe = mask.inbox().unwrap();
    assert!(!probe.is_pending().unwrap());
    let clock = TestClock::default();
    // No watcher races the reader here: this proves the reader hooks, not poll.
    let observer = clock.observer();
    let send = || {
        rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
            .unwrap();
    };
    if case == "between-hook-and-read" {
        let signals = observer
            .read_pending(inbox.as_raw_fd(), || {
                // The wrapper's before hook has already found an empty inbox.
                assert!(!inbox.is_pending().unwrap());
                send();
                assert!(inbox.is_pending().unwrap());
                inbox.read_pending()
            })
            .unwrap();
        assert!(!signals.is_empty());
        assert!(!probe.is_pending().unwrap());
        assert_eq!(test_clock::deadline(&observer), test_clock::DORMANT_STATE);
        clock.advance_to(700, &observer);
        observer.capture_started();
        assert_eq!(test_clock::deadline(&observer), 10_000_000_700);
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let mut signals = OperatorSignals::from_inbox(inbox).unwrap();
    signals.set_shutdown_watchdog(observer.clone()).unwrap();
    let signal_first = case.ends_with("signal-first");
    if !signal_first {
        observer.capture_started();
    }
    if case.starts_with("stopped-") {
        let stopped = signals.stop().unwrap();
        send();
        assert!(stopped.check_pending().is_err());
        assert!(!probe.is_pending().unwrap());
        // check_pending consumed first; received returns via its shutdown latch.
        runtime.block_on(signals.received());
        drop(stopped);
    } else {
        send();
        runtime.block_on(signals.received());
        assert!(!probe.is_pending().unwrap());
    }
    if signal_first {
        assert_eq!(test_clock::deadline(&observer), test_clock::DORMANT_STATE);
        clock.advance_to(700, &observer);
        observer.capture_started();
        assert_eq!(test_clock::deadline(&observer), 10_000_000_700);
    } else {
        assert_eq!(test_clock::deadline(&observer), 10_000_000_000);
    }
}

#[test]
fn completed_outer_lifetime_joins_before_late_clock_and_never_expires() {
    let clock = TestClock::default();
    let mut saved = None;
    let result = clock
        .run((), |(), _, observer| {
            test_clock::wire(&observer);
            observer.capture_started();
            test_clock::signal(&observer);
            clock.advance_to(9_999_000_000, &observer);
            saved = Some(observer);
            Ok("normal cleanup")
        })
        .unwrap_or_else(|failure| panic!("setup: {}", failure.error))
        .unwrap();
    assert_eq!(result, "normal cleanup");
    let observer = saved.unwrap();
    assert_eq!(test_clock::deadline(&observer), test_clock::COMPLETED_STATE);
    clock.advance_to(10_000_000_000, &observer);
    assert_eq!(test_clock::deadline(&observer), test_clock::COMPLETED_STATE);
}
#[test]
fn one_observation_alone_never_arms_even_after_ten_seconds() {
    for capture in [false, true] {
        let clock = TestClock::default();
        clock
            .run((), |(), _, observer| {
                test_clock::wire(&observer);
                if capture {
                    observer.capture_started();
                } else {
                    test_clock::signal(&observer);
                }
                clock.advance_to(100_000_000_000, &observer);
                assert_eq!(test_clock::deadline(&observer), test_clock::DORMANT_STATE);
                Ok(())
            })
            .unwrap_or_else(|failure| panic!("setup: {}", failure.error))
            .unwrap();
    }
}
#[test]
fn closed_synthetic_fd_is_hup_not_a_shutdown_observation() {
    use std::os::fd::AsRawFd;
    let (socket, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(peer);
    // Linux returns POLLIN|POLLHUP for EOF. Neither reader hook nor watcher
    // may mistake that for a real signal; this is strictly a poll leaf unit.
    assert_eq!(
        test_clock::poll_unit(socket.as_raw_fd()),
        (false, false, true)
    );
}

#[test]
fn mandatory_watchdog_setup_failure_returns_the_unconsumed_context() {
    let context = Box::new(String::from("unconsumed planned boot context"));
    let address = std::ptr::from_ref(context.as_ref());
    match TestClock::default().run_invalid_inbox(context) {
        Ok(_) => panic!("invalid mandatory fd must fail before Runtime / entry"),
        Err(failure) => {
            assert_eq!(failure.error.raw_os_error(), Some(9)); // EBADF
            assert_eq!(std::ptr::from_ref(failure.context.as_ref()), address);
            assert_eq!(*failure.context, "unconsumed planned boot context");
        }
    }
}
