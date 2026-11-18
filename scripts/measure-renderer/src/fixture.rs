//! The page the real helper renders, and the decoder for its time stamp.
//!
//! Every animation frame the page fills the whole canvas with a changing
//! colour and stamps the time in 32 bits into four 8x8 blocks at the top left
//! (the blue channel of each block carries one byte). The time is
//! `performance.timeOrigin + performance.now()` in units of 0.1 ms, taken
//! modulo 2^32, so it is comparable with `CLOCK_REALTIME` of this process.

use std::time::{SystemTime, UNIX_EPOCH};

pub const HTML: &str = r##"<!doctype html><html><body style="margin:0;overflow:hidden;background:#000">
<canvas id="c"></canvas>
<script>
const c = document.getElementById('c');
c.width = window.innerWidth;
c.height = window.innerHeight;
const g = c.getContext('2d', { alpha: false });
let n = 0;
(function draw() {
  n += 1;
  const v = n % 200;
  g.fillStyle = 'rgb(' + v + ',' + (255 - v) + ',160)';
  g.fillRect(0, 0, c.width, c.height);
  const t = Math.floor((performance.timeOrigin + performance.now()) * 10) % 4294967296;
  for (let i = 0; i < 4; i++) {
    const byte = Math.floor(t / Math.pow(256, i)) % 256;
    g.fillStyle = 'rgb(' + byte + ',' + byte + ',' + byte + ')';
    g.fillRect(i * 8, 0, 8, 8);
  }
  requestAnimationFrame(draw);
})();
</script></body></html>"##;

const BLOCK: usize = 8;

/// Units of 0.1 ms since the epoch, modulo 2^32.
pub fn now_ticks() -> u32 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    ((d.as_nanos() / 100_000) % (1u128 << 32)) as u32
}

/// The page's time stamp of a BGRA frame, or `None` when the frame is too
/// small to hold the blocks.
pub fn stamp(bgra: &[u8], width: u32) -> Option<u32> {
    let width = width as usize;
    if width < BLOCK * 4 || bgra.len() < width * BLOCK * 4 {
        return None;
    }
    let mut value = 0u32;
    for i in 0..4 {
        // Centre pixel of block i; byte 0 of a BGRA pixel is blue.
        let offset = ((BLOCK / 2) * width + i * BLOCK + BLOCK / 2) * 4;
        value |= u32::from(bgra[offset]) << (8 * i);
    }
    Some(value)
}

/// Age in milliseconds of a stamp, `None` when it is not plausible (negative
/// beyond clock noise or older than a minute: a half-painted or mis-decoded
/// frame).
pub fn age_ms(stamp: u32, now: u32) -> Option<f64> {
    let delta = now.wrapping_sub(stamp);
    let signed = delta as i32;
    let ms = f64::from(signed) / 10.0;
    (-5.0..60_000.0).contains(&ms).then_some(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A BGRA frame whose stamp blocks carry `value`, as the page draws them.
    fn frame_with(value: u32, width: usize, height: usize) -> Vec<u8> {
        let mut bgra = vec![0x40; width * height * 4];
        for i in 0..4 {
            for y in 0..BLOCK {
                for x in 0..BLOCK {
                    let at = (y * width + i * BLOCK + x) * 4;
                    bgra[at] = (value >> (8 * i)) as u8;
                }
            }
        }
        bgra
    }

    #[test]
    fn stamp_round_trips() {
        for value in [0, 1, 0x1234_5678, u32::MAX] {
            assert_eq!(stamp(&frame_with(value, 64, 16), 64), Some(value));
        }
    }

    #[test]
    fn stamp_needs_room_for_the_blocks() {
        assert_eq!(stamp(&[0; 100], 8), None);
        assert_eq!(stamp(&vec![0; 64 * 4], 64), None); // one row, blocks need 8
    }

    #[test]
    fn age_is_in_milliseconds_and_survives_wrap() {
        assert_eq!(age_ms(1000, 1500), Some(50.0));
        assert_eq!(age_ms(u32::MAX - 9, 10), Some(2.0)); // 20 ticks across the wrap
    }

    #[test]
    fn implausible_ages_are_rejected() {
        assert_eq!(age_ms(2_000_000, 0), None); // far in the future
        assert_eq!(age_ms(0, 700_000), None); // older than a minute
        assert!(age_ms(100, 99).is_some()); // 0.1 ms of clock noise is accepted
    }
}
