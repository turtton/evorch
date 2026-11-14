//! Test-binary worker uses the production driver; there is no production restart entry.
use core_instance::{ConfigIdentity, Instance, LaunchGuard};
use evorch_core::{
    backend::HostBackend,
    lifecycle::{LifecycleDriver, OperatorSignals},
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{io::Write, path::PathBuf, process::Stdio, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tools::Tool;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};

#[derive(Default)]
struct CountingModel(AtomicUsize);
#[async_trait::async_trait]
impl runtime::AgentModel for CountingModel {
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "restart-process".into()
    }
    fn requires_admission(&self) -> bool {
        false
    }
    async fn admit(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
    ) -> Result<(), runtime::RuntimeError> {
        Ok(())
    }
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "continued in this process".into(),
                }],
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: providers::FinishReason::Stop,
        })
    }
}
struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}
async fn shell_wait(root: &std::path::Path) {
    let fifo = root.join("f");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    tools::set_shell_job_handle_dir(root.join("h")).unwrap();
    let shell = tools::Shell::new(Arc::new(sandbox::DirectSandbox::new_unchecked()));
    let context = tools::ToolExecutionContext {
        run_id: "restart-process-shell".into(),
        ..Default::default()
    };
    let started = shell.execute_with_context(&context, serde_json::json!({"command":format!(r#"IFS= read -r line < '{}'; printf 'normal:%s' "$line""#, fifo.display()), "yield_ms":0})).await.unwrap();
    let identity = shell.shell_job_snapshots(&context.run_id)[0]
        .identity
        .clone();
    let mut dropped = Box::pin(shell.wait_shell_job_terminal(&identity));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(dropped.as_mut().poll(cx).is_pending()))
            .await
    );
    drop(dropped);
    emit("shell-waiting");
    let terminal = shell.wait_shell_job_terminal(&identity).await.unwrap();
    assert_eq!(terminal.exit_code, Some(0));
    assert!(!terminal.observed);
    let job = started.detail.as_ref().unwrap()["shell_job"]["job_id"]
        .as_str()
        .unwrap();
    let result = shell
        .execute_with_context(&context, serde_json::json!({"action":"poll", "job_id":job}))
        .await
        .unwrap();
    assert!(result.content.contains("normal:released"));
    assert!(shell.shell_job_snapshots(&context.run_id)[0].observed);
    shell.release_shell_jobs(&context.run_id).unwrap();
    emit("shell-finished");
}
fn emit(phase: &str) {
    println!("07A {phase}");
    std::io::stdout().flush().unwrap();
}
#[tokio::test]
#[ignore = "subprocess worker, selected explicitly by its parent test"]
async fn restart_subprocess_worker() {
    let root = PathBuf::from(std::env::var_os("EVORCH_RESTART_TEST_ROOT").unwrap());
    let config = root.join("c");
    std::fs::create_dir_all(&config).unwrap();
    let identity = ConfigIdentity::new(&config).unwrap();
    let launch = LaunchGuard::acquire(&identity).unwrap();
    let instance = Instance::acquire(&launch, std::path::Path::new("/tmp")).unwrap();
    assert!(instance.socket.as_os_str().as_encoded_bytes().len() < 108);
    let model = Arc::new(CountingModel::default());
    let mut host = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: config,
            project_root: root.clone(),
            ownership_root: root.join("o"),
        },
        TestSeams {
            model: model.clone(),
            sandbox: Arc::new(Direct),
        },
    )
    .await
    .unwrap();
    let backend = Arc::new(HostBackend::new(
        host.host.clone(),
        instance.identity.clone(),
    ));
    let signals = OperatorSignals::new().unwrap();
    let mut driver = LifecycleDriver::new(&host);
    let listener = instance.bind().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    instance.publish(backend.hello().instance_epoch.0).unwrap();
    let coordinator = driver.restart.clone();
    let runtime = host.runtime.clone();
    let calls = model.clone();
    let commands = tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        let mut attempt = None;
        let mut run = None;
        let mut expected_calls = 0;
        while let Some(line) = lines.next_line().await.unwrap() {
            match line.as_str() {
                "prepare" => {
                    run = Some(runtime.delegate_background(
                        runtime::Role::Worker,
                        "continue only after explicit cancellation".into(),
                        runtime::RunConfig::default(),
                    ));
                    let id = coordinator.request().unwrap();
                    assert_eq!(id, coordinator.request().unwrap());
                    coordinator.wait_sealed(id).await.unwrap();
                    attempt = Some(id);
                    assert_eq!(calls.0.load(Ordering::SeqCst), expected_calls);
                    emit("sealed");
                }
                "cancel" => {
                    coordinator.cancel(attempt.take().unwrap()).unwrap();
                    assert_eq!(
                        runtime.wait(run.take().unwrap()).await.unwrap(),
                        event_bus::AgentRunPhase::Done
                    );
                    expected_calls += 1;
                    assert_eq!(calls.0.load(Ordering::SeqCst), expected_calls);
                    emit("cancelled");
                }
                "shell" => shell_wait(&root).await,
                _ => panic!("unexpected test command"),
            }
        }
    });
    emit("ready");
    driver
        .run(&host, backend, listener, true, signals)
        .await
        .unwrap();
    commands.abort();
    let _ = commands.await;
    host.shutdown().await.unwrap();
    assert_eq!(
        model.0.load(Ordering::SeqCst),
        2,
        "sealed operator shutdown must not resume the third run"
    );
    emit("stopped");
    drop(instance);
    drop(launch);
}
#[tokio::test]
async fn prepare_cancel_retains_lifetime_lock_and_sigterm_uses_shared_driver() {
    let root = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "restart_subprocess_worker",
            "--nocapture",
        ])
        .env("EVORCH_RESTART_TEST_ROOT", root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    async fn reached(
        lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
        phase: &str,
    ) {
        while let Some(line) = lines.next_line().await.unwrap() {
            if let Some(actual) = line.strip_prefix("07A ") {
                assert_eq!(actual, phase);
                return;
            }
        }
        panic!("worker exited before checkpoint");
    }
    reached(&mut lines, "ready").await;
    let identity = ConfigIdentity::new(&root.path().join("c")).unwrap();
    assert!(LaunchGuard::acquire(&identity).is_err());
    let pid = child.id().unwrap();
    for command in ["prepare", "cancel", "prepare", "cancel", "shell", "prepare"] {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(format!("{command}\n").as_bytes())
            .await
            .unwrap();
        if command == "shell" {
            reached(&mut lines, "shell-waiting").await;
            let fifo = root.path().join("f");
            tokio::task::spawn_blocking(move || {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(fifo)
                    .unwrap()
                    .write_all(b"released\n")
                    .unwrap();
            })
            .await
            .unwrap();
            reached(&mut lines, "shell-finished").await;
        } else {
            reached(
                &mut lines,
                if command == "cancel" {
                    "cancelled"
                } else {
                    "sealed"
                },
            )
            .await;
        }
        assert_eq!(child.id(), Some(pid));
        assert!(child.try_wait().unwrap().is_none());
        assert!(LaunchGuard::acquire(&identity).is_err());
    }
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.id().unwrap() as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    // Tokio stdin owns a blocking reader: EOF lets that reader exit at runtime teardown.
    drop(child.stdin.take());
    reached(&mut lines, "stopped").await;
    assert!(child.wait().await.unwrap().success());
    let guard = LaunchGuard::acquire(&identity).unwrap();
    assert!(guard.lifetime_available().unwrap());
}
