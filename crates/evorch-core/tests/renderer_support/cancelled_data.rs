//! Cancellation is observed by joining the production data-connection task.
//! A successful replacement proves both the pending binding place and the
//! blob lease/transfer budget were released, without timing or retry probes.
use evorch_core::{backend::HostBackend, unix};
use std::{path::Path, sync::Arc};
use tokio::{net::UnixListener, sync::watch, task::JoinHandle};
use workbench_client::{WorkbenchClient, renderer::DataChannel};
use workbench_protocol::namespaces::{
    artifacts::ArtifactRef,
    browser::data::{MAX_BLOB_TRANSFERS, MAX_UNBOUND_CONNECTIONS},
};

async fn channel(
    path: &Path,
    backend: &Arc<HostBackend>,
    shutdown: &watch::Receiver<bool>,
) -> (
    Result<DataChannel, workbench_client::renderer::DataClientError>,
    JoinHandle<()>,
) {
    let listener = UnixListener::bind(path).unwrap();
    let (backend, shutdown) = (backend.clone(), shutdown.clone());
    let served = tokio::spawn(async move {
        let (peer, _) = listener.accept().await.unwrap();
        let _ = unix::connection(peer, backend, shutdown).await;
    });
    (DataChannel::connect(path).await, served)
}
pub async fn verify(
    root: &Path,
    backend: &Arc<HostBackend>,
    shutdown: &watch::Receiver<bool>,
    client: &WorkbenchClient,
    large_image: &ArtifactRef,
) {
    let mut pending = Vec::new();
    for index in 0..MAX_UNBOUND_CONNECTIONS {
        let (link, served) = channel(
            &root.join(format!("pending-{index}.sock")),
            backend,
            shutdown,
        )
        .await;
        pending.push((link.unwrap(), served));
    }
    let (full, served) = channel(&root.join("pending-full.sock"), backend, shutdown).await;
    assert!(full.is_err(), "unbound channel budget is full");
    served.await.unwrap();
    let (link, served) = pending.pop().unwrap();
    drop(link);
    served.await.unwrap(); // EOF completed and the PendingGuard has dropped.
    let (replacement, served) =
        channel(&root.join("pending-replaced.sock"), backend, shutdown).await;
    drop(replacement.unwrap());
    served.await.unwrap();
    for (link, served) in pending {
        drop(link);
        served.await.unwrap();
    }

    let mut held = Vec::new();
    for index in 0..MAX_BLOB_TRANSFERS {
        let (link, served) =
            channel(&root.join(format!("blob-{index}.sock")), backend, shutdown).await;
        let blob = link
            .unwrap()
            .bind_artifact(client, large_image.clone())
            .await
            .unwrap();
        held.push((blob, served));
    }
    let (full, served) = channel(&root.join("blob-full.sock"), backend, shutdown).await;
    assert_eq!(
        super::data_peer::refusal(
            full.unwrap()
                .bind_artifact(client, large_image.clone())
                .await
        ),
        workbench_protocol::errors::ErrorCode::Busy
    );
    served.await.unwrap();
    let (blob, served) = held.pop().unwrap();
    drop(blob); // closes a socket while the core is still sending its body
    served.await.unwrap(); // the real BlobLease and semaphore permit have dropped.
    let (replacement, served) = channel(&root.join("blob-replaced.sock"), backend, shutdown).await;
    let blob = replacement
        .unwrap()
        .bind_artifact(client, large_image.clone())
        .await
        .unwrap();
    drop(blob);
    served.await.unwrap();
    for (blob, served) in held {
        drop(blob);
        served.await.unwrap();
    }
}
