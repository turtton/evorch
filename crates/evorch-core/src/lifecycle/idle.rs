//! Composition tests await transport/runtime completion, never elapsed real time.
use super::*;
use std::{future::Future, pin::Pin, sync::Arc, task::Poll};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::{framing::encode_frame, handshake::*, ids::ClientId};

#[derive(Default)]
struct Model(tokio::sync::Notify);
#[async_trait::async_trait]
impl runtime::AgentModel for Model {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.0.notified().await;
        unreachable!("the fixture explicitly stops this active turn")
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "idle-lifecycle".into()
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
async fn open(dir: &test_socket_dir::SplitDir) -> BootstrappedHost {
    let mut host = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: dir.path().into(),
            project_root: dir.path().into(),
            ownership_root: dir.sockets().into(),
        },
        TestSeams {
            model: Arc::new(Model::default()),
            sandbox: Arc::new(Direct),
        },
    )
    .await
    .unwrap();
    host.config.core.idle_grace_secs = 60;
    host
}
fn signals() -> OperatorSignals {
    #[cfg(target_os = "linux")]
    {
        OperatorSignals::new(crate::signal_test_entry::mask()).unwrap()
    }
    #[cfg(not(target_os = "linux"))]
    {
        OperatorSignals::new().unwrap()
    }
}

async fn masked_child() -> bool {
    #[cfg(target_os = "linux")]
    if std::env::var_os("EVORCH_MASKED_TEST_CHILD").is_none() {
        // The existing entry constructor installs the signal mask before
        // libtest/Tokio create threads, just as the production core does.
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
    assert!(
        state.is_pending(),
        "core exited while a retention reason existed: {state:?}"
    );
}
async fn handshake(stream: &mut tokio::net::UnixStream, name: &str) {
    let hello = ClientHello {
        supported_versions: vec![ProtocolVersion::CURRENT],
        required_capabilities: vec![],
        optional_capabilities: vec![],
        client_build: BuildInfo {
            name: name.into(),
            version: "test".into(),
            revision: None,
        },
        client_instance_id: ClientId::new(name).unwrap(),
        desired_core: None,
    };
    stream
        .write_all(&encode_frame(&hello, WireLimits::default().max_frame_bytes).unwrap())
        .await
        .unwrap();
    let mut response = vec![0; stream.read_u32().await.unwrap() as usize];
    stream.read_exact(&mut response).await.unwrap();
    assert!(matches!(
        serde_json::from_slice::<HandshakeReply>(&response).unwrap(),
        HandshakeReply::Accepted(_)
    ));
}

#[tokio::test]
async fn no_clients_and_no_work_exit_after_the_idle_grace() {
    if masked_child().await {
        return;
    }
    let dir = test_socket_dir::SplitDir::new();
    let mut host = open(&dir).await;
    let listener = tokio::net::UnixListener::bind(dir.sockets().join("idle.sock")).unwrap();
    let backend = Arc::new(crate::backend::HostBackend::new(
        host.host.clone(),
        "idle-core".into(),
    ));
    let mut driver = LifecycleDriver::new(&host);
    tokio::time::pause();
    {
        let running = driver.run_with_web(&host, backend, listener, None, signals());
        tokio::pin!(running);
        assert_pending(running.as_mut()).await;
        tokio::time::advance(Duration::from_secs(60) - Duration::from_nanos(1)).await;
        assert_pending(running.as_mut()).await;
        tokio::time::advance(Duration::from_nanos(1)).await;
        running.await.unwrap();
    }
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn gui_and_cli_connections_each_retain_until_the_last_disconnect() {
    if masked_child().await {
        return;
    }
    let dir = test_socket_dir::SplitDir::new();
    let mut host = open(&dir).await;
    // Hold the core while the OS admits the sockets, before pausing its clock.
    let active = host.runtime.delegate_background(
        runtime::Role::Worker,
        "hold admission".into(),
        Default::default(),
    );
    let socket = dir.sockets().join("idle.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let backend = Arc::new(crate::backend::HostBackend::new(
        host.host.clone(),
        "idle-core".into(),
    ));
    let mut driver = LifecycleDriver::new(&host);
    {
        let running = driver.run_with_web(&host, backend.clone(), listener, None, signals());
        tokio::pin!(running);
        let mut gui = tokio::net::UnixStream::connect(&socket).await.unwrap();
        tokio::select! {
            result = &mut running => panic!("exited during GUI admission: {result:?}"),
            _ = handshake(&mut gui, "gui") => {}
        }
        let mut cli = tokio::net::UnixStream::connect(&socket).await.unwrap();
        tokio::select! {
            result = &mut running => panic!("exited during CLI admission: {result:?}"),
            _ = handshake(&mut cli, "cli") => {}
        }
        host.runtime
            .stop(active, runtime::StopScope::SelfOnly)
            .unwrap();
        host.runtime.wait(active).await.unwrap();
        assert!(!host.busy());
        assert_eq!(backend.hello().status.running.clients, 2);
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(120)).await;
        assert_pending(running.as_mut()).await;
        drop(gui);
        tokio::time::advance(Duration::from_secs(120)).await;
        assert_pending(running.as_mut()).await;
        // The remaining CLI connection still serves a reply, not just a stale count.
        let frame = workbench_protocol::envelope::RequestEnvelope {
            request_id: workbench_protocol::ids::RequestId::new("ping").unwrap(),
            session_id: workbench_protocol::ids::SessionId::new("cli").unwrap(),
            body: workbench_protocol::envelope::RequestBody::Control(
                workbench_protocol::envelope::Control::Ping {
                    nonce: 41_u64.into(),
                },
            ),
        };
        cli.write_all(&encode_frame(&frame, WireLimits::default().max_frame_bytes).unwrap())
            .await
            .unwrap();
        let reply = async {
            let mut bytes = vec![0; cli.read_u32().await.unwrap() as usize];
            cli.read_exact(&mut bytes).await.unwrap();
            use workbench_protocol::envelope::{ReplyPayload, ReplyResult, ServerFrame};
            let ServerFrame::Reply(reply) = serde_json::from_slice(&bytes).unwrap() else {
                panic!("expected pong")
            };
            assert!(
                matches!(reply.result, ReplyResult::Success(ReplyPayload::Pong { nonce }) if nonce == 41_u64.into())
            );
        };
        tokio::select! {
            result = &mut running => panic!("exited with one CLI: {result:?}"),
            _ = reply => {}
        }
        assert_eq!(backend.hello().status.running.clients, 1);
        drop(cli);
        running.await.unwrap();
    }
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_enabled_web_listener_is_idle_but_a_live_web_connection_retains() {
    if masked_child().await {
        return;
    }
    let dir = test_socket_dir::SplitDir::new();
    let mut host = open(&dir).await;
    let listener = tokio::net::UnixListener::bind(dir.sockets().join("idle.sock")).unwrap();
    let backend = Arc::new(crate::backend::HostBackend::new(
        host.host.clone(),
        "idle-core".into(),
    ));
    backend.set_web_enabled(true);
    let web = workbench_web::WebServer::start(
        workbench_web::WebServerConfig {
            enabled: true,
            bind: "127.0.0.1:0".parse().unwrap(),
            header_read_timeout: Duration::from_secs(1000),
            ..Default::default()
        },
        backend.clone(),
    )
    .await
    .unwrap()
    .unwrap();
    let census = web.connected_clients();
    let stream = tokio::net::TcpStream::connect(web.local_addr())
        .await
        .unwrap();
    wait_count(&census, 1).await;
    let mut driver = LifecycleDriver::new(&host);
    {
        let running = driver.run_with_web(&host, backend.clone(), listener, Some(web), signals());
        tokio::pin!(running);
        tokio::time::pause();
        assert_pending(running.as_mut()).await;
        tokio::time::advance(Duration::from_secs(120)).await;
        assert_pending(running.as_mut()).await;
        assert_eq!(backend.hello().status.running.clients, 1);
        drop(stream);
        wait_count(&census, 0).await;
        assert_pending(running.as_mut()).await;
        tokio::time::advance(Duration::from_secs(60) - Duration::from_nanos(1)).await;
        assert_pending(running.as_mut()).await;
        tokio::time::advance(Duration::from_nanos(1)).await;
        running.await.unwrap();
        let status = backend.hello().status;
        assert_eq!(status.running.clients, 0);
        assert!(
            !status
                .retention_reasons
                .contains(&RetentionReason::WebServing)
        );
    }
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_web_connect_disconnect_between_driver_polls_restarts_the_entire_grace() {
    if masked_child().await {
        return;
    }
    let dir = test_socket_dir::SplitDir::new();
    let mut host = open(&dir).await;
    let listener = tokio::net::UnixListener::bind(dir.sockets().join("idle.sock")).unwrap();
    let backend = Arc::new(crate::backend::HostBackend::new(
        host.host.clone(),
        "idle-core".into(),
    ));
    let web = workbench_web::WebServer::start(
        workbench_web::WebServerConfig {
            enabled: true,
            bind: "127.0.0.1:0".parse().unwrap(),
            ..Default::default()
        },
        backend.clone(),
    )
    .await
    .unwrap()
    .unwrap();
    let address = web.local_addr();
    let census = web.connected_clients();
    let mut driver = LifecycleDriver::new(&host);
    {
        let running = driver.run_with_web(&host, backend, listener, Some(web), signals());
        tokio::pin!(running);
        tokio::time::pause();
        assert_pending(running.as_mut()).await;
        tokio::time::advance(Duration::from_secs(30)).await;
        // Deliberately do not poll the driver until the count returns to zero.
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        wait_count(&census, 1).await;
        drop(stream);
        wait_count(&census, 0).await;
        assert_pending(running.as_mut()).await;
        tokio::time::advance(Duration::from_secs(60) - Duration::from_nanos(1)).await;
        assert_pending(running.as_mut()).await;
        tokio::time::advance(Duration::from_nanos(1)).await;
        running.await.unwrap();
    }
    host.shutdown().await.unwrap();
}

async fn wait_count(activity: &workbench_web::ActivityObserver, expected: usize) {
    loop {
        let (count, _, generation) = activity.snapshot();
        if count == expected {
            return;
        }
        activity.changed(generation).await;
    }
}
