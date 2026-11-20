//! REGISTRATION-ONLY CW placeholder, not evidence of browser restart behavior.
#[ignore = "registration-only: browser-e2e selects this target explicitly"]
#[tokio::test]
async fn registration_only_public_restart_browser_is_available() {
    use futures_util::StreamExt;
    let (mut browser, mut handler) = chromiumoxide::Browser::launch(
        chromiumoxide::BrowserConfig::builder()
            .no_sandbox()
            .build()
            .unwrap(),
    )
    .await
    .expect("Chromium is required");
    let task = tokio::spawn(async move { while handler.next().await.is_some() {} });
    browser.close().await.unwrap();
    task.await.unwrap();
}
