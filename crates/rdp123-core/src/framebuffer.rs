//! A thread-shared BGRA framebuffer with dirty-region tracking.
//!
//! The session task writes decoded pixels into it; the UI thread syncs a
//! presentation buffer from it via [`SharedFramebuffer::present_into`], which
//! copies only the regions that changed since that buffer was last synced.
//! Layout is tightly packed BGRA (4 bytes/pixel, top-down, stride = width * 4)
//! to match both IronRDP's `BgrX32` and CoreGraphics.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const BYTES_PER_PIXEL: usize = 4;

/// Cap on the dirty log. Overflow degrades gracefully: the log is cleared and
/// presenters that were synced before the overflow full-copy once.
const MAX_DIRTY_ENTRIES: usize = 128;

/// Exclusive dirty rectangle in framebuffer pixels.
#[derive(Debug, Clone, Copy)]
struct DirtyRect {
    left: u16,
    top: u16,
    right: u16,
    bottom: u16,
}

#[derive(Debug)]
struct Inner {
    width: u16,
    height: u16,
    pixels: Vec<u8>,
    /// Monotonic update counter; bumped by every write.
    generation: u64,
    /// Regions changed at a given generation, oldest first.
    dirty: VecDeque<(u64, DirtyRect)>,
    /// Presenters synced before this generation must copy everything (set on
    /// resize and on dirty-log overflow).
    full_dirty_generation: u64,
}

impl Inner {
    fn mark_dirty(&mut self, left: usize, top: usize, right: usize, bottom: usize) {
        self.generation += 1;
        if self.dirty.len() >= MAX_DIRTY_ENTRIES {
            self.dirty.clear();
            self.full_dirty_generation = self.generation;
            return;
        }
        // The values are clipped to the u16 framebuffer dimensions.
        #[expect(clippy::cast_possible_truncation)]
        self.dirty.push_back((
            self.generation,
            DirtyRect {
                left: left as u16,
                top: top as u16,
                right: right as u16,
                bottom: bottom as u16,
            },
        ));
    }
}

/// Shared, interior-mutable framebuffer handed to both the session and the UI.
#[derive(Debug)]
pub struct SharedFramebuffer {
    inner: Mutex<Inner>,
    /// Packed `width << 16 | height` for lock-free reads on the input path
    /// (queried on every mouse move).
    dims: AtomicU32,
}

impl SharedFramebuffer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                width: 0,
                height: 0,
                pixels: Vec::new(),
                generation: 0,
                dirty: VecDeque::new(),
                full_dirty_generation: 0,
            }),
            dims: AtomicU32::new(0),
        })
    }

    /// (Re)allocate for a new size, clearing to black.
    pub fn resize(&self, width: u16, height: u16) {
        let mut inner = self.inner.lock().unwrap();
        inner.width = width;
        inner.height = height;
        inner.pixels = vec![0u8; usize::from(width) * usize::from(height) * BYTES_PER_PIXEL];
        inner.generation += 1;
        inner.dirty.clear();
        inner.full_dirty_generation = inner.generation;
        self.dims.store(
            u32::from(width) << 16 | u32::from(height),
            Ordering::Release,
        );
    }

    pub fn dimensions(&self) -> (u16, u16) {
        let packed = self.dims.load(Ordering::Acquire);
        // Truncation is the point: the two u16 halves are unpacked.
        #[expect(clippy::cast_possible_truncation)]
        ((packed >> 16) as u16, packed as u16)
    }

    /// Copy a rectangle from a source buffer that shares this framebuffer's
    /// dimensions and packed-BGRA layout (i.e. IronRDP's `DecodedImage::data`).
    ///
    /// Out-of-bounds rectangles are clamped; a size mismatch is ignored so a
    /// late update arriving after a resize cannot panic.
    pub fn blit_rect(&self, src: &[u8], left: u16, top: u16, width: u16, height: u16) {
        let mut inner = self.inner.lock().unwrap();
        let fb_w = usize::from(inner.width);
        let fb_h = usize::from(inner.height);
        let stride = fb_w * BYTES_PER_PIXEL;
        if src.len() != inner.pixels.len() || stride == 0 {
            return;
        }
        let left = usize::from(left);
        let top = usize::from(top);
        let right = (left + usize::from(width)).min(fb_w);
        let bottom = (top + usize::from(height)).min(fb_h);
        if left >= right || top >= bottom {
            return;
        }
        let row_bytes = (right - left) * BYTES_PER_PIXEL;
        for y in top..bottom {
            let start = y * stride + left * BYTES_PER_PIXEL;
            let end = start + row_bytes;
            inner.pixels[start..end].copy_from_slice(&src[start..end]);
        }
        inner.mark_dirty(left, top, right, bottom);
    }

    /// Write a rectangle from a tightly packed BGRA buffer of exactly
    /// `width * height` pixels (EGFX surface flushes). Clipped to the
    /// framebuffer bounds; rows outside are dropped.
    pub fn write_rect(&self, src: &[u8], left: u32, top: u32, width: u32, height: u32) {
        let mut inner = self.inner.lock().unwrap();
        let fb_w = u32::from(inner.width);
        let fb_h = u32::from(inner.height);
        let src_stride = width as usize * BYTES_PER_PIXEL;
        if src.len() < src_stride * height as usize || fb_w == 0 {
            return;
        }
        let right = (left + width).min(fb_w);
        let bottom = (top + height).min(fb_h);
        if left >= right || top >= bottom {
            return;
        }
        let fb_stride = fb_w as usize * BYTES_PER_PIXEL;
        let row_bytes = (right - left) as usize * BYTES_PER_PIXEL;
        for row in 0..(bottom - top) as usize {
            let src_start = row * src_stride;
            let dst_start = (top as usize + row) * fb_stride + left as usize * BYTES_PER_PIXEL;
            inner.pixels[dst_start..dst_start + row_bytes]
                .copy_from_slice(&src[src_start..src_start + row_bytes]);
        }
        inner.mark_dirty(left as usize, top as usize, right as usize, bottom as usize);
    }

    /// Sync a presentation buffer with the current contents.
    ///
    /// `synced_generation` is the generation returned by the previous call for
    /// this same buffer (0 for a fresh buffer). Only regions dirtied after it
    /// are copied; a size mismatch or an out-of-range generation falls back to
    /// a full copy. Returns `(width, height, generation)` to store alongside
    /// the buffer, or `None` while the framebuffer is empty.
    pub fn present_into(
        &self,
        buf: &mut Vec<u8>,
        synced_generation: u64,
    ) -> Option<(u16, u16, u64)> {
        let inner = self.inner.lock().unwrap();
        if inner.pixels.is_empty() {
            return None;
        }
        let result = (inner.width, inner.height, inner.generation);

        if buf.len() != inner.pixels.len() || synced_generation < inner.full_dirty_generation {
            buf.clear();
            buf.extend_from_slice(&inner.pixels);
            return Some(result);
        }

        let stride = usize::from(inner.width) * BYTES_PER_PIXEL;
        for (generation, rect) in &inner.dirty {
            if *generation <= synced_generation {
                continue;
            }
            let row_bytes = usize::from(rect.right - rect.left) * BYTES_PER_PIXEL;
            for y in usize::from(rect.top)..usize::from(rect.bottom) {
                let start = y * stride + usize::from(rect.left) * BYTES_PER_PIXEL;
                buf[start..start + row_bytes]
                    .copy_from_slice(&inner.pixels[start..start + row_bytes]);
            }
        }
        Some(result)
    }

    /// Sync a strided destination (e.g. a locked IOSurface) with the current
    /// contents. Same contract as [`Self::present_into`], but the destination
    /// keeps its own row stride and must already be sized for the current
    /// dimensions — `None` is returned (nothing copied) when it is not, or
    /// while the framebuffer is empty.
    pub fn present_into_stride(
        &self,
        dst: &mut [u8],
        dst_stride: usize,
        synced_generation: u64,
    ) -> Option<(u16, u16, u64)> {
        let inner = self.inner.lock().unwrap();
        if inner.pixels.is_empty() {
            return None;
        }
        let width = usize::from(inner.width);
        let height = usize::from(inner.height);
        let src_stride = width * BYTES_PER_PIXEL;
        if dst_stride < src_stride
            || height == 0
            || dst.len() < (height - 1) * dst_stride + src_stride
        {
            return None;
        }
        let result = (inner.width, inner.height, inner.generation);

        let mut copy_rows = |left: usize, top: usize, right: usize, bottom: usize| {
            let row_bytes = (right - left) * BYTES_PER_PIXEL;
            for y in top..bottom {
                let src_start = y * src_stride + left * BYTES_PER_PIXEL;
                let dst_start = y * dst_stride + left * BYTES_PER_PIXEL;
                dst[dst_start..dst_start + row_bytes]
                    .copy_from_slice(&inner.pixels[src_start..src_start + row_bytes]);
            }
        };

        if synced_generation < inner.full_dirty_generation {
            copy_rows(0, 0, width, height);
            return Some(result);
        }
        for (generation, rect) in &inner.dirty {
            if *generation <= synced_generation {
                continue;
            }
            copy_rows(
                usize::from(rect.left),
                usize::from(rect.top),
                usize::from(rect.right),
                usize::from(rect.bottom),
            );
        }
        Some(result)
    }

    /// Run `f` with a borrow of the current pixels and dimensions.
    /// Returns `None` if the framebuffer is empty.
    pub fn with_pixels<R>(&self, f: impl FnOnce(&[u8], u16, u16) -> R) -> Option<R> {
        let inner = self.inner.lock().unwrap();
        if inner.pixels.is_empty() {
            return None;
        }
        Some(f(&inner.pixels, inner.width, inner.height))
    }

    /// Clone the current pixels and dimensions.
    pub fn snapshot(&self) -> Option<(Vec<u8>, u16, u16)> {
        let inner = self.inner.lock().unwrap();
        if inner.pixels.is_empty() {
            return None;
        }
        Some((inner.pixels.clone(), inner.width, inner.height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled_rect(width: u16, height: u16) -> Vec<u8> {
        vec![0xAB; usize::from(width) * usize::from(height) * BYTES_PER_PIXEL]
    }

    #[test]
    fn dimensions_track_resize_without_lock() {
        let fb = SharedFramebuffer::new();
        assert_eq!(fb.dimensions(), (0, 0));
        fb.resize(1794, 1134);
        assert_eq!(fb.dimensions(), (1794, 1134));
    }

    #[test]
    fn present_copies_only_regions_dirtied_since_sync() {
        let fb = SharedFramebuffer::new();
        fb.resize(4, 4);

        let mut buf = Vec::new();
        let (_, _, first_generation) = fb.present_into(&mut buf, 0).unwrap();

        // Corrupt a byte the next update must NOT touch (pixel 0,0).
        buf[0] = 0x77;

        // Dirty only the bottom-right pixel.
        fb.write_rect(&[1, 2, 3, 4], 3, 3, 1, 1);
        let (_, _, second_generation) = fb.present_into(&mut buf, first_generation).unwrap();

        assert!(second_generation > first_generation);
        // Partial copy: the corrupted byte outside the dirty region survives.
        assert_eq!(buf[0], 0x77);
        // The dirty pixel was synced.
        let offset = (3 * 4 + 3) * BYTES_PER_PIXEL;
        assert_eq!(&buf[offset..offset + 4], &[1, 2, 3, 4]);
    }

    #[test]
    fn present_full_copies_on_size_mismatch() {
        let fb = SharedFramebuffer::new();
        fb.resize(2, 2);
        let mut buf = vec![0u8; 3];
        fb.present_into(&mut buf, u64::MAX).unwrap();
        assert_eq!(buf.len(), 2 * 2 * BYTES_PER_PIXEL);
    }

    #[test]
    fn resize_forces_full_copy_for_stale_presenters() {
        let fb = SharedFramebuffer::new();
        fb.resize(2, 2);
        let mut buf = Vec::new();
        let (_, _, generation) = fb.present_into(&mut buf, 0).unwrap();

        buf[0] = 0x55;
        fb.resize(2, 2); // same size, but content reset
        fb.present_into(&mut buf, generation).unwrap();
        assert_eq!(buf[0], 0x00);
    }

    #[test]
    fn dirty_log_overflow_degrades_to_full_copy() {
        let fb = SharedFramebuffer::new();
        fb.resize(4, 4);
        let mut buf = Vec::new();
        let (_, _, generation) = fb.present_into(&mut buf, 0).unwrap();

        buf[0] = 0x99;
        for _ in 0..(MAX_DIRTY_ENTRIES + 1) {
            fb.write_rect(&[1, 2, 3, 4], 3, 3, 1, 1);
        }
        fb.present_into(&mut buf, generation).unwrap();
        // Overflow cleared the log, so the presenter full-copied.
        assert_eq!(buf[0], 0x00);
    }

    #[test]
    fn strided_present_respects_destination_stride() {
        let fb = SharedFramebuffer::new();
        fb.resize(2, 2);
        fb.write_rect(&[1, 2, 3, 4], 1, 1, 1, 1);

        // Destination rows padded to 16 bytes (stride > 2 px * 4).
        let stride = 16;
        let mut dst = vec![0xEE; stride * 2];
        let (_, _, generation) = fb.present_into_stride(&mut dst, stride, 0).unwrap();

        // Row 1, pixel 1 lands at stride + 4.
        assert_eq!(&dst[stride + 4..stride + 8], &[1, 2, 3, 4]);
        // Padding bytes are untouched.
        assert_eq!(dst[stride - 1], 0xEE);

        // Partial sync: corrupt outside, dirty one pixel, sync again.
        dst[0] = 0x11;
        fb.write_rect(&[9, 9, 9, 9], 0, 1, 1, 1);
        fb.present_into_stride(&mut dst, stride, generation)
            .unwrap();
        assert_eq!(dst[0], 0x11);
        assert_eq!(&dst[stride..stride + 4], &[9, 9, 9, 9]);
    }

    #[test]
    fn strided_present_rejects_undersized_destination() {
        let fb = SharedFramebuffer::new();
        fb.resize(4, 4);
        let mut dst = vec![0u8; 8];
        assert!(fb.present_into_stride(&mut dst, 16, 0).is_none());
        assert!(fb.present_into_stride(&mut vec![0u8; 1024], 8, 0).is_none());
    }

    #[test]
    fn blit_rect_marks_dirty() {
        let fb = SharedFramebuffer::new();
        fb.resize(4, 4);
        let mut buf = Vec::new();
        let (_, _, generation) = fb.present_into(&mut buf, 0).unwrap();

        let src = filled_rect(4, 4);
        fb.blit_rect(&src, 1, 1, 2, 2);
        fb.present_into(&mut buf, generation).unwrap();

        let inside = (4 + 1) * BYTES_PER_PIXEL;
        assert_eq!(buf[inside], 0xAB);
        assert_eq!(buf[0], 0x00);
    }
}
