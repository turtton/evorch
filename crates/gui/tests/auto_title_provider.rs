use gui::{
    app::WorkbenchState,
    fixture::DemoSource,
    headless::HeadlessWorkbench,
    model::{composer::ProviderStatus, production::ProductionModel},
};
use std::{
    io::{Read, Write},
    sync::Arc,
};

#[test]
fn production_title_uses_quick_route_or_explicit_thread_model() {
    for quick in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let size = socket.read(&mut buffer).unwrap();
                assert!(size > 0);
                request.extend_from_slice(&buffer[..size]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        let body: serde_json::Value =
                            serde_json::from_slice(&request[end + 4..end + 4 + length]).unwrap();
                        let response = r#"{"id":"title","object":"chat.completion","created":0,"model":"fast","choices":[{"index":0,"message":{"role":"assistant","content":"Provider title"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#;
                        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
                        return body;
                    }
                }
            }
        });
        let category = if quick {
            "[agents.worker.categories.quick]\nlogical_model = \"title-route\"\n"
        } else {
            ""
        };
        std::fs::write(
            root.path().join("evorch.toml"),
            format!(
                r#"
[providers.local]
type = "openai-compatible"
base_url = "http://{address}/v1"
api_key_env = "TEST_KEY"
models = ["base", "fast", "chosen"]
default_model = "base"
[routing.routes]
worker = [{{ profile = "local", model = "base" }}]
title-route = [{{ profile = "local", model = "fast" }}]
{category}
"#
            ),
        )
        .unwrap();
        let context = ProductionModel {
            load_options: config::LoadOptions {
                project_dir: Some(root.path().into()),
                user_config_dir: Some(root.path().join("user")),
                read_env: false,
                ..Default::default()
            },
            credential_store: Arc::new(
                sandbox::credential::FileCredentialStore::open(root.path().join("credentials"))
                    .unwrap(),
            ),
            bus: Arc::new(event_bus::EventBus::new(32)),
            env: Arc::new(routing::MapEnv::new(
                [("TEST_KEY".into(), "test-secret".into())].into(),
            )),
        };
        let mut state =
            WorkbenchState::new(DemoSource(vec![]), &workspace_ui::UiSettings::default())
                .unwrap()
                .with_provider_status(ProviderStatus::Configured)
                .with_production_model(
                    context,
                    Arc::new(runtime::compose::SwitchableModel::new(Arc::new(
                        runtime::compose::UnconfiguredModel,
                    ))),
                );
        state.add_project(root.path()).unwrap();
        state.create_thread("New thread").unwrap();
        state.set_thread_model_preference(Some(workspace_ui::ModelPreference {
            profile: "local".into(),
            model: Some("chosen".into()),
        }));
        let mut h = HeadlessWorkbench::new(state, [1200.0, 900.0]);
        h.state_mut().composer_mut().input = "Explain lifetimes".into();
        h.run();
        h.click_label("Send");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while h.state().auto_title_running() {
            assert!(std::time::Instant::now() < deadline);
            h.step();
            std::thread::yield_now();
        }
        h.run();
        assert_eq!(h.state().sidebar().threads[0].title, "Provider title");
        assert_eq!(
            server.join().unwrap()["model"],
            if quick { "fast" } else { "chosen" }
        );
    }
}
