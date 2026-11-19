//! Frames the core received from the helper: one allocation, many readers.
//!
//! The core keeps **one** authoritative latest frame per tab (the client's
//! slot). Every consumer (the data-plane senders of each subscription) takes an
//! `Arc` to that same allocation, so fan-out copies nothing; the F3
//! measurement (`docs/renderer-measurement.md` §8) showed a per-subscriber
//! copy misses the 33 ms p95 and multiplies peak memory.
//!
//! All pixel memory the core holds (the frame being received, the latest
//! frames, frames a sender still holds, recycled buffers) is charged to one
//! [`FrameBudget`]. Room is reserved **before** the body is read and returned
//! when the last holder drops the frame, so a stalled reader pins memory but
//! can never push the total past the budget: a frame that does not fit is read
//! off the socket and discarded (and acknowledged, so the helper never waits
//! on a data client).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::protocol::TabId;

/// Hard budget for raw pixel memory held by the core (plan §2.4).
pub const FRAME_BUDGET_BYTES: usize = 512 * 1024 * 1024;

/// Recycled buffers kept for the next frame.
pub const SPARE_FRAMES: usize = 2;

/// A byte budget. Reservations are exact and never exceed `limit`.
#[derive(Debug)]
pub struct FrameBudget {
    limit: usize,
    used: AtomicUsize,
}

impl FrameBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
        })
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Bytes currently reserved.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    fn reserve(self: &Arc<Self>, bytes: usize) -> Option<Permit> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let next = used.checked_add(bytes).filter(|next| *next <= self.limit)?;
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Some(Permit {
                        budget: Arc::clone(self),
                        bytes,
                    });
                }
                Err(current) => used = current,
            }
        }
    }
}

/// Reserved room; returned to the budget on drop.
#[derive(Debug)]
struct Permit {
    budget: Arc<FrameBudget>,
    bytes: usize,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct Buffer {
    bytes: Vec<u8>,
    permit: Permit,
}

/// Hands out budgeted buffers and recycles them.
#[derive(Debug)]
pub struct FramePool {
    budget: Arc<FrameBudget>,
    spare: Mutex<Vec<Buffer>>,
}

/// A buffer for one incoming frame together with its reservation. Hand the
/// vector to the codec and give the rest back through [`FramePool::finish`].
#[derive(Debug)]
pub struct Reserved {
    pub bytes: Vec<u8>,
    permit: Permit,
}

impl FramePool {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            budget: FrameBudget::new(limit),
            spare: Mutex::new(Vec::new()),
        })
    }

    pub fn budget(&self) -> &Arc<FrameBudget> {
        &self.budget
    }

    fn spares(&self) -> MutexGuard<'_, Vec<Buffer>> {
        self.spare.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reserves room for `len` bytes, reusing a recycled buffer of a similar
    /// size when there is one. `None`: the budget cannot hold it.
    pub fn reserve(&self, len: usize) -> Option<Reserved> {
        {
            let mut spare = self.spares();
            // Reuse only a buffer that is big enough and not wastefully big,
            // because its whole reservation stays charged to the new frame.
            if let Some(index) = spare
                .iter()
                .position(|b| b.permit.bytes >= len && b.permit.bytes / 2 <= len)
            {
                let Buffer { bytes, permit } = spare.swap_remove(index);
                return Some(Reserved { bytes, permit });
            }
        }
        if let Some(permit) = self.budget.reserve(len) {
            return Some(Reserved {
                bytes: Vec::new(),
                permit,
            });
        }
        // Recycled buffers are the only memory nobody needs: free them and
        // try once more before refusing the frame.
        self.spares().clear();
        let permit = self.budget.reserve(len)?;
        Some(Reserved {
            bytes: Vec::new(),
            permit,
        })
    }

    /// Wraps the filled buffer of a reservation into a shared frame.
    pub fn finish(
        self: &Arc<Self>,
        reserved: Reserved,
        bytes: Vec<u8>,
        tab: TabId,
        seq: u64,
        width: u32,
        height: u32,
    ) -> Arc<ReceivedFrame> {
        Arc::new(ReceivedFrame {
            tab,
            seq,
            width,
            height,
            bgra: bytes,
            permit: Some(reserved.permit),
            pool: Arc::clone(self),
        })
    }

    /// Drops every recycled buffer (a closed connection).
    pub fn clear(&self) {
        self.spares().clear();
    }

    fn recycle(&self, buffer: Buffer) {
        let mut spare = self.spares();
        if spare.len() < SPARE_FRAMES {
            spare.push(buffer);
        }
    }
}

/// One frame as the core received it. Immutable and shared; the buffer returns
/// to the pool, and the reservation to the budget, when the last `Arc` drops.
#[derive(Debug)]
pub struct ReceivedFrame {
    pub tab: TabId,
    /// The helper's queue-wide sequence number.
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    /// Premultiplied BGRA, `width * height * 4` bytes.
    pub bgra: Vec<u8>,
    permit: Option<Permit>,
    pool: Arc<FramePool>,
}

impl Drop for ReceivedFrame {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            self.pool.recycle(Buffer {
                bytes: std::mem::take(&mut self.bgra),
                permit,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(pool: &Arc<FramePool>, len: usize) -> Option<Arc<ReceivedFrame>> {
        let mut reserved = pool.reserve(len)?;
        let mut bytes = std::mem::take(&mut reserved.bytes);
        bytes.clear();
        bytes.resize(len, 7);
        Some(pool.finish(reserved, bytes, TabId(1), 1, len as u32 / 4, 1))
    }

    #[test]
    fn a_reservation_is_charged_until_the_last_holder_drops() {
        let pool = FramePool::new(100);
        let first = frame(&pool, 40).unwrap();
        let shared = Arc::clone(&first);
        assert_eq!(pool.budget().used(), 40);
        drop(first);
        assert_eq!(pool.budget().used(), 40, "a second holder keeps it charged");
        drop(shared);
        // The buffer is kept for reuse, still charged, until reused or cleared.
        assert_eq!(pool.budget().used(), 40);
        pool.clear();
        assert_eq!(pool.budget().used(), 0);
    }

    #[test]
    fn the_budget_is_never_exceeded_and_spares_are_freed_before_refusing() {
        let pool = FramePool::new(100);
        let a = frame(&pool, 60).unwrap();
        assert!(frame(&pool, 60).is_none(), "60 + 60 does not fit in 100");
        drop(a);
        // The recycled 60 is not a fit for 100 bytes, so it is freed.
        let b = frame(&pool, 100).expect("spares are released to make room");
        assert_eq!(pool.budget().used(), 100);
        assert!(frame(&pool, 1).is_none());
        drop(b);
    }

    #[test]
    fn a_recycled_buffer_is_reused_without_a_new_allocation() {
        let pool = FramePool::new(1000);
        let first = frame(&pool, 400).unwrap();
        let address = first.bgra.as_ptr();
        drop(first);
        let second = frame(&pool, 400).unwrap();
        assert_eq!(second.bgra.as_ptr(), address);
        assert_eq!(pool.budget().used(), 400);
    }

    #[test]
    fn a_wastefully_large_spare_is_not_reused_for_a_small_frame() {
        let pool = FramePool::new(1000);
        drop(frame(&pool, 800).unwrap());
        let small = frame(&pool, 100).unwrap();
        // The 800-byte spare stays in the pool, charged beside the new frame.
        assert_eq!(pool.budget().used(), 900);
        drop(small);
    }

    #[test]
    fn oversized_requests_are_refused_without_overflow() {
        let pool = FramePool::new(100);
        assert!(pool.reserve(usize::MAX).is_none());
        assert_eq!(pool.budget().used(), 0);
    }
}
