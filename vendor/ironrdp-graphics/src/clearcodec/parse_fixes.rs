//! Corrected ClearCodec layer parsers.
//!
//! Two parsing bugs in `ironrdp-pdu` 0.8.0 make real Windows ClearCodec
//! streams fail to decode. Both were confirmed against live captures and the
//! reference implementation, so the affected layers are re-parsed here (the
//! upstream data types are reused; only the byte-level readers differ):
//!
//! 1. **RLEX with `paletteCount == 1`.** Upstream special-cases a one-entry
//!    palette as a bare sequence of run lengths. The bit width is actually
//!    `floor(log2(paletteCount - 1)) + 1`, which is **1** even for a single
//!    entry (the reference table maps 0 to 0, plus one) — every segment still
//!    carries the packed stopIndex/suiteDepth byte and contributes
//!    `suiteDepth + 1` suite pixels after its run. The upstream reading both
//!    consumes the wrong bytes and drops the implicit suite pixel, so streams
//!    fail with "suite exceeds region pixel count".
//! 2. **`SHORT_VBAR_CACHE_MISS` bit layout.** Upstream reads `shortVBarYOn`
//!    from bits 13:6 and `shortVBarYOff` from bits 5:0 of the header word.
//!    The actual layout is `yOn` = bits 7:0 and `yOff` = bits 13:8, so real
//!    headers decode as `yOff < yOn` and the band layer is rejected.

use ironrdp_core::{DecodeResult, ReadCursor, ensure_size, invalid_field_err};
use ironrdp_pdu::codecs::clearcodec::{
    Band, MAX_BAND_HEIGHT, MAX_PALETTE_COUNT, RlexData, RlexSegment, ShortVBarCacheMiss, VBar,
};

/// Decode RLEX subcodec data with the correct bit widths for every palette
/// size.
pub(crate) fn decode_rlex(data: &[u8]) -> DecodeResult<RlexData> {
    let mut src = ReadCursor::new(data);

    ensure_size!(ctx: "RlexPalette", in: src, size: 1);
    let palette_count = src.read_u8();

    if palette_count == 0 {
        return Err(invalid_field_err!("paletteCount", "palette count is 0"));
    }
    if palette_count > MAX_PALETTE_COUNT {
        return Err(invalid_field_err!("paletteCount", "palette count exceeds 127"));
    }

    let palette_byte_count = usize::from(palette_count) * 3;
    ensure_size!(ctx: "RlexPalette", in: src, size: palette_byte_count);

    let mut palette = Vec::with_capacity(usize::from(palette_count));
    for _ in 0..palette_count {
        let b = src.read_u8();
        let g = src.read_u8();
        let r = src.read_u8();
        palette.push([b, g, r]);
    }

    // floor(log2(paletteCount - 1)) + 1; the log2-floor of 0 is treated as 0,
    // so a single-entry palette still uses one stop-index bit.
    let stop_index_bits = bit_length(u32::from(palette_count.saturating_sub(1))).max(1);
    let suite_depth_bits = 8 - stop_index_bits;
    let stop_mask = mask(stop_index_bits);
    let depth_mask = mask(suite_depth_bits);

    let mut segments = Vec::new();
    while !src.is_empty() {
        ensure_size!(ctx: "RlexSegment", in: src, size: 2);
        let packed = src.read_u8();
        let stop_index = packed & stop_mask;
        let suite_depth = (packed >> stop_index_bits) & depth_mask;
        let Some(start_index) = stop_index.checked_sub(suite_depth) else {
            return Err(invalid_field_err!("suiteDepth", "suite depth exceeds stop index"));
        };

        let run_length = decode_run_length(&mut src)?;

        segments.push(RlexSegment {
            start_index,
            stop_index,
            run_length,
        });
    }

    Ok(RlexData { palette, segments })
}

/// Decode the bands layer with the correct `SHORT_VBAR_CACHE_MISS` layout.
pub(crate) fn decode_bands_layer(data: &[u8]) -> DecodeResult<Vec<Band<'_>>> {
    let mut bands = Vec::new();
    let mut src = ReadCursor::new(data);

    // Band header: 4 x u16 + 3 x u8 = 11 bytes.
    while src.len() >= 11 {
        bands.push(decode_single_band(&mut src)?);
    }

    Ok(bands)
}

fn decode_single_band<'a>(src: &mut ReadCursor<'a>) -> DecodeResult<Band<'a>> {
    ensure_size!(ctx: "ClearCodecBand", in: src, size: 11);

    let x_start = src.read_u16();
    let x_end = src.read_u16();
    let y_start = src.read_u16();
    let y_end = src.read_u16();
    let blue_bkg = src.read_u8();
    let green_bkg = src.read_u8();
    let red_bkg = src.read_u8();

    let height = y_end
        .checked_sub(y_start)
        .and_then(|h| h.checked_add(1))
        .ok_or_else(|| invalid_field_err!("yEnd", "yEnd < yStart"))?;
    if height > MAX_BAND_HEIGHT {
        return Err(invalid_field_err!("bandHeight", "band height exceeds 52"));
    }
    if x_end < x_start {
        return Err(invalid_field_err!("xEnd", "xEnd < xStart"));
    }

    let column_count = usize::from(x_end - x_start) + 1;
    let mut vbars = Vec::with_capacity(column_count);
    for _ in 0..column_count {
        vbars.push(decode_vbar(src)?);
    }

    Ok(Band {
        x_start,
        x_end,
        y_start,
        y_end,
        blue_bkg,
        green_bkg,
        red_bkg,
        vbars,
    })
}

fn decode_vbar<'a>(src: &mut ReadCursor<'a>) -> DecodeResult<VBar<'a>> {
    ensure_size!(ctx: "VBar", in: src, size: 2);
    let first_word = src.read_u16();

    // Bit 15 set: full V-bar cache hit (15-bit index).
    if first_word & 0x8000 != 0 {
        return Ok(VBar::CacheHit {
            index: first_word & 0x7FFF,
        });
    }

    // Bits 15:14 = 01: short V-bar cache hit (14-bit index + yOn byte).
    if first_word & 0x4000 != 0 {
        let index = first_word & 0x3FFF;
        ensure_size!(ctx: "ShortVBarCacheHit", in: src, size: 1);
        let y_on = src.read_u8();
        return Ok(VBar::ShortCacheHit { index, y_on });
    }

    // Bits 15:14 = 00: short V-bar cache miss.
    // Layout per the reference implementation: yOn = bits 7:0,
    // yOff = bits 13:8. Pixel count = yOff - yOn (rows of inline BGR data).
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "masked to 8 and 6 bits respectively"
    )]
    let (y_on, y_off) = ((first_word & 0xFF) as u8, ((first_word >> 8) & 0x3F) as u8);

    let Some(pixel_count) = y_off.checked_sub(y_on) else {
        return Err(invalid_field_err!(
            "shortVBarCacheMiss",
            "shortVBarYOff < shortVBarYOn"
        ));
    };

    let pixel_byte_count = usize::from(pixel_count) * 3;
    ensure_size!(ctx: "ShortVBarCacheMiss", in: src, size: pixel_byte_count);
    let pixel_data = src.read_slice(pixel_byte_count);

    Ok(VBar::ShortCacheMiss(ShortVBarCacheMiss {
        y_on,
        y_off_delta: pixel_count,
        pixel_data,
    }))
}

/// Variable-length run length: u8, escalating to u16 at 0xFF and to u32 at
/// 0xFFFF (each wider read replaces the previous value).
fn decode_run_length(src: &mut ReadCursor<'_>) -> DecodeResult<u32> {
    ensure_size!(ctx: "RlexRunLength", in: src, size: 1);
    let factor1 = src.read_u8();
    if factor1 < 0xFF {
        return Ok(u32::from(factor1));
    }

    ensure_size!(ctx: "RlexRunLength", in: src, size: 2);
    let factor2 = src.read_u16();
    if factor2 < 0xFFFF {
        return Ok(u32::from(factor2));
    }

    ensure_size!(ctx: "RlexRunLength", in: src, size: 4);
    Ok(src.read_u32())
}

fn mask(bits: u8) -> u8 {
    if bits >= 8 {
        0xFF
    } else {
        (1u8 << bits) - 1
    }
}

/// floor(log2(n)) + 1 for non-zero n; 0 for n = 0.
fn bit_length(n: u32) -> u8 {
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "bit length of u32 is at most 32"
    )]
    if n == 0 {
        0
    } else {
        (32 - n.leading_zeros()) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live-captured failing stream: 2x64 region, single black palette entry,
    /// segment bytes `00 7f` = run 127 of palette[0] + suite of 1 = 128 px.
    #[test]
    fn rlex_single_palette_uses_one_stop_bit() {
        let data = [0x01, 0x00, 0x00, 0x00, 0x00, 0x7F];
        let rlex = decode_rlex(&data).unwrap();
        assert_eq!(rlex.palette.len(), 1);
        assert_eq!(rlex.segments.len(), 1);
        let seg = rlex.segments[0];
        assert_eq!((seg.start_index, seg.stop_index, seg.run_length), (0, 0, 127));
        // run + suite = 127 + 1 = 128 pixels
    }

    /// Live-captured failing stream: 64x46, run length escalates to u16
    /// (0xFF marker then 0x0B7F = 2943) + suite of 1 = 2944 px.
    #[test]
    fn rlex_run_length_escalates_to_u16() {
        let data = [0x01, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x7F, 0x0B];
        let rlex = decode_rlex(&data).unwrap();
        assert_eq!(rlex.segments.len(), 1);
        assert_eq!(rlex.segments[0].run_length, 2943);
    }

    /// The short-vbar cache-miss header carries yOn in the low byte and yOff
    /// in bits 13:8.
    #[test]
    fn short_vbar_cache_miss_bit_layout() {
        // yOn = 2, yOff = 5 -> 3 pixel rows (9 bytes of BGR).
        let word: u16 = (5 << 8) | 2;
        let mut data = word.to_le_bytes().to_vec();
        data.extend_from_slice(&[10, 20, 30, 40, 50, 60, 70, 80, 90]);
        let mut src = ReadCursor::new(&data);
        match decode_vbar(&mut src).unwrap() {
            VBar::ShortCacheMiss(miss) => {
                assert_eq!(miss.y_on, 2);
                assert_eq!(miss.y_off_delta, 3);
                assert_eq!(miss.pixel_data.len(), 9);
            }
            other => panic!("expected ShortCacheMiss, got {other:?}"),
        }
    }
}
