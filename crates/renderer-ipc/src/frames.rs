//! The helper's bounded frame queue.
//!
//! CEF paints on its UI thread and must never wait for the socket. The queue
//! keeps at most one pending frame per tab: a newer paint replaces an older
//! one that was not sent yet (counted as dropped), so a slow receiver costs
//! frames, not memory. One frame per tab is in flight on the socket at a time
//! (the sender waits for the core's acknowledgement), and replaced buffers are
//! recycled, so a steady stream does not allocate per frame.
//!
//! Pixel memory is therefore bounded by `MAX_TABS` pending frames, one
//! in-flight frame and [`SPARE_BUFFERS`] recycled buffers, each at most
//! [`MAX_FRAME_BYTES`](crate::protocol::MAX_FRAME_BYTES).

use std::collections::HashSet;
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::protocol::{MAX_FRAME_BYTES, TabId};

pub const SPARE_BUFFERS: usize = 2;

#[derive(Debug)]
pub struct Frame {
    pub tab: TabId,
    /// Queue-wide sequence number, increasing in publication order.
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    /// Premultiplied BGRA, `width * height * 4` bytes.
    pub bgra: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    /// Frames accepted into the queue.
    pub published: u64,
    /// Accepted frames replaced by a newer one before they were taken.
    pub dropped: u64,
    /// Frames handed to the sender.
    pub taken: u64,
    /// Paints refused: tab not streaming, queue closed or malformed size.
    pub ignored: u64,
}

#[derive(Default)]
struct State {
    pending: Vec<Frame>,
    streaming: HashSet<TabId>,
    spare: Vec<Vec<u8>>,
    next_seq: u64,
    closed: bool,
    stats: FrameStats,
}

#[derive(Default)]
pub struct FrameQueue {
    state: Mutex<State>,
    ready: Condvar,
}

impl FrameQueue {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts or stops accepting frames of `tab`. Stopping discards the tab's
    /// pending frame.
    pub fn set_streaming(&self, tab: TabId, enabled: bool) {
        let mut state = self.lock();
        if enabled {
            state.streaming.insert(tab);
            return;
        }
        state.streaming.remove(&tab);
        if let Some(index) = state.pending.iter().position(|f| f.tab == tab) {
            let frame = state.pending.swap_remove(index);
            Self::keep_spare(&mut state, frame.bgra);
        }
    }

    pub fn is_streaming(&self, tab: TabId) -> bool {
        self.lock().streaming.contains(&tab)
    }

    /// Offers one painted frame. Called from CEF's UI thread: it copies the
    /// pixels (outside the lock) and never blocks on the receiver. Returns
    /// whether the frame was queued.
    pub fn publish(&self, tab: TabId, width: u32, height: u32, bgra: &[u8]) -> bool {
        let well_formed = width > 0
            && height > 0
            && bgra.len() <= MAX_FRAME_BYTES
            && bgra.len() == width as usize * height as usize * 4;
        let mut buffer = {
            let mut state = self.lock();
            if !well_formed || state.closed || !state.streaming.contains(&tab) {
                state.stats.ignored += 1;
                return false;
            }
            state.spare.pop().unwrap_or_default()
        };
        buffer.clear();
        buffer.extend_from_slice(bgra);
        let mut state = self.lock();
        if state.closed || !state.streaming.contains(&tab) {
            state.stats.ignored += 1;
            Self::keep_spare(&mut state, buffer);
            return false;
        }
        state.next_seq += 1;
        let frame = Frame {
            tab,
            seq: state.next_seq,
            width,
            height,
            bgra: buffer,
        };
        if let Some(index) = state.pending.iter().position(|f| f.tab == tab) {
            let old = std::mem::replace(&mut state.pending[index], frame);
            state.stats.dropped += 1;
            Self::keep_spare(&mut state, old.bgra);
        } else {
            state.pending.push(frame);
        }
        state.stats.published += 1;
        drop(state);
        self.ready.notify_one();
        true
    }

    /// Waits for the oldest pending frame; `None` once the queue is closed.
    pub fn next(&self) -> Option<Frame> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return None;
            }
            let oldest = state
                .pending
                .iter()
                .enumerate()
                .min_by_key(|(_, frame)| frame.seq)
                .map(|(index, _)| index);
            if let Some(index) = oldest {
                state.stats.taken += 1;
                return Some(state.pending.swap_remove(index));
            }
            state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Returns a sent frame's buffer for reuse.
    pub fn recycle(&self, buffer: Vec<u8>) {
        let mut state = self.lock();
        Self::keep_spare(&mut state, buffer);
    }

    /// Stops the queue: pending frames are discarded, publishers are refused
    /// and [`next`](Self::next) returns `None`.
    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.pending.clear();
        drop(state);
        self.ready.notify_all();
    }

    pub fn stats(&self) -> FrameStats {
        self.lock().stats
    }

    fn keep_spare(state: &mut State, buffer: Vec<u8>) {
        if state.spare.len() < SPARE_BUFFERS {
            state.spare.push(buffer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAB: TabId = TabId(1);

    fn pixels(width: u32, height: u32, fill: u8) -> Vec<u8> {
        vec![fill; width as usize * height as usize * 4]
    }

    #[test]
    fn a_newer_paint_replaces_the_pending_frame_of_the_same_tab() {
        let queue = FrameQueue::default();
        queue.set_streaming(TAB, true);
        assert!(queue.publish(TAB, 2, 2, &pixels(2, 2, 1)));
        assert!(queue.publish(TAB, 2, 2, &pixels(2, 2, 2)));
        assert!(queue.publish(TAB, 2, 2, &pixels(2, 2, 3)));
        let frame = queue.next().unwrap();
        assert_eq!(frame.bgra, pixels(2, 2, 3));
        assert_eq!(frame.seq, 3);
        assert_eq!(
            queue.stats(),
            FrameStats {
                published: 3,
                dropped: 2,
                taken: 1,
                ignored: 0
            }
        );
    }

    #[test]
    fn frames_of_different_tabs_are_taken_oldest_first() {
        let queue = FrameQueue::default();
        queue.set_streaming(TabId(1), true);
        queue.set_streaming(TabId(2), true);
        queue.publish(TabId(2), 1, 1, &pixels(1, 1, 2));
        queue.publish(TabId(1), 1, 1, &pixels(1, 1, 1));
        assert_eq!(queue.next().unwrap().tab, TabId(2));
        assert_eq!(queue.next().unwrap().tab, TabId(1));
    }

    #[test]
    fn paints_of_tabs_that_do_not_stream_or_with_a_wrong_size_are_ignored() {
        let queue = FrameQueue::default();
        assert!(!queue.publish(TAB, 1, 1, &pixels(1, 1, 0)));
        queue.set_streaming(TAB, true);
        assert!(
            !queue.publish(TAB, 2, 2, &pixels(1, 1, 0)),
            "length mismatch"
        );
        assert!(!queue.publish(TAB, 0, 1, &[]), "zero width");
        assert_eq!(queue.stats().ignored, 3);
        assert_eq!(queue.stats().published, 0);
    }

    #[test]
    fn stopping_a_stream_discards_its_pending_frame() {
        let queue = FrameQueue::default();
        queue.set_streaming(TAB, true);
        queue.publish(TAB, 1, 1, &pixels(1, 1, 5));
        queue.set_streaming(TAB, false);
        queue.set_streaming(TabId(2), true);
        queue.publish(TabId(2), 1, 1, &pixels(1, 1, 6));
        assert_eq!(queue.next().unwrap().tab, TabId(2));
    }

    #[test]
    fn replaced_and_recycled_buffers_are_reused() {
        let queue = FrameQueue::default();
        queue.set_streaming(TAB, true);
        queue.publish(TAB, 4, 4, &pixels(4, 4, 1));
        let first = queue.next().unwrap();
        let pointer = first.bgra.as_ptr();
        queue.recycle(first.bgra);
        queue.publish(TAB, 4, 4, &pixels(4, 4, 2));
        assert_eq!(queue.next().unwrap().bgra.as_ptr(), pointer);
    }

    #[test]
    fn closing_wakes_a_waiting_sender_and_refuses_publishers() {
        let queue = std::sync::Arc::new(FrameQueue::default());
        queue.set_streaming(TAB, true);
        let waiter = {
            let queue = std::sync::Arc::clone(&queue);
            std::thread::spawn(move || queue.next().is_none())
        };
        queue.close();
        assert!(waiter.join().unwrap());
        assert!(!queue.publish(TAB, 1, 1, &pixels(1, 1, 0)));
    }
}
