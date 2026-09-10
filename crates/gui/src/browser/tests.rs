use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot, watch},
};

use super::{BrowserAction, cdp};

#[tokio::test]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn chromium_screencast_and_action_evidence() {
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
    let browser = tokio::spawn(cdp::run(
        bus,
        false,
        cdp::Channels {
            commands: rx,
            frames: frames_tx,
            shutdown,
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
        commands
            .send(BrowserAction::Navigate(
                format!("http://{address}/").parse().expect("URL"),
            ))
            .await
            .expect("navigate");
        let mut screenshots = 0;
        loop {
            let event = events.recv().await.expect("event");
            if let event_bus::EventKind::Diagnostic(diagnostic) = event.kind {
                assert_ne!(
                    diagnostic.severity,
                    event_bus::DiagnosticSeverity::Error,
                    "{}",
                    diagnostic.detail
                );
                match diagnostic.code.as_str() {
                    "browser.screenshot" => {
                        let detail: serde_json::Value =
                            serde_json::from_str(&diagnostic.detail).expect("JSON");
                        assert_eq!(detail["mime"], "image/jpeg");
                        screenshots += 1;
                    }
                    "browser.dom_diff" => break,
                    _ => {}
                }
            }
        }
        assert_eq!(screenshots, 2);
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
