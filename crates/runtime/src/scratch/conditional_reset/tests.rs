use super::*;
use crate::scratch::usage::ScratchUser;
fn setup() -> (tempfile::TempDir, ThreadScratch, GraphDisposalKey) {
    let dir = tempfile::tempdir().unwrap();
    let scratch = ThreadScratch::at(dir.path().join("scratch"));
    let effect = GraphDisposalKey::new(
        "graph".into(),
        "tx".into(),
        "capture".into(),
        "thread".into(),
    )
    .unwrap();
    (dir, scratch, effect)
}
#[test]
fn lost_ack_and_new_operation_do_not_reset_new_files_or_notify_twice() {
    for exists in [false, true] {
        let (_dir, scratch, effect) = setup();
        let key = ScratchKey::Thread("thread".into());
        if exists {
            let path = scratch.prepare(&key).unwrap();
            std::fs::write(path.join("old"), "old").unwrap();
        }
        let run = RunId::new(1);
        assert!(!scratch.take_emptied_notice(&key, run, true));
        assert_eq!(
            scratch
                .reset_thread_if_unused("thread", 0, &effect)
                .unwrap(),
            Some(exists)
        );
        scratch.wake_trash().unwrap();
        assert_eq!(scratch.take_emptied_notice(&key, run, true), exists);
        assert!(!scratch.take_emptied_notice(&key, run, true));
        assert!(!scratch.take_archived_notice(&key, run));
        let path = scratch.prepare(&key).unwrap();
        std::fs::write(path.join("new"), "keep").unwrap();
        // A new operation must carry the same graph/tx/capture/thread.
        let new_operation = effect.clone();
        scratch.advance_usage_epoch(&key);
        assert_eq!(
            scratch
                .reset_thread_if_unused("thread", 0, &new_operation)
                .unwrap(),
            Some(exists)
        );
        assert_eq!(std::fs::read(path.join("new")).unwrap(), b"keep");
        assert!(!scratch.take_emptied_notice(&key, run, true));
        scratch.forget_durable_reset(&effect);
        assert!(scratch.lock().reset_effects.is_empty());
    }
}
#[test]
fn changed_epoch_users_and_reset_overflow_fail_closed() {
    for cause in ["epoch", "users", "overflow"] {
        let (_dir, scratch, effect) = setup();
        let key = ScratchKey::Thread("thread".into());
        let path = scratch.prepare(&key).unwrap();
        std::fs::write(path.join("keep"), "keep").unwrap();
        match cause {
            "epoch" => scratch.advance_usage_epoch(&key),
            "users" => scratch
                .register_user(
                    &key,
                    ScratchUser::Run {
                        run: RunId::new(1),
                        incarnation: "one".into(),
                    },
                )
                .unwrap(),
            "overflow" => {
                scratch.lock().resets.insert(key.name(), u64::MAX);
            }
            _ => unreachable!(),
        }
        let outcome = scratch.reset_thread_if_unused("thread", 0, &effect);
        if cause == "overflow" {
            assert!(outcome.is_err());
        } else {
            assert_eq!(outcome.unwrap(), None);
        }
        assert_eq!(std::fs::read(path.join("keep")).unwrap(), b"keep");
        assert!(scratch.lock().reset_effects.is_empty());
    }
}
#[test]
fn detach_failure_retains_namespace_and_bookkeeping() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, scratch, effect) = setup();
    let key = ScratchKey::Thread("thread".into());
    let path = scratch.prepare(&key).unwrap();
    std::fs::write(path.join("keep"), "keep").unwrap();
    std::fs::set_permissions(scratch.parent(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let outcome = scratch.reset_thread_if_unused("thread", 0, &effect);
    std::fs::set_permissions(scratch.parent(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(outcome.is_err());
    assert_eq!(std::fs::read(path.join("keep")).unwrap(), b"keep");
    assert!(scratch.lock().resets.is_empty());
    assert!(scratch.lock().reset_effects.is_empty());
}
#[test]
fn bounded_results_never_evict_unknown_resets() {
    let (_dir, scratch, effect) = setup();
    for n in 0..4096 {
        let mut key = effect.clone();
        key.graph = format!("graph-{n}");
        scratch.lock().reset_effects.insert(key, false);
    }
    let key = ScratchKey::Thread("thread".into());
    let path = scratch.prepare(&key).unwrap();
    std::fs::write(path.join("keep"), "keep").unwrap();
    assert!(
        scratch
            .reset_thread_if_unused("thread", 0, &effect)
            .is_err()
    );
    assert_eq!(scratch.lock().reset_effects.len(), 4096);
    assert_eq!(std::fs::read(path.join("keep")).unwrap(), b"keep");
    assert!(scratch.lock().resets.is_empty());
}

#[test]
fn epoch_hook_saturates_without_changing_existing_notice_state_or_users() {
    let (_dir, scratch, effect) = setup();
    let key = ScratchKey::Thread("thread".into());
    let run = RunId::new(1);
    scratch.prepare(&key).unwrap();
    scratch.reset_thread("thread");
    scratch.lock().archived.insert(key.name(), 3);
    assert!(scratch.take_emptied_notice(&key, run, true));
    assert!(scratch.take_archived_notice(&key, run));
    let before = {
        let state = scratch.lock();
        (
            state.resets.clone(),
            state.archived.clone(),
            state.seen.clone(),
            state.archive_seen.clone(),
        )
    };
    scratch.lock().usage.entry(key.clone()).or_default().epoch = u64::MAX - 1;
    scratch.advance_usage_epoch(&key);
    scratch.advance_usage_epoch(&key);
    {
        let state = scratch.lock();
        assert_eq!(state.usage[&key].epoch, u64::MAX);
        assert!(state.usage[&key].users.is_empty());
        assert_eq!(
            (
                state.resets.clone(),
                state.archived.clone(),
                state.seen.clone(),
                state.archive_seen.clone()
            ),
            before
        );
    }
    assert!(scratch.archive_snapshot("thread").is_none());
    assert_eq!(
        scratch
            .reset_thread_if_unused("thread", u64::MAX, &effect)
            .unwrap(),
        None
    );
    assert!(!scratch.take_emptied_notice(&key, run, true));
    assert!(!scratch.take_archived_notice(&key, run));
}

#[test]
fn disposal_key_preserves_long_internal_ids_and_rejects_mismatched_scope() {
    // Raw manifest/capture/restart/thread identities are NOT wire AdminIds.
    let long = "x".repeat(129);
    assert!(GraphDisposalKey::new(long.clone(), long.clone(), long.clone(), long).is_ok());
    assert!(
        GraphDisposalKey::new(
            String::new(),
            "tx".into(),
            "capture".into(),
            "thread".into()
        )
        .is_err()
    );
    let (_dir, scratch, effect) = setup();
    assert!(scratch.reset_thread_if_unused("other", 0, &effect).is_err());
    assert!(scratch.lock().resets.is_empty());
    assert!(scratch.lock().reset_effects.is_empty());
}

#[test]
fn empty_existing_namespace_also_notifies_only_once() {
    let (_dir, scratch, effect) = setup();
    let key = ScratchKey::Thread("thread".into());
    let run = RunId::new(1);
    scratch.prepare(&key).unwrap();
    assert_eq!(
        scratch
            .reset_thread_if_unused("thread", 0, &effect)
            .unwrap(),
        Some(true)
    );
    scratch.wake_trash().unwrap();
    assert!(scratch.take_emptied_notice(&key, run, true));
    assert!(!scratch.take_emptied_notice(&key, run, true));
}
