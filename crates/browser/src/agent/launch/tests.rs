use super::*;
#[test]
fn c13_handler_config_has_all_seven_explicit_values() {
    let config = handler_config();
    assert!(!config.ignore_https_errors);
    assert!(config.ignore_invalid_messages);
    assert!(config.viewport.is_none());
    assert!(config.context_ids.is_empty());
    assert_eq!(config.request_timeout, std::time::Duration::from_secs(30));
    assert!(!config.request_intercept);
    assert!(config.cache_enabled);
}

#[tokio::test]
async fn partial_initializer_drop_aborts_its_driver_even_while_peer_stays_open() {
    struct Stopped(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for Stopped {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }
    let (began, start) = tokio::sync::oneshot::channel();
    let (stopped, finish) = tokio::sync::oneshot::channel();
    let driver = super::AbortTask::new(tokio::spawn(async move {
        let _stopped = Stopped(Some(stopped));
        let _ = began.send(());
        std::future::pending::<()>().await;
    }));
    start.await.unwrap();
    assert!(!driver.is_finished());
    drop(driver);
    finish
        .await
        .expect("scoped driver must be dropped without a peer EOF");
}
