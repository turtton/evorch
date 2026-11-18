//! The control-reconnection grace of a disconnected client's renderer preview.
//!
//! The browser leaf owns the deadline; the core only reads a copy of it. While
//! it lies in the future the process is retained (status reports
//! `RendererReconnectGrace`, the idle grace does not run). It is neither a
//! connected client nor running work: the helper's watcher is not counted, and
//! once the deadline has passed the ordinary idle grace starts from zero.
use tokio::time::Instant;
use workbench_host::areas::browser::RendererReconnectGrace;

/// The end of the grace if it is still running at `now`.
pub(crate) fn deadline(grace: RendererReconnectGrace, now: Instant) -> Option<Instant> {
    grace.until.filter(|until| *until > now)
}

/// The grace of the host's renderer previews, as a copy.
pub(super) fn current(
    host: &workbench_host::bootstrap::BootstrappedHost,
    now: Instant,
) -> Option<Instant> {
    deadline(host.host.lock().unwrap().renderer_reconnect_grace(), now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::IdleGrace;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn a_running_grace_retains_and_the_idle_grace_starts_only_after_it() {
        let reconnect = Duration::from_secs(30);
        let idle_grace = Duration::from_secs(60);
        let now = Instant::now();
        let grace = RendererReconnectGrace {
            until: Some(now + reconnect),
        };
        let mut idle = IdleGrace::new(idle_grace);
        // Retained: neither a client nor work, yet no exit deadline exists.
        assert!(deadline(grace, now).is_some());
        assert!(
            idle.update(false, 0, deadline(grace, now).is_some())
                .is_none()
        );
        tokio::time::advance(reconnect - Duration::from_nanos(1)).await;
        assert_eq!(deadline(grace, Instant::now()), Some(now + reconnect));
        assert!(idle.update(false, 0, true).is_none());
        // Over at the deadline: idle starts counting from there.
        tokio::time::advance(Duration::from_nanos(1)).await;
        assert_eq!(deadline(grace, Instant::now()), None);
        assert_eq!(
            idle.update(false, 0, false),
            Some(now + reconnect + idle_grace)
        );
        assert_eq!(deadline(RendererReconnectGrace::default(), now), None);
    }

    // The composition below follows `lifecycle/idle.rs`: a masked child process,
    // virtual time and the real driver. No test waits for wall-clock time.
    use std::{future::Future, pin::Pin, sync::Arc, task::Poll};
    use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
    use workbench_protocol::{
        commands::Command,
        handshake::RetentionReason,
        ids::{RequestId, SessionId},
        namespaces::browser::BrowserCommand,
    };

    struct Idle;
    #[async_trait::async_trait]
    impl runtime::AgentModel for Idle {
        async fn complete(
            &self,
            _: &runtime::AgentInvocationContext,
            _: runtime::Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
            unreachable!("no turn runs in this fixture")
        }
        fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
            "renderer-grace".into()
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
    fn signals() -> crate::lifecycle::OperatorSignals {
        #[cfg(target_os = "linux")]
        {
            crate::lifecycle::OperatorSignals::new(crate::signal_test_entry::mask()).unwrap()
        }
        #[cfg(not(target_os = "linux"))]
        {
            crate::lifecycle::OperatorSignals::new().unwrap()
        }
    }
    async fn masked_child() -> bool {
        #[cfg(target_os = "linux")]
        if std::env::var_os("EVORCH_MASKED_TEST_CHILD").is_none() {
            let name = std::thread::current().name().unwrap().to_owned();
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env("EVORCH_MASKED_TEST_CHILD", "1")
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return true;
        }
        false
    }
    async fn assert_pending(future: Pin<&mut impl Future<Output = std::io::Result<()>>>) {
        let mut future = future;
        let state = std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await;
        assert!(state.is_pending(), "core exited while retained: {state:?}");
    }

    #[tokio::test]
    async fn a_detached_preview_retains_the_core_for_the_grace_and_then_the_core_goes_idle() {
        if masked_child().await {
            return;
        }
        let reconnect = Duration::from_secs(30);
        let idle_grace = Duration::from_secs(60);
        let dir = test_socket_dir::SplitDir::new();
        let mut host = BootstrappedHost::open_with_test_seams(
            BootstrapOptions {
                config_dir: dir.path().into(),
                project_root: dir.path().into(),
                ownership_root: dir.sockets().into(),
            },
            TestSeams {
                model: Arc::new(Idle),
                sandbox: Arc::new(Direct),
            },
        )
        .await
        .unwrap();
        host.config.core.idle_grace_secs = idle_grace.as_secs();
        let listener = tokio::net::UnixListener::bind(dir.sockets().join("grace.sock")).unwrap();
        let backend = Arc::new(crate::backend::HostBackend::new(
            host.host.clone(),
            "grace-core".into(),
        ));
        tokio::time::pause();
        // A desktop client opens a preview session and goes away.
        let provenance = workbench_host::provenance::local_desktop();
        let connection = host
            .host
            .lock()
            .unwrap()
            .connections()
            .open(provenance.client_id.clone().unwrap());
        host.host
            .lock()
            .unwrap()
            .handle_request(
                &SessionId::new("grace").unwrap(),
                &RequestId::new("grace-session").unwrap(),
                Command::Browser(BrowserCommand::CreateRendererSession {}),
                provenance,
            )
            .unwrap();
        drop(connection);
        let mut driver = crate::lifecycle::LifecycleDriver::new(&host);
        let retained = |backend: &crate::backend::HostBackend| {
            let status = backend.hello().status;
            // The grace is a retention reason, never a client or running work.
            assert_eq!(status.running.clients, 0);
            assert!(
                !status
                    .retention_reasons
                    .contains(&RetentionReason::ConnectedClients)
            );
            status
                .retention_reasons
                .contains(&RetentionReason::RendererReconnectGrace)
        };
        {
            let running = driver.run_with_web(&host, backend.clone(), listener, None, signals());
            tokio::pin!(running);
            assert_pending(running.as_mut()).await;
            assert!(retained(&backend));
            tokio::time::advance(reconnect - Duration::from_nanos(1)).await;
            assert_pending(running.as_mut()).await;
            assert!(retained(&backend));
            // The grace ends: the ordinary idle grace starts from zero.
            tokio::time::advance(Duration::from_nanos(1)).await;
            assert_pending(running.as_mut()).await;
            assert!(!retained(&backend));
            tokio::time::advance(idle_grace - Duration::from_nanos(1)).await;
            assert_pending(running.as_mut()).await;
            tokio::time::advance(Duration::from_nanos(1)).await;
            running.await.unwrap();
        }
        host.shutdown().await.unwrap();
    }
}
