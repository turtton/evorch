//! Actor-owned data readers. No payload ever enters the action/event queues.
use super::{Shared, update};
use crate::core_client::features::{FeatureError, FeatureId, data::*};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::task::JoinHandle;
use workbench_client::{
    ConnectionState, WorkbenchClient,
    renderer::{DataChannel, DataClientError},
};
use workbench_protocol::namespaces::browser::data::{BlobSource, BlobStart, DataError};

#[derive(Default)]
pub(super) struct DataReaders {
    jobs: HashMap<(FeatureId, u64), JoinHandle<()>>,
}
impl Drop for DataReaders {
    fn drop(&mut self) {
        for job in self.jobs.values() {
            job.abort();
        }
    }
}
impl DataReaders {
    async fn cancel(
        &mut self,
        feature: FeatureId,
        tag: u64,
        shared: &Arc<Mutex<Shared>>,
        ctx: &egui::Context,
    ) {
        update(shared, ctx, |s| {
            if s.features.data.remove(feature, tag) {
                s.features.data_notice(feature, tag, DataNotice::Cancelled);
            }
        });
        if let Some(job) = self.jobs.remove(&(feature, tag)) {
            job.abort();
            let _ = job.await;
        }
    }
    /// Invalidate slots BEFORE cancelling tasks. Join every reader BEFORE a new
    /// endpoint/epoch/snapshot may bind, including same-epoch reconnects.
    pub async fn stop(&mut self, shared: &Arc<Mutex<Shared>>, ctx: &egui::Context) {
        update(shared, ctx, |s| {
            s.features.clear_data();
            for &(feature, tag) in self.jobs.keys() {
                s.features.data_notice(feature, tag, DataNotice::Cancelled);
            }
        });
        for job in self.jobs.values() {
            job.abort();
        }
        for (_, job) in self.jobs.drain() {
            let _ = job.await;
        }
    }
    pub async fn handle(
        &mut self,
        action: DataAction,
        endpoint: &Path,
        client: &WorkbenchClient,
        shared: &Arc<Mutex<Shared>>,
        ctx: &egui::Context,
    ) {
        // A stale cancellation must not cancel a newly rebound request using the same tag.
        if !shared.lock().unwrap().features.data.current(&action) {
            return;
        }
        self.cancel(action.feature, action.tag, shared, ctx).await;
        let DataRequest::Start(source) = &action.request else {
            return;
        };
        let source = source.clone();
        let mut key = None;
        update(shared, ctx, |s| {
            // Joining a replaced reader can yield to a concurrent feature reset.
            if !s.features.data.current(&action) {
                return;
            }
            if !s.connected || client.connection_state() != ConnectionState::Connected {
                s.features.data_notice(
                    action.feature,
                    action.tag,
                    DataNotice::Failed(FeatureError::new("disconnected")),
                );
                return;
            }
            if let DataSource::Frames { tab, .. } = &source
                && tab.core_epoch != client.server_hello().instance_epoch.0
            {
                s.features.data_notice(
                    action.feature,
                    action.tag,
                    DataNotice::Failed(FeatureError::new("stale renderer epoch")),
                );
                return;
            }
            match s.features.data.claim(&action, source.clone()) {
                Ok(k) => key = Some(k),
                Err(e) => s
                    .features
                    .data_notice(action.feature, action.tag, DataNotice::Failed(e)),
            }
        });
        let Some(key) = key else {
            return;
        };
        let finished: Vec<_> = self
            .jobs
            .iter()
            .filter(|(_, j)| j.is_finished())
            .map(|(k, _)| *k)
            .collect();
        for id in finished {
            if let Some(job) = self.jobs.remove(&id) {
                let _ = job.await;
            }
        }
        let (endpoint, client, shared, ctx) = (
            endpoint.to_owned(),
            client.clone(),
            shared.clone(),
            ctx.clone(),
        );
        self.jobs.insert((key.feature, key.tag), tokio::spawn(async move {
            // Dropping this future also drops a partially connected/binding socket.
            // Bind is ephemeral: no receipt query, retry or automatic rebind.
            tokio::select! {
                biased;
                _ = client.closed() => finish(&shared, &ctx, key, DataNotice::Cancelled, true),
                result = receive(endpoint, &client, &shared, &ctx, key, source) => {
                    match result {
                        Ok(()) => finish(&shared, &ctx, key, DataNotice::Completed, false),
                        Err(error) => {
                            let error = match &error { DataClientError::Bind(e) => super::feature_error(e), _ => FeatureError::new(error.to_string()) };
                            finish(&shared, &ctx, key, DataNotice::Failed(error), true);
                        }
                    }
                }
            }
        }));
    }
}
fn finish(
    shared: &Arc<Mutex<Shared>>,
    ctx: &egui::Context,
    key: DataKey,
    notice: DataNotice,
    remove: bool,
) {
    update(shared, ctx, |s| {
        if s.features.data.contains(key) {
            if remove {
                s.features.data.remove(key.feature, key.tag);
            }
            s.features.data_notice(key.feature, key.tag, notice);
        }
    });
}
fn publish(
    shared: &Arc<Mutex<Shared>>,
    ctx: &egui::Context,
    key: DataKey,
    payload: DataPayload,
) -> bool {
    let mut accepted = false;
    update(shared, ctx, |s| {
        accepted = s.connected && s.features.data.publish(key, payload);
        if accepted {
            s.features
                .data_notice(key.feature, key.tag, DataNotice::Ready);
        }
    });
    accepted
}
fn mismatch() -> DataClientError {
    DataError::OutOfOrder("data source or control epoch mismatch").into()
}
async fn receive(
    endpoint: PathBuf,
    client: &WorkbenchClient,
    shared: &Arc<Mutex<Shared>>,
    ctx: &egui::Context,
    key: DataKey,
    source: DataSource,
) -> Result<(), DataClientError> {
    let epoch = client.server_hello().instance_epoch.0;
    let channel = DataChannel::connect(endpoint).await?;
    match source {
        DataSource::Frames {
            tab,
            subscription,
            token,
        } => {
            let mut frames = channel.bind_frames(client, subscription, token).await?;
            finish(shared, ctx, key, DataNotice::Started, false);
            while let Some(frame) = frames.next_frame().await? {
                if frame.start.core_epoch != epoch
                    || frame.start.tab != tab.id
                    || frame.start.generation != tab.generation
                {
                    return Err(mismatch());
                }
                if !publish(shared, ctx, key, DataPayload::Frame(frame)) {
                    return Ok(());
                }
            }
        }
        source => {
            let max = source.max_bytes();
            let blob = match &source {
                DataSource::Artifact(artifact) => {
                    channel.bind_artifact(client, artifact.clone()).await?
                }
                DataSource::Capture {
                    operation_id,
                    capture_id,
                } => {
                    channel
                        .bind_capture(client, operation_id.clone(), capture_id.clone())
                        .await?
                }
                DataSource::Frames { .. } => unreachable!(),
            };
            let start = blob.start().clone();
            if start.core_epoch != epoch {
                return Err(mismatch());
            }
            let digest = match &source {
                DataSource::Artifact(artifact) => {
                    let digest = BlobStart::artifact_digest(&artifact.digest)?;
                    if start.source != BlobSource::Artifact || start.digest != digest {
                        return Err(mismatch());
                    }
                    Some(digest)
                }
                _ => {
                    if start.source != BlobSource::Capture {
                        return Err(mismatch());
                    }
                    None
                }
            };
            finish(shared, ctx, key, DataNotice::Started, false);
            let bytes = blob.read_to_end(max).await?;
            if digest.is_some_and(|expected| Sha256::digest(&bytes).as_slice() != expected) {
                return Err(mismatch());
            }
            publish(shared, ctx, key, DataPayload::Blob(Blob { start, bytes }));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
