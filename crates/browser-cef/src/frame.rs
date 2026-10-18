use egui::{Color32, ColorImage};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PixelRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl PixelRect {
    pub const fn full(width: i32, height: i32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    pub const fn is_empty(self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);
        Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }

    fn clamp_to(self, width: i32, height: i32) -> Self {
        let x = self.x.clamp(0, width);
        let y = self.y.clamp(0, height);
        Self {
            x,
            y,
            width: (self.x + self.width).clamp(0, width) - x,
            height: (self.y + self.height).clamp(0, height) - y,
        }
    }
}

/// Premultiplied BGRA pixels as CEF delivers them to `OnPaint`, in physical pixels.
#[derive(Default)]
pub struct PaintBuffer {
    bgra: Vec<u8>,
    width: i32,
    height: i32,
    seq: u64,
    pending: PixelRect,
}

pub struct Upload {
    pub at: [usize; 2],
    pub image: ColorImage,
    pub partial: bool,
}

impl PaintBuffer {
    pub fn size(&self) -> [usize; 2] {
        [self.width.max(0) as usize, self.height.max(0) as usize]
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Copies a paint; `source` must hold `width * height * 4` bytes.
    pub fn paint(&mut self, source: &[u8], width: i32, height: i32, dirty: &[PixelRect]) {
        let len = width.max(0) as usize * height.max(0) as usize * 4;
        if len == 0 || source.len() < len {
            return;
        }
        if (self.width, self.height) != (width, height) || self.bgra.len() != len {
            self.bgra = source[..len].to_vec();
            self.width = width;
            self.height = height;
            self.pending = PixelRect::full(width, height);
        } else {
            let dirty = dirty
                .iter()
                .fold(PixelRect::default(), |area, rect| area.union(*rect))
                .clamp_to(width, height);
            let stride = width as usize * 4;
            let (x0, x1) = (dirty.x as usize * 4, (dirty.x + dirty.width) as usize * 4);
            for y in dirty.y..dirty.y + dirty.height {
                let row = y as usize * stride;
                self.bgra[row + x0..row + x1].copy_from_slice(&source[row + x0..row + x1]);
            }
            self.pending = self.pending.union(dirty);
        }
        self.seq += 1;
    }

    /// What changed since the last upload: only the dirty area when the texture
    /// already has this size, otherwise the whole frame.
    pub fn take_upload(&mut self, texture_size: Option<[usize; 2]>) -> Option<Upload> {
        if self.bgra.is_empty() {
            return None;
        }
        let partial = texture_size == Some(self.size());
        let area = if partial {
            self.pending.clamp_to(self.width, self.height)
        } else {
            PixelRect::full(self.width, self.height)
        };
        self.pending = PixelRect::default();
        if area.is_empty() {
            return None;
        }
        Some(Upload {
            at: [area.x as usize, area.y as usize],
            image: self.rgba(area),
            partial,
        })
    }

    /// The whole current frame without consuming the pending upload.
    pub fn frame(&self) -> Option<ColorImage> {
        (!self.bgra.is_empty()).then(|| self.rgba(PixelRect::full(self.width, self.height)))
    }

    fn rgba(&self, area: PixelRect) -> ColorImage {
        let stride = self.width as usize * 4;
        let mut pixels = Vec::with_capacity((area.width * area.height) as usize);
        for y in area.y..area.y + area.height {
            let start = y as usize * stride + area.x as usize * 4;
            let row = &self.bgra[start..start + area.width as usize * 4];
            pixels.extend(
                row.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|[b, g, r, a]| Color32::from_rgba_premultiplied(*r, *g, *b, *a)),
            );
        }
        ColorImage::new([area.width as usize, area.height as usize], pixels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: i32, height: i32, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat((width * height) as usize)
    }

    #[test]
    fn first_paint_uploads_the_whole_frame_in_rgba() {
        let mut buffer = PaintBuffer::default();
        buffer.paint(&solid(3, 2, [10, 20, 30, 255]), 3, 2, &[]);

        let upload = buffer.take_upload(None).expect("first frame");

        assert!(!upload.partial);
        assert_eq!(upload.at, [0, 0]);
        assert_eq!(upload.image.size, [3, 2]);
        assert_eq!(
            upload.image.pixels[0],
            Color32::from_rgba_premultiplied(30, 20, 10, 255)
        );
        assert!(buffer.take_upload(Some([3, 2])).is_none());
    }

    #[test]
    fn later_paints_upload_only_their_dirty_area_until_the_size_changes() {
        let mut buffer = PaintBuffer::default();
        buffer.paint(&solid(4, 4, [0, 0, 0, 255]), 4, 4, &[]);
        buffer.take_upload(None);
        let dirty = [
            PixelRect {
                x: 1,
                y: 1,
                width: 1,
                height: 1,
            },
            PixelRect {
                x: 2,
                y: 2,
                width: 9,
                height: 9,
            },
        ];

        buffer.paint(&solid(4, 4, [255, 0, 0, 255]), 4, 4, &dirty);
        let upload = buffer.take_upload(Some([4, 4])).expect("dirty area");

        assert!(upload.partial);
        assert_eq!(upload.at, [1, 1]);
        assert_eq!(upload.image.size, [3, 3]);
        assert_eq!(upload.image.pixels[0], Color32::from_rgb(0, 0, 255));

        buffer.paint(&solid(4, 4, [0, 255, 0, 255]), 4, 4, &dirty);
        let resized = buffer
            .take_upload(Some([2, 2]))
            .expect("texture size differs");
        assert!(!resized.partial);
        assert_eq!(resized.image.size, [4, 4]);
        assert_eq!(buffer.seq(), 3);
    }

    #[test]
    fn frame_copies_the_whole_buffer_and_keeps_the_pending_upload() {
        let mut buffer = PaintBuffer::default();
        assert!(buffer.frame().is_none());
        buffer.paint(&solid(2, 2, [0, 0, 255, 255]), 2, 2, &[]);

        let frame = buffer.frame().expect("painted");

        assert_eq!(frame.size, [2, 2]);
        assert!(
            frame
                .pixels
                .iter()
                .all(|pixel| *pixel == Color32::from_rgb(255, 0, 0))
        );
        assert!(buffer.take_upload(None).is_some());
    }
}
