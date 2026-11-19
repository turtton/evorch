//! Independent RGBA8 records, sharing only framing, transfer order and ACK.
use super::*;
use crate::namespaces::browser::data::{DataError, Kind, encode_record};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentFrameStart {
    pub scope: ObservationScope,
    pub tab: ObservationId,
    /// Connection-global, increasing across active-tab changes.
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub total_bytes: u64,
}
impl AgentFrameStart {
    // A fixed header: RGBA8 discriminator, JSON length, bounded JSON and zero
    // padding. Prefix validates this exact size before allocating anything.
    pub const LEN: usize = 2048; // Includes maximally escaped MAX_ID_BYTES identities.
    pub fn validate(&self) -> Result<(), DataError> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_WIDTH
            || self.height > MAX_HEIGHT
            || self.stride as u64 != self.width as u64 * 4
            || self.total_bytes != self.stride as u64 * self.height as u64
            || self.total_bytes > MAX_FRAME_BYTES
        {
            return Err(DataError::Malformed("agent frame dimensions"));
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>, DataError> {
        self.validate()?;
        let json = serde_json::to_vec(self).map_err(|_| DataError::Malformed("agent header"))?;
        if json.len() > Self::LEN - 4 {
            return Err(DataError::Malformed("agent header size"));
        }
        let mut meta = vec![0; Self::LEN];
        meta[0] = 2; // RGBA8, not renderer BGRA8=1.
        meta[2..4].copy_from_slice(&(json.len() as u16).to_le_bytes());
        meta[4..4 + json.len()].copy_from_slice(&json);
        Ok(encode_record(Kind::AgentFrameStart, &meta, &[]))
    }
    pub fn decode(meta: &[u8]) -> Result<Self, DataError> {
        if meta.len() != Self::LEN || meta[..2] != [2, 0] {
            return Err(DataError::Malformed("agent header format"));
        }
        let len = u16::from_le_bytes([meta[2], meta[3]]) as usize;
        if len > Self::LEN - 4 || meta[4 + len..].iter().any(|b| *b != 0) {
            return Err(DataError::Malformed("agent header length"));
        }
        let out: Self = serde_json::from_slice(&meta[4..4 + len])
            .map_err(|_| DataError::Malformed("agent header"))?;
        out.validate()?;
        Ok(out)
    }
}
