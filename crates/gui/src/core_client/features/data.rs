//! Binary delivery contract shared by PR A (artifacts) and PR B (preview).
//!
//! Only the connection actor opens/binds sockets. A feature asks through
//! FeatureContext, receives payload-free DataNotice callbacks, and takes the
//! latest payload while rendering. Never retain a history of these payloads;
//! decoded images/textures have their own feature-owned limits.
use super::{FeatureError, FeatureId};
use std::collections::HashMap;
use workbench_client::renderer::Frame;
use workbench_protocol::namespaces::{
    artifacts::ArtifactRef,
    browser::{RendererId, RendererTabRef, data::*},
};

/// Worst-case binary payload storage, including one incoming frame per slot
/// and BlobStream's one chunk alongside each assembled blob. Excludes small
/// protocol metadata and feature-owned decoded images/GPU textures.
pub const MAX_DATA_BYTES: u64 = 2 * MAX_FRAME_SUBSCRIPTIONS as u64 * MAX_FRAME_BYTES
    + MAX_BLOB_TRANSFERS as u64 * (MAX_CAPTURE_BLOB_BYTES + CHUNK_BYTES as u64);

#[derive(Debug, Clone)]
pub enum DataSource {
    Frames {
        tab: RendererTabRef,
        subscription: RendererId,
        token: RendererId,
    },
    Artifact(ArtifactRef),
    Capture {
        operation_id: RendererId,
        capture_id: RendererId,
    },
}
impl DataSource {
    pub(crate) fn is_frame(&self) -> bool {
        matches!(self, Self::Frames { .. })
    }
    pub(crate) fn max_bytes(&self) -> u64 {
        match self {
            Self::Frames { .. } => MAX_FRAME_BYTES,
            Self::Artifact(_) => MAX_IMAGE_BLOB_BYTES,
            Self::Capture { .. } => MAX_CAPTURE_BLOB_BYTES,
        }
    }
    fn reservation(&self) -> u64 {
        if self.is_frame() {
            2 * MAX_FRAME_BYTES
        } else {
            self.max_bytes() + CHUNK_BYTES as u64
        }
    }
    fn owner(&self) -> FeatureId {
        if matches!(self, Self::Artifact(_)) {
            FeatureId::Artifacts
        } else {
            FeatureId::Browser
        }
    }
}
#[derive(Debug)]
pub enum DataRequest {
    Start(DataSource),
    Cancel,
}

/// Created by FeatureContext, not by a feature constructing connection state.
#[derive(Debug)]
pub struct DataAction {
    pub(crate) feature: FeatureId,
    pub(crate) tag: u64,
    pub(crate) generation: u64,
    pub(crate) request: DataRequest,
}

/// Signals only: no bytes, paths, transport or control client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataNotice {
    Started,
    Ready,
    Completed,
    Cancelled,
    Failed(FeatureError),
}

#[derive(Debug)]
pub struct Blob {
    pub start: BlobStart,
    pub bytes: Vec<u8>,
}
#[derive(Debug)]
pub enum DataPayload {
    Frame(Frame),
    Blob(Blob),
}
impl DataPayload {
    fn bytes(&self) -> usize {
        match self {
            Self::Frame(f) => f.bgra.len(),
            Self::Blob(b) => b.bytes.len(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DataKey {
    pub feature: FeatureId,
    pub tag: u64,
    generation: u64,
    serial: u64,
}
struct Slot {
    key: DataKey,
    source: DataSource,
    payload: Option<DataPayload>,
}
#[derive(Default)]
pub(crate) struct DataInbox {
    generations: HashMap<FeatureId, u64>,
    serial: u64,
    slots: HashMap<(FeatureId, u64), Slot>,
}
impl DataInbox {
    pub fn generation(&self, feature: FeatureId) -> u64 {
        self.generations.get(&feature).copied().unwrap_or(0)
    }
    pub fn current(&self, action: &DataAction) -> bool {
        self.generation(action.feature) == action.generation
    }
    pub fn reset(&mut self, feature: FeatureId) {
        *self.generations.entry(feature).or_default() += 1;
        self.slots.retain(|(owner, _), _| *owner != feature);
    }
    pub fn claim(
        &mut self,
        action: &DataAction,
        source: DataSource,
    ) -> Result<DataKey, FeatureError> {
        if !self.current(action) || source.owner() != action.feature {
            return Err(FeatureError::new("stale request or wrong data owner"));
        }
        let count = self
            .slots
            .values()
            .filter(|s| s.source.is_frame() == source.is_frame())
            .count();
        let limit = if source.is_frame() {
            MAX_FRAME_SUBSCRIPTIONS
        } else {
            MAX_BLOB_TRANSFERS
        };
        let reserved: u64 = self.slots.values().map(|s| s.source.reservation()).sum();
        if count >= limit || reserved + source.reservation() > MAX_DATA_BYTES {
            return Err(FeatureError::new("binary receive budget exhausted"));
        }
        self.serial += 1;
        let key = DataKey {
            feature: action.feature,
            tag: action.tag,
            generation: action.generation,
            serial: self.serial,
        };
        self.slots.insert(
            (key.feature, key.tag),
            Slot {
                key,
                source,
                payload: None,
            },
        );
        Ok(key)
    }
    /// Includes completed slots after their reader task has been reaped.
    pub fn tags(&self) -> impl Iterator<Item = (FeatureId, u64)> + '_ {
        self.slots.keys().copied()
    }
    pub fn contains(&self, key: DataKey) -> bool {
        self.slots
            .get(&(key.feature, key.tag))
            .is_some_and(|slot| slot.key == key)
    }
    /// Takes ownership even on rejection: stale bytes are dropped here, never queued.
    pub fn publish(&mut self, key: DataKey, payload: DataPayload) -> bool {
        let Some(slot) = self
            .slots
            .get_mut(&(key.feature, key.tag))
            .filter(|s| s.key == key)
        else {
            return false;
        };
        if payload.bytes() as u64 > slot.source.max_bytes()
            || matches!(&payload, DataPayload::Frame(_)) != slot.source.is_frame()
        {
            return false;
        }
        slot.payload = Some(payload);
        true
    }
    pub fn remove(&mut self, feature: FeatureId, tag: u64) -> bool {
        self.slots.remove(&(feature, tag)).is_some()
    }
    /// Move, do not clone. At most one latest frame/blob exists per request.
    pub fn take(&mut self, feature: FeatureId, tag: u64) -> Option<DataPayload> {
        self.slots.get_mut(&(feature, tag))?.payload.take()
    }
    #[cfg(test)]
    pub fn usage(&self) -> (usize, u64, u64) {
        (
            self.slots.len(),
            self.slots
                .values()
                .filter_map(|s| s.payload.as_ref())
                .map(|p| p.bytes() as u64)
                .sum(),
            self.slots.values().map(|s| s.source.reservation()).sum(),
        )
    }
}

#[cfg(test)]
mod tests;
