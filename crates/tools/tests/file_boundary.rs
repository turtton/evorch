//! Direct file tools stay inside the sandbox boundary unless one call is
//! reviewed through the shared escalation gate (ADR 0033).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use event_bus::{EventBus, EventKind};
use sandbox::{DirectSandbox, LivePaths, PathAccess, SandboxBoundary};
use serde_json::json;
use tools::tool::Tool;
use tools::tools::shell_escalation::{EscalationDecision, FileAccessRequest, ShellEscalationGate};
use tools::tools::{Edit, Grep, Read, Write};
use tools::{ToolError, ToolExecutionContext, ToolExecutor};

const SECRET: &str = "outside-secret-token";

fn context() -> ToolExecutionContext {
    ToolExecutionContext {
        run_id: "run-1".into(),
        thread_id: None,
        call_id: None,
    }
}

struct Layout {
    _dirs: Vec<tempfile::TempDir>,
    project: PathBuf,
    scratch: PathBuf,
    cargo: PathBuf,
    readonly: PathBuf,
    outside: PathBuf,
}

fn layout() -> Layout {
    let dirs = (0..5)
        .map(|_| tempfile::tempdir().unwrap())
        .collect::<Vec<_>>();
    let path = |index: usize| dirs[index].path().canonicalize().unwrap();
    let layout = Layout {
        project: path(0),
        scratch: path(1),
        cargo: path(2),
        readonly: path(3),
        outside: path(4),
        _dirs: dirs,
    };
    std::fs::write(layout.project.join("inside.txt"), "project file").unwrap();
    std::fs::write(layout.scratch.join("tmp.txt"), "scratch file").unwrap();
    let crate_src = layout.cargo.join("registry/src/index/dep-1.0.0");
    std::fs::create_dir_all(&crate_src).unwrap();
    std::fs::write(crate_src.join("lib.rs"), "dependency source").unwrap();
    std::fs::write(layout.cargo.join("credentials.toml"), SECRET).unwrap();
    std::fs::write(layout.readonly.join("shared.txt"), "read-only file").unwrap();
    std::fs::write(layout.outside.join("secret.txt"), SECRET).unwrap();
    layout
}

fn executor(layout: &Layout) -> (Arc<ToolExecutor>, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(256));
    let executor = ToolExecutor::with_standard_tools_in(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
        Some(layout.project.clone()),
    );
    let boundary = SandboxBoundary::new(layout.project.clone())
        .cargo_cache(&layout.cargo)
        .read_only(&layout.readonly)
        .with_scratch(&layout.scratch);
    executor.set_file_boundary(&boundary);
    (Arc::new(executor), bus)
}

#[derive(Default)]
struct RecordingGate {
    approve: bool,
    requests: Mutex<Vec<FileAccessRequest>>,
}

#[async_trait::async_trait]
impl ShellEscalationGate for RecordingGate {
    async fn decide(&self, _: &ToolExecutionContext, _: &str, _: &str) -> EscalationDecision {
        EscalationDecision::Deny {
            reason: "shell is not under test".into(),
        }
    }

    async fn decide_file_access(
        &self,
        _: &ToolExecutionContext,
        request: &FileAccessRequest,
    ) -> EscalationDecision {
        self.requests.lock().unwrap().push(request.clone());
        if self.approve {
            EscalationDecision::Approve
        } else {
            EscalationDecision::Deny {
                reason: "reviewer declined".into(),
            }
        }
    }
}

fn with_gate(executor: &ToolExecutor, approve: bool) -> Arc<RecordingGate> {
    let gate = Arc::new(RecordingGate {
        approve,
        ..RecordingGate::default()
    });
    executor.set_shell_escalation(gate.clone(), Arc::new(DirectSandbox::new_unchecked()));
    gate
}

fn denial(result: Result<tools::ToolResult, ToolError>) -> String {
    match result {
        Err(ToolError::ExecutionDenied { reason, .. }) => reason,
        other => panic!("expected a denial, got {other:?}"),
    }
}

async fn read(executor: &ToolExecutor, path: &Path) -> Result<tools::ToolResult, ToolError> {
    executor
        .execute(&context(), "read", "read", json!({"path": path}))
        .await
}

#[tokio::test]
async fn boundary_paths_are_usable_without_escalation() {
    // Given: a project, a thread scratch, a Cargo registry source and a read-only region.
    let layout = layout();
    let (executor, _) = executor(&layout);

    // When / Then: reads inside every region succeed without a gate.
    for (path, text) in [
        (layout.project.join("inside.txt"), "project file"),
        (PathBuf::from("inside.txt"), "project file"),
        (layout.scratch.join("tmp.txt"), "scratch file"),
        (
            layout.cargo.join("registry/src/index/dep-1.0.0/lib.rs"),
            "dependency source",
        ),
        (layout.readonly.join("shared.txt"), "read-only file"),
    ] {
        let result = read(&executor, &path).await.unwrap();
        assert!(result.content.contains(text), "{}", path.display());
    }
    let grep = executor
        .execute(
            &context(),
            "grep",
            "grep",
            json!({"pattern":"dependency", "path": layout.cargo.join("registry/src")}),
        )
        .await
        .unwrap();
    assert!(grep.content.contains("dependency source"));

    // Then: writes are accepted in the project and the scratch, including new files.
    for path in [
        layout.project.join("new.txt"),
        layout.scratch.join("new.txt"),
    ] {
        executor
            .execute(
                &context(),
                "write",
                "write",
                json!({"path": path, "content":"written"}),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "written");
    }
}

#[tokio::test]
async fn outside_and_read_only_targets_are_denied_with_both_remedies_and_no_content() {
    // Given: an executor with a boundary and a subscriber recording every event.
    let layout = layout();
    let (executor, bus) = executor(&layout);
    let mut events = bus.subscribe();
    let secret = layout.outside.join("secret.txt");

    // When: reading outside, reading Cargo credentials, and writing a read-only region.
    let outside = denial(read(&executor, &secret).await);
    let credentials = denial(read(&executor, &layout.cargo.join("credentials.toml")).await);
    let read_only = denial(
        executor
            .execute(
                &context(),
                "write",
                "write",
                json!({"path": layout.readonly.join("shared.txt"), "content":"changed"}),
            )
            .await,
    );
    let edit = denial(
        executor
            .execute(
                &context(),
                "edit",
                "edit",
                json!({"path": layout.cargo.join("registry/src/index/dep-1.0.0/lib.rs"), "old_string":"dependency", "new_string":"patched"}),
            )
            .await,
    );

    // Then: every denial names both ways forward, and no outside content leaks.
    for reason in [&outside, &credentials, &read_only, &edit] {
        assert!(
            reason.contains("sandbox_access: \"unsandboxed\""),
            "{reason}"
        );
        assert!(reason.contains("justification"), "{reason}");
        assert!(reason.contains("allowed directory"), "{reason}");
    }
    assert!(outside.contains("outside the sandbox boundary"));
    assert!(read_only.contains("read-only"));
    assert_eq!(
        std::fs::read_to_string(layout.readonly.join("shared.txt")).unwrap(),
        "read-only file"
    );
    for event in events.drain_pending_snapshot() {
        if let EventKind::Tool(event) = event.kind {
            assert!(!serde_json::to_string(&event).unwrap().contains(SECRET));
        }
    }
}

#[tokio::test]
async fn parent_components_links_and_special_files_cannot_leave_the_boundary() {
    // Given: links between the project and an outside directory.
    let layout = layout();
    let (executor, _) = executor(&layout);
    std::os::unix::fs::symlink(&layout.outside, layout.project.join("escape")).unwrap();
    std::os::unix::fs::symlink(
        layout.outside.join("missing.txt"),
        layout.project.join("dangling"),
    )
    .unwrap();
    std::os::unix::fs::symlink(&layout.project, layout.outside.join("into-project")).unwrap();
    let socket = layout.project.join("agent.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

    // When / Then: `..`, a link out of the project and a dangling link are outside.
    let parent = layout
        .project
        .join("..")
        .join(layout.outside.file_name().unwrap())
        .join("secret.txt");
    for path in [
        parent.clone(),
        layout.project.join("escape/secret.txt"),
        PathBuf::from("escape/secret.txt"),
    ] {
        let reason = denial(read(&executor, &path).await);
        assert!(reason.contains("outside the sandbox boundary"), "{reason}");
    }
    for path in [
        layout.project.join("escape/new.txt"),
        layout.project.join("dangling"),
    ] {
        denial(
            executor
                .execute(
                    &context(),
                    "write",
                    "write",
                    json!({"path": path, "content":"x"}),
                )
                .await,
        );
        assert!(!layout.outside.join("new.txt").exists());
        assert!(!layout.outside.join("missing.txt").exists());
    }

    // Then: a link placed outside that resolves into the project is judged by its target.
    let through = read(&executor, &layout.outside.join("into-project/inside.txt"))
        .await
        .unwrap();
    assert!(through.content.contains("project file"));

    // Then: special files are refused even inside the project.
    let reason = denial(read(&executor, &socket).await);
    assert!(
        reason.contains("not a regular file or directory"),
        "{reason}"
    );
}

#[tokio::test]
async fn grep_does_not_follow_links_out_of_the_boundary() {
    // Given: a project directory containing links to an outside file and directory.
    let layout = layout();
    let (executor, _) = executor(&layout);
    std::os::unix::fs::symlink(
        layout.outside.join("secret.txt"),
        layout.project.join("link.txt"),
    )
    .unwrap();
    std::os::unix::fs::symlink(&layout.outside, layout.project.join("linked-dir")).unwrap();

    // When: searching the project for the outside content.
    let result = executor
        .execute(
            &context(),
            "grep",
            "grep",
            json!({"pattern": SECRET, "path": layout.project}),
        )
        .await
        .unwrap();

    // Then: no outside content is returned.
    assert!(!result.content.contains(SECRET), "{}", result.content);
}

#[tokio::test]
async fn unsandboxed_access_requires_justification_and_a_gate_approval_per_call() {
    // Given: an outside file and a boundary-only executor.
    let layout = layout();
    let (executor, _) = executor(&layout);
    let secret = layout.outside.join("secret.txt");
    let args = |justification: &str| json!({"path": secret, "sandbox_access":"unsandboxed", "justification": justification});

    // When / Then: an empty justification and a missing gate are refused before review.
    let empty = denial(executor.execute(&context(), "read", "r", args("  ")).await);
    assert!(empty.contains("justification is required"), "{empty}");
    let missing = denial(
        executor
            .execute(&context(), "read", "r", args("need it"))
            .await,
    );
    assert!(missing.contains("no escalation gate"), "{missing}");

    // When: the gate declines.
    let declining = with_gate(&executor, false);
    let declined = denial(
        executor
            .execute(&context(), "read", "r", args("need it"))
            .await,
    );

    // Then: the reviewer's reason is returned, with no file content.
    assert!(declined.contains("reviewer declined"), "{declined}");
    assert!(!declined.contains(SECRET));
    assert_eq!(declining.requests.lock().unwrap().len(), 1);

    // When: the gate approves two calls.
    let approving = with_gate(&executor, true);
    for _ in 0..2 {
        let result = executor
            .execute(&context(), "read", "r", args("user asked for it"))
            .await
            .unwrap();
        assert!(result.content.contains(SECRET));
    }

    // Then: each call was reviewed on its own, with the canonical path and reason.
    let requests = approving.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tool, "read");
    assert_eq!(requests[0].paths, std::slice::from_ref(&secret));
    assert!(!requests[0].write);
    assert_eq!(requests[0].justification, "user asked for it");
    assert_eq!(requests[0].change, None);
}

#[tokio::test]
async fn unsandboxed_access_inside_the_boundary_runs_without_review() {
    // Given: a declining gate, and a read-only allowed directory next to the project.
    let layout = layout();
    let (executor, _) = executor(&layout);
    let allowed = LivePaths::default();
    allowed.set(vec![layout.outside.clone()]);
    executor.set_file_boundary(
        &SandboxBoundary::new(layout.project.clone())
            .read_only(&layout.readonly)
            .with_scratch(&layout.scratch)
            .with_allowed(allowed, PathAccess::ReadOnly),
    );
    let gate = with_gate(&executor, false);
    let unsandboxed = |mut args: serde_json::Value| {
        args["sandbox_access"] = json!("unsandboxed");
        args["justification"] = json!("review the allowed directory");
        args
    };

    // When: reads in the project, a read-only region and the allowed directory,
    // and a write in the project, all ask for unsandboxed access.
    for path in [
        layout.project.join("inside.txt"),
        layout.readonly.join("shared.txt"),
        layout.outside.join("secret.txt"),
    ] {
        executor
            .execute(&context(), "read", "r", unsandboxed(json!({"path": path})))
            .await
            .unwrap();
    }
    executor
        .execute(
            &context(),
            "grep",
            "g",
            unsandboxed(json!({"pattern": SECRET, "path": layout.outside})),
        )
        .await
        .unwrap();
    executor
        .execute(
            &context(),
            "write",
            "w",
            unsandboxed(json!({"path": layout.project.join("new.txt"), "content":"written"})),
        )
        .await
        .unwrap();

    // Then: every call ran as if isolated, and the reviewer was never consulted.
    assert_eq!(
        std::fs::read_to_string(layout.project.join("new.txt")).unwrap(),
        "written"
    );
    assert!(gate.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn escalation_review_lists_only_the_paths_outside_the_boundary() {
    // Given: an approving gate.
    let layout = layout();
    let (executor, _) = executor(&layout);
    let gate = with_gate(&executor, true);
    let secret = layout.outside.join("secret.txt");

    // When: a read names an outside path, with in-boundary paths in its aliases.
    executor
        .execute(
            &context(),
            "read",
            "r",
            json!({"path": secret, "file": layout.project, "target": layout.scratch.join("tmp.txt"),
                "sandbox_access":"unsandboxed", "justification":"user asked for it"}),
        )
        .await
        .unwrap();

    // Then: the reviewer is asked only about the path that leaves the boundary.
    let requests = gate.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].paths, std::slice::from_ref(&secret));
}

#[test]
fn file_tool_access_description_counts_allowed_directories_inside_the_boundary() {
    // Given: the standard file tools.
    let tools: [Box<dyn Tool>; 4] = [
        Box::new(Read),
        Box::new(Grep),
        Box::new(Write::default()),
        Box::new(Edit::default()),
    ];

    // When / Then: each sandbox_access description says allowed directories are inside.
    for tool in tools {
        let schema = tool.schema();
        let description = schema["properties"]["sandbox_access"]["description"]
            .as_str()
            .unwrap();
        assert!(
            description.contains("allowed directories"),
            "{}: {description}",
            tool.name()
        );
    }
}

#[tokio::test]
async fn write_escalation_shows_bounded_change_to_the_reviewer() {
    // Given: an approving gate and a large write through a link to an outside directory.
    let layout = layout();
    let (executor, _) = executor(&layout);
    let gate = with_gate(&executor, true);
    std::os::unix::fs::symlink(&layout.outside, layout.project.join("escape")).unwrap();
    let content = "x".repeat(10_000);

    // When: writing and editing outside the boundary with escalation.
    executor
        .execute(
            &context(),
            "write",
            "w",
            json!({"path": layout.project.join("escape/notes.txt"), "content": content, "sandbox_access":"unsandboxed", "justification":"user asked"}),
        )
        .await
        .unwrap();
    executor
        .execute(
            &context(),
            "edit",
            "e",
            json!({"path": layout.outside.join("notes.txt"), "old_string":"xx", "new_string":"yy", "sandbox_access":"unsandboxed", "justification":"user asked"}),
        )
        .await
        .unwrap();

    // Then: the reviewer saw the resolved target and a bounded preview.
    let requests = gate.requests.lock().unwrap();
    assert_eq!(requests[0].paths, [layout.outside.join("notes.txt")]);
    assert!(requests[0].write);
    let change = requests[0].change.as_deref().unwrap();
    assert!(
        change.len() < 4100 && change.contains("more bytes"),
        "{change}"
    );
    let edit = requests[1].change.as_deref().unwrap();
    assert!(edit.contains("old_string:\nxx") && edit.contains("new_string:\nyy"));
    assert!(
        std::fs::read_to_string(layout.outside.join("notes.txt"))
            .unwrap()
            .starts_with("yy")
    );
}

#[tokio::test]
async fn benchmark_workspace_never_escalates() {
    // Given: a benchmark executor with an approving gate.
    let layout = layout();
    let (executor, _) = executor(&layout);
    executor
        .set_workspace_boundary(layout.project.clone())
        .unwrap();
    let gate = with_gate(&executor, true);

    // When: a file tool asks to leave the recorded workspace.
    let reason = denial(
        executor
            .execute(
                &context(),
                "read",
                "r",
                json!({"path": layout.project.join("inside.txt"), "sandbox_access":"unsandboxed", "justification":"x"}),
            )
            .await,
    );

    // Then: it is refused without consulting the gate.
    assert!(reason.contains("benchmark"), "{reason}");
    assert!(gate.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn allowed_directories_apply_to_the_same_executor_as_they_change() {
    // Given: one executor per access whose boundary follows a shared, initially empty list.
    let layout = layout();
    let allowed = LivePaths::default();
    let executors = [PathAccess::ReadWrite, PathAccess::ReadOnly].map(|access| {
        let executor = ToolExecutor::with_standard_tools_in(
            Arc::new(EventBus::new(16)),
            Arc::new(DirectSandbox::new_unchecked()),
            Some(layout.project.clone()),
        );
        executor.set_file_boundary(
            &SandboxBoundary::new(layout.project.clone()).with_allowed(allowed.clone(), access),
        );
        executor
    });
    let [writer, reader] = &executors;
    let secret = layout.outside.join("secret.txt");
    let notes = layout.outside.join("notes.txt");
    let write = async |executor: &ToolExecutor, content: &str| {
        executor
            .execute(
                &context(),
                "write",
                "w",
                json!({"path": notes, "content": content}),
            )
            .await
    };
    assert!(denial(read(writer, &secret).await).contains("outside the sandbox boundary"));

    // When: the directory is declared after the executors were configured.
    allowed.set(vec![layout.outside.clone()]);

    // Then: both read it, only the writing role may change it.
    for executor in [writer, reader] {
        assert!(
            read(executor, &secret)
                .await
                .unwrap()
                .content
                .contains(SECRET)
        );
    }
    write(writer, "from writer").await.unwrap();
    assert!(denial(write(reader, "from reader").await).contains("read-only"));
    assert_eq!(
        std::fs::read_to_string(layout.outside.join("notes.txt")).unwrap(),
        "from writer"
    );

    // When / Then: once the declaration is withdrawn, the next call is outside again.
    allowed.set(Vec::new());
    for executor in [writer, reader] {
        assert!(denial(read(executor, &secret).await).contains("outside the sandbox boundary"));
    }
}

/// A page whose outline names one file input, `e5`, and that records the
/// files Chromium would be given.
#[derive(Default)]
struct UploadPage {
    files: Mutex<Vec<serde_json::Value>>,
}

#[async_trait::async_trait]
impl browser::agent::CdpTarget for UploadPage {
    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, browser::agent::AgentError> {
        if method == "DOM.setFileInputFiles" {
            self.files.lock().unwrap().push(params["files"].clone());
        }
        Ok(match method {
            "Accessibility.getFullAXTree" => json!({"nodes": [{
                "nodeId": "1", "role": {"value": "button"}, "name": {"value": "Choose file"},
                "backendDOMNodeId": 5
            }]}),
            _ => json!({}),
        })
    }

    async fn navigate(&self, _url: &str) -> Result<(), browser::agent::AgentError> {
        Ok(())
    }
}

struct OnePage(Arc<browser::agent::PageAgent>);

#[async_trait::async_trait]
impl browser::agent::PageProvider for OnePage {
    async fn page(
        &self,
        _key: &str,
    ) -> Result<Arc<browser::agent::PageAgent>, browser::agent::AgentError> {
        Ok(Arc::clone(&self.0))
    }

    async fn open_page(&self, _key: &str) -> Option<Arc<browser::agent::PageAgent>> {
        Some(Arc::clone(&self.0))
    }

    async fn close(&self, _key: &str) -> bool {
        false
    }

    fn release(&self, _key: &str) {}
}

async fn upload(executor: &ToolExecutor, path: PathBuf) -> Result<tools::ToolResult, ToolError> {
    executor
        .execute(
            &context(),
            "browser_upload_file",
            "upload",
            json!({"ref": "e5", "paths": [path]}),
        )
        .await
}

#[tokio::test]
async fn browser_uploads_take_only_files_the_run_sandbox_may_read() {
    // Given: a run boundary, a link inside it that points outside, and a
    // browser page with a file input.
    let layout = layout();
    std::os::unix::fs::symlink(
        layout.outside.join("secret.txt"),
        layout.project.join("escape.txt"),
    )
    .unwrap();
    let page = Arc::new(UploadPage::default());
    let agent = Arc::new(browser::agent::PageAgent::new(page.clone()));
    agent.snapshot().await.unwrap();
    let browsing = |bounded: bool| {
        let executor = ToolExecutor::with_standard_tools_in(
            Arc::new(EventBus::new(64)),
            Arc::new(DirectSandbox::new_unchecked()),
            Some(layout.project.clone()),
        )
        .with_policy(
            sandbox::ApprovalPolicy::standard(sandbox::ApprovalMode::OnRequest)
                .with_override("browser_upload_file", sandbox::PolicyDecision::AutoAllow),
        )
        .with_browser_provider(Arc::new(OnePage(Arc::clone(&agent))), Default::default());
        if bounded {
            executor.set_file_boundary(
                &SandboxBoundary::new(layout.project.clone())
                    .cargo_cache(&layout.cargo)
                    .read_only(&layout.readonly)
                    .with_scratch(&layout.scratch),
            );
        }
        executor
    };
    let bounded = browsing(true);

    // When: the agent uploads files inside, outside and through the link,
    // and once from an executor without a boundary.
    let inside = upload(&bounded, layout.project.join("inside.txt"))
        .await
        .unwrap();
    let read_only = upload(&bounded, layout.readonly.join("shared.txt"))
        .await
        .unwrap();
    let outside = denial(upload(&bounded, layout.outside.join("secret.txt")).await);
    let linked = denial(upload(&bounded, layout.project.join("escape.txt")).await);
    let cargo_secret = denial(upload(&bounded, layout.cargo.join("credentials.toml")).await);
    let unbounded = denial(upload(&browsing(false), layout.project.join("inside.txt")).await);

    // Then: only readable files reached the page, checked after resolving links.
    assert!(!inside.is_error, "{}", inside.content);
    assert!(!read_only.is_error, "{}", read_only.content);
    for reason in [&outside, &linked, &cargo_secret] {
        assert!(reason.contains("cannot be uploaded"), "{reason}");
    }
    assert!(
        linked.contains(&layout.outside.join("secret.txt").display().to_string()),
        "the link is judged by its target: {linked}"
    );
    assert!(
        unbounded.contains("needs the run's sandbox boundary"),
        "{unbounded}"
    );
    assert_eq!(
        *page.files.lock().unwrap(),
        [
            json!([layout.project.join("inside.txt")]),
            json!([layout.readonly.join("shared.txt")]),
        ]
    );
}
