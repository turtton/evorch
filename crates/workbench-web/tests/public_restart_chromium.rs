//! Browser end-to-end test of the web management screen (`#/admin`).
//!
//! Two worlds, kept apart on purpose:
//!
//! - **Production shape**: the real server with no authentication. The page
//!   must show the disabled state and must not have been able to reach the
//!   backend at all. This is the evidence for "management stays disabled".
//! - **Contract shape**: the same adapter with a contract-only fake principal
//!   injected in front of it and a scripted facade behind it. It proves the
//!   page's confirmation, conflict and cursor handling. It is not evidence of
//!   web authentication, which does not exist yet.
//!
//! Ignored by default because it needs Chrome or Chromium (`CHROME` names the
//! executable); the CI `browser-e2e` job runs it with `--run-ignored only`.
//! Every wait is a DOM mutation or an event the backend recorded; nothing
//! sleeps.

mod common;
#[path = "public_restart/support.rs"]
mod support;

use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::handler::viewport::Viewport;
use chromiumoxide::{Browser, BrowserConfig, Page};
use common::FakeBackend;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use support::Facade;
use tokio::time::Instant;
use workbench_protocol::events::UiEvent;
use workbench_protocol::namespaces::LifecycleEvent;
use workbench_protocol::wire::Revision;
use workbench_web::{WebApp, WebServer, WebServerConfig};

async fn launch_browser(profile: &std::path::Path) -> Browser {
    let mut config = BrowserConfig::builder()
        .new_headless_mode()
        .user_data_dir(profile)
        .viewport(Viewport {
            width: 390,
            height: 844,
            device_scale_factor: Some(1.0),
            emulating_mobile: true,
            is_landscape: false,
            has_touch: true,
        })
        .launch_timeout(Duration::from_secs(120))
        .request_timeout(Duration::from_secs(300))
        .args([
            "no-first-run",
            "no-default-browser-check",
            "disable-background-networking",
            "disable-component-update",
            "disable-sync",
            "disable-extensions",
            "mute-audio",
            "lang=ja-JP",
        ]);
    if let Some(executable) = std::env::var_os("CHROME") {
        config = config.chrome_executable(executable);
    }
    if std::env::var_os("WORKBENCH_WEB_CHROME_NO_SANDBOX").is_some() {
        config = config.no_sandbox();
    }
    let (browser, mut handler) = Browser::launch(config.build().unwrap())
        .await
        .expect("launch Chrome or Chromium (set CHROME to its executable)");
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    browser
}

struct Driver {
    page: Page,
}

impl Driver {
    async fn eval(&self, expression: &str) -> Value {
        let params = EvaluateParams::builder()
            .expression(expression)
            .await_promise(true)
            .return_by_value(true)
            .build()
            .unwrap();
        let result = self.page.evaluate_expression(params).await.unwrap();
        result.value().cloned().unwrap_or(Value::Null)
    }

    /// Wait until `predicate` (JS function source) is truthy, re-checking on
    /// every DOM mutation.
    async fn wait(&self, predicate: &str) -> Value {
        self.eval(&format!(
            r##"new Promise((resolve) => {{
                const test = {predicate};
                let observer = null;
                const check = () => {{
                    let value;
                    try {{ value = test(); }} catch (error) {{ value = undefined; }}
                    if (!value) return false;
                    if (observer) observer.disconnect();
                    resolve(value instanceof Node ? true : value);
                    return true;
                }};
                if (!check()) {{
                    observer = new MutationObserver(check);
                    observer.observe(document, {{ subtree: true, childList: true, attributes: true, characterData: true }});
                }}
            }})"##
        ))
        .await
    }

    async fn click(&self, selector: &str) {
        let clicked = self
            .eval(&format!(
                r##"(() => {{
                    const el = document.querySelector({selector:?});
                    if (!el || el.disabled) return false;
                    el.click();
                    return true;
                }})()"##
            ))
            .await;
        assert_eq!(clicked, json!(true), "could not click {selector}");
    }
}

/// The adapter with a contract-only principal in front of a scripted facade.
async fn serve_with_fake_principal(facade: &Arc<Facade>) -> String {
    let app = WebApp::new(&WebServerConfig::default(), Arc::clone(facade)).unwrap();
    let (principal, lease) = workbench_authority::test_support::web(
        "contract-admin",
        Instant::now() + Duration::from_secs(3600),
    );
    std::mem::forget(lease);
    let router = app.router().layer(axum::Extension(principal));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{address}")
}

const BUTTON_STATE: &str = r##"(() => [...document.querySelectorAll(".admin-body button")]
    .map((b) => ({ text: b.textContent, disabled: b.disabled })))()"##;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chrome or Chromium; run with --run-ignored only (CI browser-e2e job)"]
async fn production_ui_is_disabled_and_never_reaches_the_backend() {
    let backend = Arc::new(FakeBackend::new());
    // The page's ordinary `all` stream is allowed without authentication.
    let (subscription, _events) = common::subscription(common::cursor(7, 0));
    backend.script_subscription(Ok(subscription));
    let server = WebServer::start(
        WebServerConfig {
            enabled: true,
            bind: "127.0.0.1:0".parse().unwrap(),
            ..WebServerConfig::default()
        },
        Arc::clone(&backend),
    )
    .await
    .unwrap()
    .unwrap();
    let base = format!("http://127.0.0.1:{}", server.local_addr().port());
    let profile = tempfile::tempdir().unwrap();
    let browser = launch_browser(profile.path()).await;
    let ui = Driver {
        page: browser.new_page(format!("{base}/#/admin")).await.unwrap(),
    };

    ui.wait(r##"() => document.querySelector('.admin[data-access="disabled"]')"##)
        .await;
    let note = ui
        .eval(r##"document.querySelector(".admin-access").textContent"##)
        .await;
    assert!(note.as_str().unwrap().contains("無効"), "{note}");

    // Every action is shown, none can be pressed, and nothing is open.
    let buttons = ui.eval(BUTTON_STATE).await;
    let buttons = buttons.as_array().unwrap();
    assert!(!buttons.is_empty());
    assert!(
        buttons.iter().all(|b| b["disabled"] == json!(true)),
        "{buttons:?}"
    );
    assert_eq!(
        ui.eval(r##"document.querySelectorAll(".admin-confirm").length"##)
            .await,
        json!(0)
    );

    // Hiding is not the protection, so check what the page actually asked for:
    // no management stream was opened and the backend was never called.
    let streams = ui
        .eval(
            r##"performance.getEntriesByType("resource").filter((e) => e.name.includes("scope=management")).length"##,
        )
        .await;
    assert_eq!(streams, json!(0));
    assert_eq!(backend.seen_count(), 0);
    assert!(
        backend
            .subscribes()
            .iter()
            .all(|(_, s)| s.scope != workbench_protocol::events::SubscriptionScope::Management)
    );

    // The home screen links to the screen; nothing there is enabled by Human.
    ui.eval("location.hash = '#/'; true").await;
    ui.wait(r##"() => document.querySelector("a.admin-link")"##)
        .await;
    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chrome or Chromium; run with --run-ignored only (CI browser-e2e job)"]
async fn confirmation_conflict_and_epoch_handling_with_a_fake_principal() {
    let facade = Arc::new(Facade::new());
    let base = serve_with_fake_principal(&facade).await;
    let profile = tempfile::tempdir().unwrap();
    let browser = launch_browser(profile.path()).await;
    let ui = Driver {
        page: browser.new_page(format!("{base}/#/admin")).await.unwrap(),
    };

    ui.wait(r##"() => document.querySelector('.admin[data-access="enabled"] [data-target="current"]')"##)
        .await;
    // The path-free listing: what is shown is the id and revision only.
    let text = ui
        .eval("document.querySelector('.admin').textContent")
        .await;
    let text = text.as_str().unwrap();
    assert!(text.contains("current") && !text.contains('/'), "{text}");

    // The target changes while the user is looking at the confirmation: the
    // core answers Conflict, the page says so and the stale confirmation
    // cannot be sent.
    ui.click(r##"[data-target="current"] button"##).await;
    ui.wait(r##"() => document.querySelector(".admin-confirm")"##)
        .await;
    facade.set_target_revision(2);
    ui.click(".admin-confirm .danger").await;
    ui.wait(r##"() => document.querySelector('.admin-outcome[data-outcome="conflict"]')"##)
        .await;
    ui.wait(r##"() => document.querySelector(".admin-void")"##)
        .await;
    assert!(
        facade.restarts().is_empty(),
        "a conflicting request changes nothing"
    );
    ui.click(".admin-confirm .decide-row button").await;

    // A new confirmation binds the current revision and goes through.
    ui.wait(r##"() => document.querySelector('[data-target="current"] .row-sub')?.textContent.includes("リビジョン 2")"##)
        .await;
    ui.click(r##"[data-target="current"] button"##).await;
    ui.wait(r##"() => document.querySelector(".admin-confirm .danger:not(:disabled)")"##)
        .await;
    ui.click(".admin-confirm .danger").await;
    ui.wait(r##"() => document.querySelector('.admin-outcome[data-outcome="pending"]')"##)
        .await;
    assert_eq!(facade.restarts().len(), 1);
    ui.click(".admin-confirm .decide-row button").await;

    // A confirmation open when the core epoch changes is void without any
    // request: the next management notice makes the page read the new epoch.
    ui.click(r##"[data-target="current"] button"##).await;
    ui.wait(r##"() => document.querySelector('.admin[data-stream="live"] .admin-confirm .danger:not(:disabled)')"##)
        .await;
    assert!(facade.has_open_stream());
    facade.set_management_epoch(3);
    facade
        .push(common::event(
            common::cursor(7, 1),
            UiEvent::Lifecycle(LifecycleEvent::AdminChanged {
                revision: Revision(1),
                restarting: false,
            }),
        ))
        .await;
    ui.wait(r##"() => document.querySelector(".admin-void")"##)
        .await;
    ui.wait(r##"() => document.querySelector(".admin-confirm .danger")?.disabled"##)
        .await;
    assert_eq!(
        facade.restarts().len(),
        1,
        "the void confirmation sent nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Chrome or Chromium; run with --run-ignored only (CI browser-e2e job)"]
async fn page_side_cursor_and_request_rules() {
    let facade = Arc::new(Facade::new());
    let base = serve_with_fake_principal(&facade).await;
    let profile = tempfile::tempdir().unwrap();
    let browser = launch_browser(profile.path()).await;
    let ui = Driver {
        page: browser.new_page(format!("{base}/")).await.unwrap(),
    };
    let result = ui
        .eval(include_str!("public_restart_chromium/rules.js"))
        .await;
    assert_eq!(result["failures"], json!([]), "{result:#}");
    assert!(result["passed"].as_u64().unwrap() >= 6, "{result:#}");
}
