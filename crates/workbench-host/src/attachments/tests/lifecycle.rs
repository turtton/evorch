use super::*;

fn reserve(
    s: &mut AttachmentService,
    thread: usize,
    bytes: u64,
) -> Result<AttachmentId, WorkbenchError> {
    s.begin(
        project(),
        ThreadId::new(format!("thread-{thread}")).unwrap(),
        uuid::Uuid::new_v4().to_string(),
        "file.bin".into(),
        String::new(),
        bytes,
    )
}

#[test]
fn store_pending_count_accepts_256_and_rejects_257() {
    let (_temp, mut s) = service();
    for n in 0..STORE_PENDING_COUNT {
        reserve(&mut s, n / THREAD_PENDING_COUNT, 0).unwrap();
    }
    assert_eq!(
        reserve(&mut s, 100, 0).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
    let id = AttachmentId::new(s.uploads.keys().next().unwrap().clone()).unwrap();
    s.discard(&id).unwrap();
    reserve(&mut s, 100, 0).unwrap();
}

#[test]
fn store_pending_bytes_accepts_256_mib_and_rejects_one_more_byte() {
    let (_temp, mut s) = service();
    for n in 0..STORE_PENDING_BYTES / FILE_BYTES {
        reserve(
            &mut s,
            (n / (THREAD_PENDING_BYTES / FILE_BYTES)) as usize,
            FILE_BYTES,
        )
        .unwrap();
    }
    assert_eq!(
        reserve(&mut s, 100, 1).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
    reserve(&mut s, 100, 0).unwrap();
}

#[test]
fn sent_retention_accepts_512_mib_and_rejects_one_more_byte() {
    let (_temp, mut s) = service();
    // Retention measures lengths. A sparse fixture avoids writing 512 MiB
    // while exercising the real scan, publication and retry paths.
    s.root
        .files(thread().as_str())
        .unwrap()
        .file(
            "earlier.bin",
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY,
        )
        .unwrap()
        .set_len(THREAD_SENT_BYTES - 1)
        .unwrap();
    let first = s
        .upload_bytes(
            project(),
            thread(),
            "first".into(),
            "a.bin".into(),
            String::new(),
            b"a",
        )
        .unwrap();
    let second = s
        .upload_bytes(
            project(),
            thread(),
            "second".into(),
            "b.bin".into(),
            String::new(),
            b"b",
        )
        .unwrap();
    let root = s.root.clone();
    let _lock = root.lock_thread(thread().as_str()).unwrap();
    let published = s
        .commit_locked(&project(), &thread(), std::slice::from_ref(&first.id))
        .unwrap();
    assert_eq!(std::fs::read(&published[0].1).unwrap(), b"a");
    s.commit_locked(&project(), &thread(), &[first.id]).unwrap();
    assert_eq!(
        s.commit_locked(&project(), &thread(), std::slice::from_ref(&second.id))
            .unwrap_err()
            .code,
        ErrorCode::LimitExceeded
    );
    assert!(
        root.thread(thread().as_str())
            .unwrap()
            .directory("staging")
            .unwrap()
            .path()
            .join(format!("{}.ready", second.id))
            .exists()
    );
}

#[test]
#[ignore = "subprocess entry point; invoked by startup_recovery_failure_preserves_orphan_and_retry_reclaims_it"]
fn orphan_upload_process() {
    let root = std::env::var_os("EVORCH_ORPHAN_ROOT").expect("parent root");
    let mut service = AttachmentService::new(AttachmentRoot::at(root.into()));
    begin(&mut service, 1).unwrap();
}

#[test]
fn startup_recovery_failure_preserves_orphan_and_retry_reclaims_it() {
    use std::os::unix::fs::PermissionsExt;
    let (_temp, s) = service();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "attachments::tests::lifecycle::orphan_upload_process",
            "--include-ignored",
            "--nocapture",
        ])
        .env("EVORCH_ORPHAN_ROOT", s.root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Child exit proves creator death, without a fabricated PID or sleep.
    let directory = s.root.thread(thread().as_str()).unwrap();
    let staging = directory.directory("staging").unwrap();
    let meta = directory.directory("meta").unwrap();
    assert_eq!(staging.names().unwrap().len(), 1);
    std::fs::set_permissions(staging.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(s.recover_dead_uploads().is_err());
    std::fs::set_permissions(staging.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(staging.names().unwrap().len(), 1);
    s.recover_dead_uploads().unwrap();
    assert!(staging.names().unwrap().is_empty());
    assert!(meta.names().unwrap().is_empty());
    s.recover_dead_uploads().unwrap();
}
