use super::*;

#[tokio::test]
async fn pending_reindex_retains_without_clients_then_idle_exit_resumes() {
    if masked_child().await {
        return;
    }
    let dir = test_socket_dir::SplitDir::new();
    let mut host = open(&dir).await;
    let writer = host.composition.storage_writer.clone();
    writer.stop_thread_index_owner().unwrap();
    let db = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
    db.execute_batch("INSERT INTO thread_index_requests(request_id, include_evicted, state, created_at_ns) VALUES ('idle-test', 0, 'pending', 1); INSERT INTO thread_index_request_targets(request_id, run_id, source_revision, extractor_version, accounting_version, work_generation, state) VALUES ('idle-test', 'waiting-run', 1, 1, 1, 0, 'pending')").unwrap();
    writer.tick_thread_index_for_test().unwrap();
    let backend = Arc::new(crate::backend::HostBackend::new(
        host.host.clone(),
        "index-idle".into(),
    ));
    assert!(
        backend
            .hello()
            .status
            .retention_reasons
            .contains(&RetentionReason::PendingReindex)
    );
    let reply = backend.request(
        workbench_protocol::envelope::RequestEnvelope {
            request_id: workbench_protocol::ids::RequestId::new("status").unwrap(),
            session_id: workbench_protocol::ids::SessionId::new("status").unwrap(),
            body: workbench_protocol::envelope::RequestBody::Query(
                workbench_protocol::queries::Query::Lifecycle(
                    workbench_protocol::namespaces::LifecycleQuery::GetCoreStatus,
                ),
            ),
        },
        workbench_protocol::envelope::Provenance::human(
            workbench_protocol::envelope::Surface::Cli,
            ClientId::new("status").unwrap(),
        ),
    );
    assert!(matches!(
        reply.result,
        workbench_protocol::envelope::ReplyResult::Success(_)
    ));
    let listener = tokio::net::UnixListener::bind(dir.sockets().join("index-idle.sock")).unwrap();
    let mut driver = LifecycleDriver::new(&host);
    {
        let running = driver.run_with_web(&host, backend.clone(), listener, None, signals());
        tokio::pin!(running);
        tokio::time::pause();
        assert_pending(running.as_mut()).await;
        tokio::time::advance(Duration::from_secs(120)).await;
        assert_pending(running.as_mut()).await;
        db.execute_batch(r#"UPDATE thread_index_request_targets SET state = 'gap', gap_reason = '{"reason":"deleted"}' WHERE request_id = 'idle-test'"#).unwrap();
        writer.tick_thread_index_for_test().unwrap();
        assert!(!host.busy());
        assert!(
            !backend
                .hello()
                .status
                .retention_reasons
                .contains(&RetentionReason::PendingReindex)
        );
        running.await.unwrap();
    }
    host.shutdown().await.unwrap();
}
