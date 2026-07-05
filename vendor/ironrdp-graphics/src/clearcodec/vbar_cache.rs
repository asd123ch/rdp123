//! V-Bar caching for ClearCodec bands layer.
//!
//! The V-bar cache uses two ring buffers:
//! - **V-Bar Storage**: 32,768 full V-bars (complete column pixel data for a band height)
//! - **Short V-Bar Storage**: 16,384 short V-bars (only the non-background portion)
//!
//! Cache cursors advance linearly and wrap around, implementing LRU eviction
//! as specified in MS-RDPEGFX 3.3.8.1.

use ironrdp_pdu::codecs::clearcodec::{SHORT_VBAR_CACHE_SIZE, VBAR_CACHE_SIZE};

// VBAR_CACHE_SIZE (32,768) and SHORT_VBAR_CACHE_SIZE (16,384) as u16 for cursor wrapping.
const VBAR_WRAP: u16 = 32_768;
const SHORT_VBAR_WRAP: u16 = 16_384;

/// A full V-bar: column of BGR pixels for the full band height.
#[derive(Debug, Clone)]
pub struct FullVBar {
    /// BGR pixel data, length = band_height * 3.
    pub pixels: Vec<u8>,
}

/// A short V-bar: only the non-background pixels within a column.
#[derive(Debug, Clone)]
pub struct ShortVBar {
    /// First row index where pixel data starts.
    pub y_on: u8,
    /// Number of pixel rows with color data.
    pub pixel_count: u8,
    /// BGR pixel data, length = pixel_count * 3.
    pub pixels: Vec<u8>,
}

/// Combined V-bar cache state.
pub struct VBarCache {
    /// Full V-bar storage (32,768 entries, ring buffer).
    vbar_storage: Vec<Option<FullVBar>>,
    /// Short V-bar storage (16,384 entries, ring buffer).
    short_vbar_storage: Vec<Option<ShortVBar>>,
    /// Current write cursor for V-bar storage (wraps at 32767).
    vbar_cursor: u16,
    /// Current write cursor for short V-bar storage (wraps at 16383).
    short_vbar_cursor: u16,
}

impl VBarCache {
    pub fn new() -> Self {
        let mut vbar_storage = Vec::with_capacity(VBAR_CACHE_SIZE);
        vbar_storage.resize_with(VBAR_CACHE_SIZE, || None);

        let mut short_vbar_storage = Vec::with_capacity(SHORT_VBAR_CACHE_SIZE);
        short_vbar_storage.resize_with(SHORT_VBAR_CACHE_SIZE, || None);

        Self {
            vbar_storage,
            short_vbar_storage,
            vbar_cursor: 0,
            short_vbar_cursor: 0,
        }
    }

    /// Reset both caches (when FLAG_CACHE_RESET is received).
    pub fn reset(&mut self) {
        self.vbar_cursor = 0;
        self.short_vbar_cursor = 0;
        // Per spec, only cursors reset. Existing entries become stale
        // but the cursor reset means new entries overwrite from index 0.
    }

    /// Get a full V-bar from cache by index.
    pub fn get_vbar(&self, index: u16) -> Option<&FullVBar> {
        self.vbar_storage
            .get(usize::from(index))
            .and_then(|slot| slot.as_ref())
    }

    /// Get a full V-bar fitted to the current band height.
    ///
    /// A cache entry can be reused by a band with a different height. The
    /// reference decoder limits the entry's active pixel count to that height;
    /// returning the original longer column would overwrite rows below the
    /// current band.
    pub fn get_vbar_for_height(&self, index: u16, band_height: u16) -> Option<FullVBar> {
        let cached = self.get_vbar(index)?;
        let byte_count = usize::from(band_height) * 3;
        let mut pixels = cached.pixels[..cached.pixels.len().min(byte_count)].to_vec();
        pixels.resize(byte_count, 0);
        Some(FullVBar { pixels })
    }

    /// Get a short V-bar from cache by index.
    pub fn get_short_vbar(&self, index: u16) -> Option<&ShortVBar> {
        self.short_vbar_storage
            .get(usize::from(index))
            .and_then(|slot| slot.as_ref())
    }

    /// Store a short V-bar and return its cache index.
    pub fn store_short_vbar(&mut self, short_vbar: ShortVBar) -> u16 {
        let index = self.short_vbar_cursor;
        self.short_vbar_storage[usize::from(index)] = Some(short_vbar);
        self.short_vbar_cursor = (index + 1) % SHORT_VBAR_WRAP;
        index
    }

    /// Store a full V-bar and return its cache index.
    pub fn store_vbar(&mut self, vbar: FullVBar) -> u16 {
        let index = self.vbar_cursor;
        self.vbar_storage[usize::from(index)] = Some(vbar);
        self.vbar_cursor = (index + 1) % VBAR_WRAP;
        index
    }

    /// Fill a specific slot without advancing the cursor.
    ///
    /// Used for the reference decoder's dummy-data path: a cache hit on an
    /// empty slot persists the dummy entry at the referenced index.
    pub fn put_vbar_at(&mut self, index: u16, vbar: FullVBar) {
        if let Some(slot) = self.vbar_storage.get_mut(usize::from(index)) {
            *slot = Some(vbar);
        }
    }

    /// Reconstruct a full V-bar from a short V-bar and background color.
    ///
    /// The full V-bar has:
    /// - Background color above y_on
    /// - Short V-bar pixel data from y_on to y_on + pixel_count
    /// - Background color below y_on + pixel_count
    pub fn reconstruct_full_vbar(
        short_vbar: &ShortVBar,
        band_height: u16,
        bg_blue: u8,
        bg_green: u8,
        bg_red: u8,
    ) -> FullVBar {
        let height = usize::from(band_height);
        let mut pixels = vec![0u8; height * 3];
        for pixel in pixels.chunks_exact_mut(3) {
            pixel.copy_from_slice(&[bg_blue, bg_green, bg_red]);
        }

        let start = usize::from(short_vbar.y_on).min(height);
        let available_rows = short_vbar.pixels.len() / 3;
        let requested_rows = usize::from(short_vbar.pixel_count).min(available_rows);
        let copy_rows = requested_rows.min(height - start);
        if copy_rows > 0 {
            let dst_start = start * 3;
            let byte_count = copy_rows * 3;
            pixels[dst_start..dst_start + byte_count]
                .copy_from_slice(&short_vbar.pixels[..byte_count]);
        }

        FullVBar { pixels }
    }
}

impl Default for VBarCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_and_retrieve_vbar() {
        let mut cache = VBarCache::new();
        let vbar = FullVBar {
            pixels: vec![0xFF, 0x00, 0x00],
        };
        let idx = cache.store_vbar(vbar);
        assert_eq!(idx, 0);
        let retrieved = cache.get_vbar(0).unwrap();
        assert_eq!(retrieved.pixels, vec![0xFF, 0x00, 0x00]);
    }

    #[test]
    fn cursor_wraps() {
        let mut cache = VBarCache::new();
        // Store VBAR_CACHE_SIZE entries, cursor should wrap to 0
        for i in 0..VBAR_CACHE_SIZE {
            let idx = cache.store_vbar(FullVBar {
                pixels: vec![u8::try_from(i & 0xFF).unwrap()],
            });
            assert_eq!(idx, u16::try_from(i).unwrap());
        }
        // Next store should be at index 0 (wrapped)
        let idx = cache.store_vbar(FullVBar { pixels: vec![0xAA] });
        assert_eq!(idx, 0);
    }

    #[test]
    fn reconstruct_full_vbar() {
        let short = ShortVBar {
            y_on: 1,
            pixel_count: 2,
            pixels: vec![0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00], // 2 pixels BGR
        };
        let full = VBarCache::reconstruct_full_vbar(&short, 4, 0xAA, 0xBB, 0xCC);
        // Height=4: 1 bg row, 2 data rows, 1 bg row
        assert_eq!(full.pixels.len(), 12); // 4 * 3
        // Row 0: background
        assert_eq!(&full.pixels[0..3], &[0xAA, 0xBB, 0xCC]);
        // Row 1-2: pixel data
        assert_eq!(&full.pixels[3..9], &[0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00]);
        // Row 3: background
        assert_eq!(&full.pixels[9..12], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn reconstruct_full_vbar_clips_pixels_below_band() {
        let short = ShortVBar {
            y_on: 3,
            pixel_count: 3,
            pixels: vec![
                1, 2, 3, //
                4, 5, 6, //
                7, 8, 9,
            ],
        };
        let full = VBarCache::reconstruct_full_vbar(&short, 4, 0xAA, 0xBB, 0xCC);

        assert_eq!(full.pixels.len(), 12);
        assert_eq!(&full.pixels[9..12], &[1, 2, 3]);
    }

    #[test]
    fn cache_hit_is_fitted_to_current_band_height() {
        let mut cache = VBarCache::new();
        cache.store_vbar(FullVBar {
            pixels: (0..18).collect(),
        });

        let fitted = cache.get_vbar_for_height(0, 3).unwrap();
        assert_eq!(fitted.pixels, (0..9).collect::<Vec<_>>());
    }

    #[test]
    fn put_vbar_at_fills_slot_without_moving_cursor() {
        let mut cache = VBarCache::new();
        cache.put_vbar_at(5, FullVBar { pixels: vec![1, 2, 3] });

        assert_eq!(cache.get_vbar(5).unwrap().pixels, vec![1, 2, 3]);
        assert_eq!(cache.vbar_cursor, 0);
    }

    #[test]
    fn reset_resets_cursors() {
        let mut cache = VBarCache::new();
        cache.store_vbar(FullVBar { pixels: vec![0x01] });
        cache.store_short_vbar(ShortVBar {
            y_on: 0,
            pixel_count: 0,
            pixels: vec![],
        });
        assert_eq!(cache.vbar_cursor, 1);
        assert_eq!(cache.short_vbar_cursor, 1);
        cache.reset();
        assert_eq!(cache.vbar_cursor, 0);
        assert_eq!(cache.short_vbar_cursor, 0);
    }
}
