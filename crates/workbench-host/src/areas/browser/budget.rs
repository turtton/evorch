//! Resource reservations, deliberately independent of the spawn fence.
use super::renderer::OwnerError;
use renderer_ipc::{MAX_FRAME_BYTES, MAX_FRAME_RATE, MAX_TABS, TabId, Viewport};
use std::collections::BTreeMap;
pub const PIXELS_PER_SECOND: u64 = 250_000_000;
pub const HELPER_BYTES: u64 = 1536 * 1024 * 1024;
pub const RAW_BYTES: u64 = 512 * 1024 * 1024;
const BASE_BYTES: u64 = 240 * 1024 * 1024;
#[derive(Clone, Copy, Debug)]
pub struct TabBudget {
    pub viewport: Viewport,
    pub fps: u8,
    pub streaming: bool,
}
impl TabBudget {
    fn rate(self) -> u64 {
        if self.streaming {
            u64::from(self.viewport.width) * u64::from(self.viewport.height) * u64::from(self.fps)
        } else {
            0
        }
    }
}
#[derive(Default)]
pub struct RendererBudget {
    tabs: BTreeMap<TabId, TabBudget>,
    high_water: BTreeMap<TabId, u64>,
    pub used_4k: bool,
    helper_high_water: u64,
    retired_high_water: u64,
}
impl RendererBudget {
    pub fn len(&self) -> usize {
        self.tabs.len()
    }
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }
    pub fn get(&self, tab: TabId) -> Result<TabBudget, OwnerError> {
        self.tabs
            .get(&tab)
            .copied()
            .ok_or(OwnerError::StaleGeneration)
    }
    pub fn reserve(&mut self, tab: TabId, next: TabBudget) -> Result<(), OwnerError> {
        next.viewport.validate().map_err(|_| OwnerError::Invalid)?;
        if next.fps == 0 || next.fps > MAX_FRAME_RATE {
            return Err(OwnerError::Invalid);
        }
        if !self.tabs.contains_key(&tab) && self.tabs.len() == MAX_TABS {
            return Err(OwnerError::Busy);
        }
        let rate = self
            .tabs
            .iter()
            .filter(|(id, _)| **id != tab)
            .map(|(_, v)| v.rate())
            .sum::<u64>()
            + next.rate();
        let streams = self
            .tabs
            .iter()
            .filter(|(id, v)| **id != tab && v.rate() != 0)
            .count();
        let four_k_stream =
            next.rate() != 0 && next.fps >= 30 && next.viewport.frame_bytes() == MAX_FRAME_BYTES;
        let other_four_k = self.tabs.iter().any(|(id, v)| {
            *id != tab
                && v.rate() != 0
                && v.fps >= 30
                && v.viewport.frame_bytes() == MAX_FRAME_BYTES
        });
        let bytes = next.viewport.frame_bytes() as u64;
        let high = self.high_water.get(&tab).copied().unwrap_or(0).max(bytes);
        let sum = self
            .high_water
            .iter()
            .filter(|(id, _)| **id != tab)
            .map(|(_, v)| *v)
            .sum::<u64>()
            + high;
        // Provisional reservation model: base + 32 bytes/pixel + first-paint
        // slack. This is not a measured/enforced PSS bound; allocation high-water
        // is retained for the generation even after resize or close.
        let helper_high_water = self.helper_high_water.max(self.retired_high_water + sum);
        let helper = BASE_BYTES + helper_high_water * 8 + 64 * 1024 * 1024;
        let raw = sum + 2 * MAX_FRAME_BYTES as u64;
        if rate > PIXELS_PER_SECOND
            || (four_k_stream && streams != 0)
            || (other_four_k && next.rate() != 0)
            || helper > HELPER_BYTES
            || raw > RAW_BYTES
        {
            return Err(OwnerError::Busy);
        }
        self.used_4k |= bytes == MAX_FRAME_BYTES as u64;
        self.helper_high_water = helper_high_water;
        self.high_water.insert(tab, high);
        self.tabs.insert(tab, next);
        Ok(())
    }
    pub fn remove(&mut self, tab: TabId) {
        self.tabs.remove(&tab);
        // CEF may retain closed tab allocations until generation teardown.
        self.retired_high_water += self.high_water.remove(&tab).unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec(width: u32, height: u32, fps: u8) -> TabBudget {
        TabBudget {
            viewport: Viewport { width, height },
            fps,
            streaming: true,
        }
    }
    #[test]
    fn pixel_rate_boundary_and_tab_limit_do_not_evict() {
        let mut budget = RendererBudget::default();
        for id in 1..=4 {
            budget.reserve(TabId(id), spec(2500, 1000, 25)).unwrap();
        }
        assert_eq!(
            budget.reserve(TabId(1), spec(2500, 1000, 26)),
            Err(OwnerError::Busy)
        );
        assert_eq!(budget.get(TabId(1)).unwrap().fps, 25);
        assert_eq!(
            budget.reserve(TabId(5), spec(1, 1, 1)),
            Err(OwnerError::Busy)
        );
        budget.remove(TabId(1));
        budget.reserve(TabId(5), spec(1, 1, 1)).unwrap();
    }
    #[test]
    fn four_k_stream_is_exclusive_until_fps_or_resolution_drops() {
        let mut budget = RendererBudget::default();
        budget.reserve(TabId(1), spec(3840, 2160, 30)).unwrap();
        assert_eq!(
            budget.reserve(TabId(2), spec(1, 1, 1)),
            Err(OwnerError::Busy)
        );
        budget.reserve(TabId(1), spec(3840, 2160, 10)).unwrap();
        budget.reserve(TabId(2), spec(1280, 720, 30)).unwrap();
        assert_eq!(
            budget.reserve(TabId(1), spec(3840, 2160, 30)),
            Err(OwnerError::Busy)
        );
        budget.remove(TabId(1));
        budget.remove(TabId(2));
        assert!(budget.used_4k);
        for id in 3..1000 {
            budget.reserve(TabId(id), spec(1, 1, 30)).unwrap();
            budget.remove(TabId(id));
        }
        assert!(budget.high_water.is_empty());
        assert!(budget.helper_high_water >= MAX_FRAME_BYTES as u64);
    }
}
