use base64::{Engine, engine::general_purpose::STANDARD};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot, watch},
};

use super::{BrowserAction, cdp};

#[tokio::test]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn chromium_screencast_and_action_evidence() {
    // Given: a local fixture and a real, isolated headless Chromium session.
    let temporary = tempfile::tempdir().expect("evidence directory");
    let evidence = std::env::var_os("EVORCH_BROWSER_EVIDENCE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| temporary.path().to_owned());
    std::fs::create_dir_all(&evidence).expect("create evidence directory");
    let bus = Arc::new(event_bus::EventBus::new(32));
    let mut events = bus.subscribe();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen");
    let address = listener.local_addr().expect("address");
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = [0; 2048];
            let _ = socket.read(&mut request).await;
            let body = "<html><body><h1>Browser fixture</h1><button onclick=\"this.textContent='Changed'\">Change</button></body></html>";
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    let (commands, rx) = mpsc::channel(8);
    let (frames_tx, mut frames) = watch::channel(None);
    let (stop, shutdown) = oneshot::channel();
    let (reports_tx, mut reports_rx) = mpsc::channel(8);
    let browser = tokio::spawn(cdp::run(
        bus,
        false,
        cdp::Channels {
            commands: rx,
            frames: frames_tx,
            shutdown,
            reports: reports_tx,
        },
    ));
    let outcome = tokio::time::timeout(Duration::from_secs(45), async {
        tokio::select! {
            frame = frames.changed() => { frame.expect("first JPEG frame"); }
            event = events.recv() => {
                let event = event.expect("startup event");
                assert!(matches!(event.kind, event_bus::EventKind::Diagnostic(_)));
                tokio::time::timeout(Duration::from_secs(10), frames.changed()).await
                    .expect("first JPEG frame deadline").expect("first JPEG frame");
            }
        }
        assert!(
            frames
                .borrow_and_update()
                .as_ref()
                .is_some_and(|frame| frame.size[0] > 0)
        );
        // When: navigate to the fixture, then click its DOM-changing button.
        for (name, action) in [
            (
                "navigate",
                BrowserAction::Navigate(format!("http://{address}/").parse().expect("URL")),
            ),
            ("click", BrowserAction::Click("button".into())),
        ] {
            commands.send(action).await.expect("submit action");
            let mut screenshots = Vec::new();
            let mut phases = Vec::new();
            let mut action_id = None;
            let mut diagnostics = Vec::new();
            loop {
                let event = events.recv().await.expect("event");
                if let event_bus::EventKind::Diagnostic(diagnostic) = event.kind {
                    assert_ne!(
                        diagnostic.severity,
                        event_bus::DiagnosticSeverity::Error,
                        "{}",
                        diagnostic.detail
                    );
                    if !matches!(
                        diagnostic.code.as_str(),
                        "browser.action" | "browser.screenshot" | "browser.dom_diff"
                    ) {
                        continue;
                    }
                    let detail: serde_json::Value =
                        serde_json::from_str(&diagnostic.detail).expect("JSON");
                    let id = detail["id"].as_str().expect("action id");
                    assert_eq!(action_id.get_or_insert_with(|| id.to_owned()), id);
                    diagnostics
                        .push(serde_json::json!({"code": diagnostic.code, "detail": detail}));
                    // Then: evidence belongs to this successful action and decodes to real pixels.
                    match diagnostic.code.as_str() {
                        "browser.screenshot" => {
                            assert_eq!(detail["mime"], "image/jpeg");
                            let phase = detail["phase"].as_str().expect("screenshot phase");
                            let bytes = STANDARD
                                .decode(detail["base64"].as_str().expect("base64"))
                                .expect("JPEG bytes");
                            let image = image::load_from_memory_with_format(
                                &bytes,
                                image::ImageFormat::Jpeg,
                            )
                            .expect("decode JPEG");
                            assert!(image.width() > 0 && image.height() > 0);
                            if name == "click" {
                                let pixels = image.to_rgb8();
                                assert!(
                                    pixels
                                        .pixels()
                                        .any(|pixel| pixel.0.iter().any(|channel| *channel < 200)),
                                    "fixture must not be blank"
                                );
                            }
                            image
                                .save(evidence.join(format!("{name}-{phase}.png")))
                                .expect("save PNG");
                            screenshots.push(phase.to_owned());
                        }
                        "browser.action" => {
                            assert_eq!(detail["action"], name);
                            assert!(detail["error"].is_null());
                            phases.push(detail["phase"].as_str().expect("action phase").to_owned());
                        }
                        "browser.dom_diff" => {
                            assert_eq!(detail["diff"]["truncated"], false);
                            if name == "click" {
                                assert_eq!(detail["diff"]["removed"], "");
                                assert_eq!(detail["diff"]["inserted"], "d");
                            } else {
                                assert!(
                                    detail["diff"]["inserted"]
                                        .as_str()
                                        .expect("inserted DOM")
                                        .contains("Browser fixture")
                                );
                            }
                            break;
                        }
                        _ => {}
                    }
                }
            }
            assert_eq!(screenshots, ["before", "after"]);
            assert_eq!(phases, ["started", "completed"]);
            let report = reports_rx.recv().await.expect("action report");
            assert_eq!(report.action, name);
            assert!(report.error.is_none(), "{:?}", report.error);
            if name == "click" {
                assert_eq!(report.removed, "");
                assert_eq!(report.inserted, "d");
            }
            std::fs::write(
                evidence.join(format!("{name}.json")),
                serde_json::to_vec_pretty(&diagnostics).expect("serialize diagnostics"),
            )
            .expect("save diagnostics");
        }
        eprintln!("Browser evidence: {}", evidence.display());
    })
    .await;
    let _ = stop.send(());
    let closed = tokio::time::timeout(Duration::from_secs(25), browser).await;
    tasks.abort_all();
    outcome.expect("browser evidence within deadline");
    closed
        .expect("shutdown deadline")
        .expect("browser task")
        .expect("browser result");
}
