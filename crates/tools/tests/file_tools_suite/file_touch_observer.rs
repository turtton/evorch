//! The file tools tell the context's observer what they really read and changed, and
//! nothing else about them changes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use event_bus::EventBus;
use sandbox::DirectSandbox;
use serde_json::json;
use tools::{
    ApplyPatch, Edit, FileAccess, FileAccessObserver, Read, Tool, ToolExecutionContext,
    ToolExecutor, Write,
};

#[derive(Debug, Default)]
struct Recorder(Mutex<Vec<String>>);

impl Recorder {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

impl FileAccessObserver for Recorder {
    fn observe(&self, access: FileAccess<'_>) {
        let (kind, path) = match access {
            FileAccess::Read(path) => ("read", path),
            FileAccess::Written(path) => ("written", path),
            FileAccess::Removed(path) => ("removed", path),
        };
        self.0
            .lock()
            .unwrap()
            .push(format!("{kind} {}", path.display()));
    }
}

fn observed() -> (Arc<Recorder>, ToolExecutionContext) {
    let recorder = Arc::new(Recorder::default());
    let context = ToolExecutionContext {
        run_id: "run-1".into(),
        file_access_observer: Some(recorder.clone()),
        ..Default::default()
    };
    (recorder, context)
}

fn entry(kind: &str, path: &Path) -> String {
    format!("{kind} {}", path.display())
}

fn patch(body: &str) -> String {
    format!("*** Begin Patch\n{body}\n*** End Patch")
}

#[tokio::test]
async fn file_touch_read_reports_only_a_successful_read() {
    let dir = tempfile::tempdir().unwrap();
    let text = dir.path().join("text.txt");
    let empty = dir.path().join("empty.txt");
    let broken_image = dir.path().join("broken.png");
    std::fs::write(&text, "one\ntwo\n").unwrap();
    std::fs::write(&empty, "").unwrap();
    std::fs::write(&broken_image, "not a png").unwrap();
    let (recorder, context) = observed();

    for path in [&text, &empty] {
        let result = Read
            .execute_with_context(&context, json!({"path": path, "offset": 2}))
            .await
            .unwrap();
        assert!(!result.is_error);
    }
    assert_eq!(
        recorder.take(),
        [entry("read", &text), entry("read", &empty)],
        "partial and empty reads still count"
    );

    let missing = Read
        .execute_with_context(&context, json!({"path": dir.path().join("missing.txt")}))
        .await;
    assert!(missing.is_err());
    let image = Read
        .execute_with_context(&context, json!({"path": broken_image}))
        .await
        .unwrap();
    assert!(image.is_error, "{}", image.content);
    assert!(recorder.take().is_empty());
}

#[tokio::test]
async fn file_touch_edit_reports_only_a_real_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "alpha\nbeta\n").unwrap();
    let (recorder, context) = observed();

    Edit.execute_with_context(
        &context,
        json!({"path": path, "old_string": "alpha", "new_string": "gamma"}),
    )
    .await
    .unwrap();
    assert_eq!(recorder.take(), [entry("written", &path)]);

    let unmatched = Edit
        .execute_with_context(
            &context,
            json!({"path": path, "old_string": "missing", "new_string": "x"}),
        )
        .await;
    assert!(unmatched.is_err());
    let same = Edit
        .execute_with_context(
            &context,
            json!({"path": path, "edits": [{"old_string": "gamma", "new_string": "gamma"}]}),
        )
        .await
        .unwrap();
    assert!(!same.is_error);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "gamma\nbeta\n");
    assert!(
        recorder.take().is_empty(),
        "failures and no-ops are not changes"
    );
}

#[tokio::test]
async fn file_touch_write_reports_creation_and_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.txt");
    let (recorder, context) = observed();

    for _ in 0..2 {
        Write
            .execute_with_context(&context, json!({"path": path, "content": "same\n"}))
            .await
            .unwrap();
    }
    assert_eq!(
        recorder.take(),
        [entry("written", &path), entry("written", &path)]
    );

    let missing_parent = Write
        .execute_with_context(
            &context,
            json!({"path": dir.path().join("no/such/dir.txt"), "content": "x"}),
        )
        .await;
    assert!(missing_parent.is_err());
    assert!(recorder.take().is_empty());
}

#[tokio::test]
async fn file_touch_apply_patch_reports_each_committed_change() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    for name in ["update.txt", "delete.txt", "move_from.txt", "same.txt"] {
        std::fs::write(root.join(name), "old\n").unwrap();
    }
    let tool = ApplyPatch::new().with_default_cwd(root.clone());
    let (recorder, context) = observed();

    let result = tool
        .execute_with_context(
            &context,
            json!({"input": patch(
                "*** Add File: added.txt\n+new\n\
                 *** Update File: update.txt\n@@\n-old\n+changed\n\
                 *** Update File: same.txt\n@@\n-old\n+old\n\
                 *** Delete File: delete.txt\n\
                 *** Update File: move_from.txt\n*** Move to: moved/move_to.txt\n@@\n-old\n+moved"
            )}),
        )
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);

    let mut seen = recorder.take();
    seen.sort();
    let mut expected = vec![
        entry("written", &root.join("added.txt")),
        entry("written", &root.join("update.txt")),
        entry("removed", &root.join("delete.txt")),
        entry("removed", &root.join("move_from.txt")),
        entry("written", &root.join("moved/move_to.txt")),
    ];
    expected.sort();
    assert_eq!(
        seen, expected,
        "an update that changes nothing is not a change"
    );

    let rejected = tool
        .execute_with_context(
            &context,
            json!({"input": patch("*** Update File: absent.txt\n@@\n-x\n+y")}),
        )
        .await
        .unwrap();
    assert!(rejected.is_error);
    assert!(recorder.take().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn file_touch_apply_patch_reports_the_prefix_committed_before_a_failure() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("z")).unwrap();
    std::fs::set_permissions(root.join("z"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let restore = Restore(root.join("z"));
    // Directory modes may be bypassed (for example by running as root); then no write
    // fails and there is no partial commit to observe.
    if tempfile::NamedTempFile::new_in(root.join("z")).is_ok() {
        drop(restore);
        return;
    }
    let tool = ApplyPatch::new().with_default_cwd(root.clone());
    let (recorder, context) = observed();

    let result = tool
        .execute_with_context(
            &context,
            json!({"input": patch("*** Add File: a.txt\n+first\n*** Add File: z/second.txt\n+second")}),
        )
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(recorder.take(), [entry("written", &root.join("a.txt"))]);
    drop(restore);
}

#[cfg(unix)]
struct Restore(PathBuf);

#[cfg(unix)]
impl Drop for Restore {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

#[tokio::test]
async fn file_touch_executor_passes_the_observer_and_leaves_results_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("a.txt"), "hello\n").unwrap();
    let executor = ToolExecutor::with_standard_tools_in(
        Arc::new(EventBus::new(16)),
        Arc::new(DirectSandbox::new_unchecked()),
        Some(root.clone()),
    );
    let (recorder, context) = observed();
    let plain = ToolExecutionContext {
        run_id: "run-2".into(),
        ..Default::default()
    };

    let args = json!({"path": "a.txt"});
    let with = executor
        .execute(&context, "read", "c1", args.clone())
        .await
        .unwrap();
    let without = executor.execute(&plain, "read", "c2", args).await.unwrap();
    assert_eq!(with.content, without.content);
    assert_eq!(with.detail, without.detail);
    assert_eq!(recorder.take(), [entry("read", &root.join("a.txt"))]);

    executor
        .execute(
            &context,
            "edit",
            "c3",
            json!({"path": "a.txt", "old_string": "hello", "new_string": "bye"}),
        )
        .await
        .unwrap();
    assert_eq!(recorder.take(), [entry("written", &root.join("a.txt"))]);
}
