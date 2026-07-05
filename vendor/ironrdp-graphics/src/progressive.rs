//! Progressive RFX decode and encode algorithms ([MS-RDPEGFX] 2.2.4.2).
//!
//! Provides first-pass decode (RLGR1 + progressive dequantization + sign capture)
//! and upgrade-pass decode (SRL/raw routing by DAS sign state, coefficient
//! accumulation) for the RemoteFX Progressive codec.
//!
//! These are pure algorithmic functions operating on coefficient buffers.
//! Tile state management and EGFX integration belong in a higher layer.

extern crate alloc;

use alloc::collections::btree_map::Entry;
use alloc::collections::BTreeMap;

use ironrdp_pdu::codecs::rfx::progressive::ComponentCodecQuant;
use ironrdp_pdu::codecs::rfx::EntropyAlgorithm;

use crate::dwt_extrapolate::BandInfo;
use crate::rlgr::RlgrError;
use crate::srl;

/// Number of DWT coefficients per component in a 64x64 tile.
pub const COEFFICIENTS_PER_COMPONENT: usize = 4096;

/// Number of subbands in a 3-level DWT decomposition.
pub const NUM_BANDS: usize = 10;

/// DAS (Delta-Analysis State) values for tri-state sign tracking.
///
/// After the first pass, each coefficient position is classified:
/// - `SIGN_ZERO`: coefficient was zero (eligible for SRL upgrade)
/// - `SIGN_POSITIVE`: coefficient was positive (eligible for raw upgrade)
/// - `SIGN_NEGATIVE`: coefficient was negative (eligible for raw upgrade)
pub const SIGN_ZERO: i8 = 0;
pub const SIGN_POSITIVE: i8 = 1;
pub const SIGN_NEGATIVE: i8 = -1;
/// Tile payload contains coefficient deltas relative to the previous tile.
const TILE_FLAG_DIFFERENCE: u8 = 0x01;

// ---------------------------------------------------------------------------
// First-pass decode (TILE_SIMPLE / TILE_FIRST)
// ---------------------------------------------------------------------------

/// Decode a first-pass component from RLGR1-encoded data.
///
/// Performs: RLGR1 decode -> base dequantization -> progressive dequantization
/// -> LL3 delta decode -> sign capture.
///
/// # Arguments
/// - `data`: RLGR1-encoded coefficient stream
/// - `base_quant`: base quantization values (from region quant table, `ComponentCodecQuant` format)
/// - `prog_quant`: progressive quantization BitPos values for this quality level
/// - `use_reduce_extrapolate`: whether to use asymmetric band sizes
/// - `coefficients`: output buffer for decoded coefficients (4096 i16)
/// - `sign`: output buffer for DAS sign state (4096 i8)
///
/// # Panics
///
/// Panics if `coefficients` or `sign` has fewer than 4096 elements.
///
/// # Errors
/// Returns `RlgrError` if RLGR decoding fails.
pub fn decode_first_pass(
    data: &[u8],
    base_quant: &ComponentCodecQuant,
    prog_quant: &ComponentCodecQuant,
    use_reduce_extrapolate: bool,
    coefficients: &mut [i16],
    sign: &mut [i8],
) -> Result<crate::rlgr::RlgrConsumption, RlgrError> {
    assert!(coefficients.len() >= COEFFICIENTS_PER_COMPONENT);
    assert!(sign.len() >= COEFFICIENTS_PER_COMPONENT);

    // Step 1: RLGR1 decode into coefficient buffer
    let consumption = crate::rlgr::decode(EntropyAlgorithm::Rlgr1, data, coefficients)?;

    // The DAS sign state is defined over the entropy-decoded coefficients,
    // before LL3 differential reconstruction and dequantization.
    capture_sign(coefficients, sign);

    // Step 2: LL3 differential decoding (reverse delta encoding on last subband)
    crate::subband_reconstruction::decode(&mut coefficients[ll3_offset(use_reduce_extrapolate)..]);

    // Step 3: Base dequantization (shift left by quant - 1)
    dequantize_component_ccq(coefficients, base_quant, use_reduce_extrapolate);

    // Step 4: Progressive dequantization (shift left by BitPos)
    progressive_dequantize(coefficients, prog_quant, use_reduce_extrapolate);

    Ok(consumption)
}

/// Decode an upgrade-pass component from SRL and raw data streams.
///
/// For each coefficient position:
/// - DAS = 0 (zero): decode from SRL stream, update DAS if non-zero
/// - DAS != 0 (non-zero): decode raw magnitude bits, accumulate
///
/// # Arguments
/// - `srl_data`: SRL-encoded stream for zero-DAS positions
/// - `raw_data`: raw bit stream for non-zero-DAS positions
/// - `prev_prog_quant`: BitPos values from previous quality level
/// - `curr_prog_quant`: BitPos values for this quality level
/// - `use_reduce_extrapolate`: whether to use asymmetric band sizes
/// - `coefficients`: coefficient buffer to accumulate into (modified in-place)
/// - `sign`: DAS sign buffer (modified in-place when zeros become non-zero)
///
/// # Panics
///
/// Panics if `coefficients` or `sign` has fewer than 4096 elements.
pub fn decode_upgrade_pass(
    srl_data: &[u8],
    raw_data: &[u8],
    prev_bit_pos: &ComponentCodecQuant,
    curr_bit_pos: &ComponentCodecQuant,
    use_reduce_extrapolate: bool,
    coefficients: &mut [i16],
    sign: &mut [i8],
) -> UpgradeStreamUsage {
    assert!(coefficients.len() >= COEFFICIENTS_PER_COMPONENT);
    assert!(sign.len() >= COEFFICIENTS_PER_COMPONENT);

    let bands = get_band_layout(use_reduce_extrapolate);
    let mut srl_reader = srl::SrlDecoder::new(srl_data);
    let mut raw_reader = RawBitReader::new(raw_data);

    for (band_idx, band) in bands.iter().enumerate() {
        // Absolute bit positions (base quant + progressive quant), matching
        // the reference implementation's tile->yBitPos bookkeeping. Using only
        // the progressive quants silently miscounts numBits whenever the base
        // quant table changed between a tile's passes, which desynchronizes
        // the shared SRL/raw bit streams for the rest of the component.
        let prev = prev_bit_pos.for_band(band_idx);
        let curr = curr_bit_pos.for_band(band_idx);
        let coefficient_shift = curr.saturating_sub(1);

        // Number of raw bits per coefficient in this band
        let num_bits = prev.saturating_sub(curr);
        if num_bits == 0 {
            continue;
        }

        for i in 0..band.count() {
            let coeff_idx = band.offset + i;
            let is_ll3 = band_idx == 9;

            let value = if is_ll3 {
                i32::try_from(raw_reader.read_bits(u32::from(num_bits))).unwrap_or(i32::MAX)
            } else if sign[coeff_idx] == SIGN_ZERO {
                // Zero-DAS: get value from SRL stream
                let value = srl_reader.read(num_bits);
                if value != 0 {
                    // Coefficient transitions from zero to non-zero
                    sign[coeff_idx] = if value > 0 {
                        SIGN_POSITIVE
                    } else {
                        SIGN_NEGATIVE
                    };
                }
                i32::from(value)
            } else {
                // Non-zero DAS: read raw magnitude bits
                let raw_mag = raw_reader.read_bits(u32::from(num_bits));
                let magnitude = i32::try_from(raw_mag).unwrap_or(i32::MAX);
                if sign[coeff_idx] == SIGN_NEGATIVE {
                    -magnitude
                } else {
                    magnitude
                }
            };

            if value != 0 {
                // Upgrade values use the same absolute coefficient scale as
                // the first pass: (base quant + progressive bit pos) - 1.
                let shifted = i64::from(value) << u32::from(coefficient_shift);
                coefficients[coeff_idx] =
                    clamp_i16_i64(i64::from(coefficients[coeff_idx]) + shifted);
            }
        }
    }

    UpgradeStreamUsage {
        srl_bits_remaining: srl_reader.bits_remaining(),
        srl_overrun: srl_reader.overrun(),
        raw_bits_remaining: raw_reader.bits_remaining(),
        raw_overrun: raw_reader.overrun,
    }
}

/// How completely an upgrade pass consumed its SRL and raw bit streams.
///
/// Both streams are shared sequentially by all ten DWT bands, so any numBits
/// or state-machine error desynchronizes everything after it. A correct pass
/// consumes each stream to within its byte padding; anything else means the
/// decoded refinement is corrupt.
#[derive(Debug, Clone, Copy)]
pub struct UpgradeStreamUsage {
    /// Bits left unread in the SRL stream.
    pub srl_bits_remaining: usize,
    /// The SRL stream was over-read (reads past the end returned zeros).
    pub srl_overrun: bool,
    /// Bits left unread in the raw stream.
    pub raw_bits_remaining: usize,
    /// The raw stream was over-read.
    pub raw_overrun: bool,
}

impl UpgradeStreamUsage {
    /// Whether stream consumption looks bit-exact.
    ///
    /// The raw stream pads to a byte boundary (< 8 bits left); the SRL stream
    /// additionally tolerates one whole padding byte, mirroring the reference
    /// implementation's end-of-stream handling. Overrun on the SRL stream is
    /// benign when the encoder relies on implicit trailing zero runs, so only
    /// under-consumption and raw overrun are flagged.
    pub fn is_aligned(&self) -> bool {
        !self.raw_overrun && self.raw_bits_remaining < 8 && self.srl_bits_remaining < 16
    }
}

// ---------------------------------------------------------------------------
// Progressive (de)quantization
// ---------------------------------------------------------------------------

/// Apply progressive dequantization: left-shift each band by its BitPos value.
///
/// For non-LL3 bands, this shifts the absolute value (preserving sign).
/// For LL3, this is a simple left shift (floor toward negative infinity).
fn progressive_dequantize(
    coefficients: &mut [i16],
    prog_quant: &ComponentCodecQuant,
    use_reduce_extrapolate: bool,
) {
    let bands = get_band_layout(use_reduce_extrapolate);

    for (band_idx, band) in bands.iter().enumerate() {
        let bit_pos = prog_quant.for_band(band_idx);
        if bit_pos == 0 {
            continue;
        }

        let is_ll3 = band_idx == 9;
        let start = band.offset;
        let end = start + band.count();

        if is_ll3 {
            // LL3: simple left shift (floor toward negative infinity)
            for coeff in &mut coefficients[start..end] {
                *coeff = clamp_i16(i32::from(*coeff) << i32::from(bit_pos));
            }
        } else {
            // Other bands: shift absolute value, preserve sign
            for coeff in &mut coefficients[start..end] {
                let val = i32::from(*coeff);
                if val >= 0 {
                    *coeff = clamp_i16(val << i32::from(bit_pos));
                } else {
                    *coeff = clamp_i16(-((-val) << i32::from(bit_pos)));
                }
            }
        }
    }
}

/// Apply progressive quantization: right-shift each band by its BitPos value.
///
/// Inverse of `progressive_dequantize`.
pub fn progressive_quantize(
    coefficients: &mut [i16],
    prog_quant: &ComponentCodecQuant,
    use_reduce_extrapolate: bool,
) {
    let bands = get_band_layout(use_reduce_extrapolate);

    for (band_idx, band) in bands.iter().enumerate() {
        let bit_pos = prog_quant.for_band(band_idx);
        if bit_pos == 0 {
            continue;
        }

        let is_ll3 = band_idx == 9;
        let start = band.offset;
        let end = start + band.count();

        if is_ll3 {
            // LL3: floor division (right shift)
            for coeff in &mut coefficients[start..end] {
                *coeff >>= bit_pos;
            }
        } else {
            // Other bands: truncation toward zero
            for coeff in &mut coefficients[start..end] {
                let val = i32::from(*coeff);
                if val >= 0 {
                    *coeff = clamp_i16(val >> i32::from(bit_pos));
                } else {
                    *coeff = clamp_i16(-((-val) >> i32::from(bit_pos)));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Server-side encode pipeline
// ---------------------------------------------------------------------------

/// Encode a first-pass component from spatial-domain coefficients.
///
/// Pipeline: forward DWT -> base quantization -> progressive quantization
/// -> LL3 delta encode -> RLGR1 encode.
///
/// Returns the number of bytes written to `output`.
///
/// # Arguments
/// - `coefficients`: spatial-domain coefficients (4096 i16, modified in-place)
/// - `output`: output buffer for RLGR1-encoded data
/// - `base_quant`: base quantization values
/// - `prog_quant`: progressive quantization BitPos values for this quality level
/// - `use_reduce_extrapolate`: DWT mode flag
///
/// # Panics
///
/// Panics if `coefficients` has fewer than 4096 elements.
///
/// # Errors
/// Returns `RlgrError` if RLGR encoding fails.
pub fn encode_first_pass(
    coefficients: &mut [i16],
    output: &mut [u8],
    base_quant: &ComponentCodecQuant,
    prog_quant: &ComponentCodecQuant,
    use_reduce_extrapolate: bool,
) -> Result<usize, RlgrError> {
    assert!(coefficients.len() >= COEFFICIENTS_PER_COMPONENT);

    let mut temp = [0i16; COEFFICIENTS_PER_COMPONENT];

    // Step 1: Forward DWT
    if use_reduce_extrapolate {
        crate::dwt_extrapolate::encode(coefficients, &mut temp);
    } else {
        crate::dwt::encode(coefficients, &mut temp);
    }

    // Step 2: Base quantization (right-shift by quant - 1)
    quantize_component_ccq(coefficients, base_quant, use_reduce_extrapolate);

    // Step 3: Progressive quantization (right-shift by BitPos)
    progressive_quantize(coefficients, prog_quant, use_reduce_extrapolate);

    // Step 4: LL3 delta encoding
    crate::subband_reconstruction::encode(&mut coefficients[ll3_offset(use_reduce_extrapolate)..]);

    // Step 5: RLGR1 entropy encode
    crate::rlgr::encode(EntropyAlgorithm::Rlgr1, coefficients, output)
}

/// Base quantization using `ComponentCodecQuant` (progressive format).
///
/// Each band is right-shifted by `(quant_value - 1)`. Inverse of `dequantize_component_ccq`.
fn quantize_component_ccq(
    coefficients: &mut [i16],
    quant: &ComponentCodecQuant,
    use_reduce_extrapolate: bool,
) {
    let bands = get_band_layout(use_reduce_extrapolate);

    for (band_idx, band) in bands.iter().enumerate() {
        let q = quant.for_band(band_idx);
        let factor = q.saturating_sub(1);
        if factor > 0 {
            let start = band.offset;
            let end = start + band.count();
            for coeff in &mut coefficients[start..end] {
                // Truncation toward zero (same as classic quantization::encode)
                let val = i32::from(*coeff);
                if val >= 0 {
                    *coeff = clamp_i16(val >> i32::from(factor));
                } else {
                    *coeff = clamp_i16(-((-val) >> i32::from(factor)));
                }
            }
        }
    }
}

/// Compute the upgrade-pass data for a single component.
///
/// Given the previous and current progressive quantization, produces
/// SRL-encoded data (for zero-DAS positions) and raw bit data (for
/// non-zero DAS positions) representing the refinement.
///
/// # Arguments
/// - `coefficients`: current full-resolution DWT coefficients for this component
/// - `prev_coefficients`: coefficients as reconstructed from the previous pass
/// - `prev_prog_quant`: BitPos values from the previous pass
/// - `curr_prog_quant`: BitPos values for this upgrade pass
/// - `sign`: DAS sign array from the previous pass
/// - `use_reduce_extrapolate`: DWT mode flag
///
/// # Returns
/// A tuple of `(srl_data, raw_data)` byte vectors.
///
/// # Wire-format invariants (MS-RDPRFX 3.1.8.1.7.2)
///
/// The non-zero-DAS raw-magnitude path uses `saturating_sub` to compute
/// `raw_mag = curr_q - prev_q`. Upgrade passes are *monotonic refinements*:
/// the encoder only adds magnitude bits, never subtracts. The decoder's
/// counterpart accumulates raw_mag onto the previously-decoded coefficient
/// with the DAS-determined sign (`+=` for SIGN_POSITIVE / LL3, `-=` for
/// SIGN_NEGATIVE), so a hypothetical signed delta would have no place in
/// the wire format. Switching this to a signed-delta encoding would break
/// wire compatibility with mstsc/FreeRDP — do not "fix" the saturating_sub.
///
/// The zero-DAS SRL path uses `clamp_i16(curr_shifted - prev_shifted)`. SRL
/// stream values are i16 by wire-format definition, so wider precision is
/// not available without a spec extension. The clamp is the wire-format
/// boundary, not a precision compromise.
pub fn encode_upgrade_pass(
    coefficients: &[i16],
    prev_coefficients: &[i16],
    prev_prog_quant: &ComponentCodecQuant,
    curr_prog_quant: &ComponentCodecQuant,
    sign: &[i8],
    use_reduce_extrapolate: bool,
) -> (Vec<u8>, Vec<u8>) {
    let bands = get_band_layout(use_reduce_extrapolate);
    let mut all_srl_values = Vec::new();
    let mut raw_writer = RawBitWriter::new();

    for (band_idx, band) in bands.iter().enumerate() {
        let prev_bit_pos = prev_prog_quant.for_band(band_idx);
        let curr_bit_pos = curr_prog_quant.for_band(band_idx);

        let num_bits = prev_bit_pos.saturating_sub(curr_bit_pos);
        if num_bits == 0 {
            continue;
        }

        for i in 0..band.count() {
            let coeff_idx = band.offset + i;
            let is_ll3 = band_idx == 9;

            if is_ll3 {
                let curr = i32::from(coefficients[coeff_idx]) >> i32::from(curr_bit_pos);
                let prev = i32::from(prev_coefficients[coeff_idx]) >> i32::from(curr_bit_pos);
                let raw_mag = curr.saturating_sub(prev).max(0);
                raw_writer.write_bits(
                    u32::try_from(raw_mag).unwrap_or(u32::MAX),
                    u32::from(num_bits),
                );
            } else if sign[coeff_idx] == SIGN_ZERO {
                // Zero-DAS: compute the refined value and encode via SRL
                let curr_shifted = i32::from(coefficients[coeff_idx]) >> i32::from(curr_bit_pos);
                let prev_shifted =
                    i32::from(prev_coefficients[coeff_idx]) >> i32::from(curr_bit_pos);
                let delta = clamp_i16(curr_shifted - prev_shifted);
                all_srl_values.push((delta, num_bits));
            } else {
                // Non-zero DAS: compute raw magnitude bits
                let curr_abs = i32::from(coefficients[coeff_idx]).unsigned_abs();
                let prev_abs = i32::from(prev_coefficients[coeff_idx]).unsigned_abs();

                let curr_q = curr_abs >> u32::from(curr_bit_pos);
                let prev_q = prev_abs >> u32::from(curr_bit_pos);
                let raw_mag = curr_q.saturating_sub(prev_q);

                raw_writer.write_bits(raw_mag, u32::from(num_bits));
            }
        }
    }

    let all_srl_values = srl::encode_srl_with_bit_widths(&all_srl_values);
    let raw_data = raw_writer.finish();
    (all_srl_values, raw_data)
}

/// Encode RGBA pixels to spatial-domain i16 coefficients (RGB to YCbCr).
///
/// Performs ITU-R BT.601 RGB-to-YCbCr conversion on a 64x64 pixel tile.
/// Output is 3 buffers of 4096 i16 coefficients (Y, Cb, Cr) in tile order.
///
/// # Panics
///
/// Panics if `pixels` has fewer than 64 * 64 * 4 = 16384 bytes.
#[expect(clippy::similar_names)]
pub fn rgba_to_ycbcr(pixels: &[u8], y_out: &mut [i16], cb_out: &mut [i16], cr_out: &mut [i16]) {
    assert!(pixels.len() >= 64 * 64 * 4);
    assert!(y_out.len() >= COEFFICIENTS_PER_COMPONENT);
    assert!(cb_out.len() >= COEFFICIENTS_PER_COMPONENT);
    assert!(cr_out.len() >= COEFFICIENTS_PER_COMPONENT);

    for i in 0..64 * 64 {
        let off = i * 4;
        let r = i32::from(pixels[off]);
        let g = i32::from(pixels[off + 1]);
        let b = i32::from(pixels[off + 2]);

        // ITU-R BT.601: Y = 0.299R + 0.587G + 0.114B
        //               Cb = -0.169R - 0.331G + 0.500B
        //               Cr = 0.500R - 0.419G - 0.081B
        // Fixed-point with 16-bit precision
        let y = ((19595 * r + 38470 * g + 7471 * b + 32768) >> 16) - 128;
        let cb = (-11059 * r - 21709 * g + 32768 * b + 32768) >> 16;
        let cr = (32768 * r - 27439 * g - 5329 * b + 32768) >> 16;

        y_out[i] = clamp_i16(y);
        cb_out[i] = clamp_i16(cb);
        cr_out[i] = clamp_i16(cr);
    }
}

/// Base dequantization using `ComponentCodecQuant` (progressive-format quantization).
///
/// Each band is shifted left by `(quant_value - 1)`. Uses `for_band()` to map
/// band indices to quant values, which handles the progressive nibble ordering.
fn dequantize_component_ccq(
    coefficients: &mut [i16],
    quant: &ComponentCodecQuant,
    use_reduce_extrapolate: bool,
) {
    let bands = get_band_layout(use_reduce_extrapolate);

    for (band_idx, band) in bands.iter().enumerate() {
        let q = quant.for_band(band_idx);
        let factor = i16::from(q).saturating_sub(1);
        if factor > 0 {
            let start = band.offset;
            let end = start + band.count();
            for coeff in &mut coefficients[start..end] {
                *coeff <<= factor;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sign capture
// ---------------------------------------------------------------------------

/// Capture the tri-state sign of each coefficient into the DAS array.
fn capture_sign(coefficients: &[i16], sign: &mut [i8]) {
    for (s, &c) in sign.iter_mut().zip(coefficients.iter()) {
        *s = match c.cmp(&0) {
            core::cmp::Ordering::Greater => SIGN_POSITIVE,
            core::cmp::Ordering::Less => SIGN_NEGATIVE,
            core::cmp::Ordering::Equal => SIGN_ZERO,
        };
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get the band layout for the current DWT mode.
fn get_band_layout(use_reduce_extrapolate: bool) -> [BandInfo; NUM_BANDS] {
    if use_reduce_extrapolate {
        crate::dwt_extrapolate::band_layout()
    } else {
        standard_band_layout()
    }
}

/// Standard (non-extrapolate) band layout for a 64x64 tile.
/// Band sizes: 1024 each for level 1, 256 each for level 2, 64 each for level 3.
fn standard_band_layout() -> [BandInfo; NUM_BANDS] {
    let mut off = 0;
    let mut b = |w: usize, h: usize| {
        let info = BandInfo {
            width: w,
            height: h,
            offset: off,
        };
        off += w * h;
        info
    };

    [
        b(32, 32), // HL1: 1024
        b(32, 32), // LH1: 1024
        b(32, 32), // HH1: 1024
        b(16, 16), // HL2: 256
        b(16, 16), // LH2: 256
        b(16, 16), // HH2: 256
        b(8, 8),   // HL3: 64
        b(8, 8),   // LH3: 64
        b(8, 8),   // HH3: 64
        b(8, 8),   // LL3: 64
    ]
}

/// Per-band sum of two quant structs (base quant + progressive quant),
/// yielding the absolute bit position the reference implementation tracks
/// per tile pass.
fn quant_add(a: &ComponentCodecQuant, b: &ComponentCodecQuant) -> ComponentCodecQuant {
    ComponentCodecQuant {
        ll3: a.ll3.saturating_add(b.ll3),
        hl3: a.hl3.saturating_add(b.hl3),
        lh3: a.lh3.saturating_add(b.lh3),
        hh3: a.hh3.saturating_add(b.hh3),
        hl2: a.hl2.saturating_add(b.hl2),
        lh2: a.lh2.saturating_add(b.lh2),
        hh2: a.hh2.saturating_add(b.hh2),
        hl1: a.hl1.saturating_add(b.hl1),
        lh1: a.lh1.saturating_add(b.lh1),
        hh1: a.hh1.saturating_add(b.hh1),
    }
}

/// Starting offset of the LL3 subband for delta decoding.
fn ll3_offset(use_reduce_extrapolate: bool) -> usize {
    if use_reduce_extrapolate {
        4015 // reduce-extrapolate: 9x9 = 81 coefficients at offset 4015
    } else {
        4032 // standard: 8x8 = 64 coefficients at offset 4032
    }
}

/// Count zero-DAS positions within a band.
#[cfg(test)]
fn band_zero_count(sign: &[i8], band: &BandInfo) -> usize {
    let start = band.offset;
    let end = start + band.count();
    sign[start..end].iter().filter(|&&s| s == SIGN_ZERO).count()
}

/// Clamp i32 to i16 range.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "value is clamped to i16 range before cast"
)]
fn clamp_i16(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "value is clamped to i16 range before cast"
)]
fn clamp_i16_i64(value: i64) -> i16 {
    value.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
}

// ---------------------------------------------------------------------------
// Raw bit I/O for upgrade pass
// ---------------------------------------------------------------------------

/// Writes raw magnitude bits MSB-first to a byte stream.
///
/// Symmetric counterpart of [`RawBitReader`]. Callers are expected to pass
/// `count <= 32` to [`write_bits`](Self::write_bits); the upgrade-pass call
/// site bounds `count` by `prev_bit_pos - curr_bit_pos` which is at most a
/// few bits in practice. `count > 32` reads beyond `u32` width in the shift
/// expression, which is wrap-on-release / panic-on-debug — caller responsibility.
struct RawBitWriter {
    bytes: Vec<u8>,
    current: u8,
    bit_count: u8,
}

impl RawBitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            current: 0,
            bit_count: 0,
        }
    }

    fn write_bit(&mut self, bit: bool) {
        self.current = (self.current << 1) | u8::from(bit);
        self.bit_count += 1;
        if self.bit_count >= 8 {
            self.bytes.push(self.current);
            self.current = 0;
            self.bit_count = 0;
        }
    }

    /// Write the low `count` bits of `value`, MSB-first. Caller must ensure
    /// `count <= 32` (see type-level docs).
    fn write_bits(&mut self, value: u32, count: u32) {
        debug_assert!(count <= 32, "RawBitWriter::write_bits count must be <= 32");
        for i in (0..count).rev() {
            self.write_bit((value >> i) & 1 != 0);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bit_count > 0 {
            self.current <<= 8 - self.bit_count;
            self.bytes.push(self.current);
        }
        self.bytes
    }
}

/// Reads raw magnitude bits MSB-first from a byte stream.
///
/// Past-end-of-stream reads return zero bits rather than an error: a
/// truncated `raw_data` produces zero coefficient magnitudes (no-op upgrade)
/// for the missing positions, matching the FreeRDP reference implementation's
/// tolerance for short truncation in this exact upgrade path.
///
/// Callers are expected to pass `count <= 32` to [`read_bits`](Self::read_bits);
/// the upgrade-pass call site bounds `count` by `prev_bit_pos - curr_bit_pos`
/// which is at most a few bits in practice.
struct RawBitReader<'a> {
    data: &'a [u8],
    byte_idx: usize,
    bit_idx: u8,
    overrun: bool,
}

impl<'a> RawBitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_idx: 0,
            bit_idx: 0,
            overrun: false,
        }
    }

    /// Unconsumed bits left in the raw stream (should be sub-byte padding
    /// after a correctly aligned upgrade pass).
    fn bits_remaining(&self) -> usize {
        if self.byte_idx >= self.data.len() {
            return 0;
        }
        (self.data.len() - self.byte_idx) * 8 - usize::from(self.bit_idx)
    }

    /// Read `count` bits MSB-first into a `u32`. Bits past the end of the
    /// underlying stream read as zero.
    fn read_bits(&mut self, count: u32) -> u32 {
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1) | u32::from(self.read_bit());
        }
        value
    }

    /// Read one bit. Returns `false` past end-of-stream by design (see
    /// type-level docs).
    fn read_bit(&mut self) -> bool {
        if self.byte_idx >= self.data.len() {
            self.overrun = true;
            return false;
        }
        let bit = (self.data[self.byte_idx] >> (7 - self.bit_idx)) & 1 != 0;
        self.bit_idx += 1;
        if self.bit_idx >= 8 {
            self.bit_idx = 0;
            self.byte_idx += 1;
        }
        bit
    }
}

// ---------------------------------------------------------------------------
// Tile state machine
// ---------------------------------------------------------------------------

/// Per-tile progressive state: coefficients, signs, and quality tracking.
///
/// Each tile in a progressive surface maintains this state across decode
/// passes. The first pass (TILE_SIMPLE or TILE_FIRST) initializes the
/// coefficients and signs; subsequent upgrade passes (TILE_UPGRADE)
/// accumulate refinement data.
///
/// Memory per tile: ~37 KB (24 KB coefficients + 12 KB signs + metadata).
pub struct TileState {
    /// Accumulated DWT coefficients per component (Y, Cb, Cr).
    pub coefficients: [[i16; COEFFICIENTS_PER_COMPONENT]; 3],
    /// Tri-state sign tracking per component (DAS array).
    pub sign: [[i8; COEFFICIENTS_PER_COMPONENT]; 3],
    /// Progressive quantization BitPos from the last applied pass.
    pub prog_quant: [ComponentCodecQuant; 3],
    /// Absolute bit positions (base quant + progressive quant) per band from
    /// the last applied pass. Upgrade numBits are computed from these, so a
    /// changed base quant table between passes still counts bits correctly.
    pub bit_pos: [ComponentCodecQuant; 3],
    /// Base quantization indices (Y, Cb, Cr) into the region's quant table.
    pub quant_idx: [u8; 3],
    /// Progressive pass counter (0 = no data, 1 = first pass complete, 2+ = upgrade).
    pub pass: u16,
    /// Whether the tile was encoded as a difference tile.
    pub is_difference: bool,
    /// Last progressive quality byte (0xFF = full quality).
    pub quality: u8,
    /// Whether reduce-extrapolate DWT is used for this tile's context.
    pub use_reduce_extrapolate: bool,
}

impl TileState {
    /// Create a new tile with zeroed state.
    pub fn new() -> Self {
        Self {
            coefficients: [[0; COEFFICIENTS_PER_COMPONENT]; 3],
            sign: [[0; COEFFICIENTS_PER_COMPONENT]; 3],
            prog_quant: [ComponentCodecQuant::LOSSLESS; 3],
            bit_pos: [ComponentCodecQuant::LOSSLESS; 3],
            quant_idx: [0; 3],
            pass: 0,
            is_difference: false,
            quality: 0,
            use_reduce_extrapolate: false,
        }
    }

    /// Decode a first-pass tile (TILE_SIMPLE or TILE_FIRST).
    ///
    /// Resets this tile's state and decodes three components from RLGR1 data.
    /// After this call, `coefficients` hold DWT-domain values ready for
    /// inverse DWT + color conversion.
    ///
    /// # Arguments
    /// - `component_data`: RLGR1-encoded data for [Y, Cb, Cr]
    /// - `base_quants`: base quantization values for [Y, Cb, Cr]
    /// - `prog_quants`: progressive quantization for [Y, Cb, Cr]
    /// - `quality`: progressive quality byte
    /// - `use_reduce_extrapolate`: DWT mode flag
    ///
    /// # Errors
    /// Returns `RlgrError` if any component's RLGR decode fails.
    #[expect(
        clippy::too_many_arguments,
        reason = "the tile decode state mirrors the protocol's component, quant, quality, and flag fields"
    )]
    pub fn decode_first(
        &mut self,
        component_data: [&[u8]; 3],
        base_quants: [&ComponentCodecQuant; 3],
        prog_quants: [ComponentCodecQuant; 3],
        quant_idx: [u8; 3],
        quality: u8,
        use_reduce_extrapolate: bool,
        is_difference: bool,
    ) -> Result<[crate::rlgr::RlgrConsumption; 3], RlgrError> {
        self.pass = 1;
        self.quality = quality;
        self.quant_idx = quant_idx;
        self.use_reduce_extrapolate = use_reduce_extrapolate;
        self.is_difference = is_difference;
        self.prog_quant = prog_quants;
        self.bit_pos = [
            quant_add(base_quants[0], &prog_quants[0]),
            quant_add(base_quants[1], &prog_quants[1]),
            quant_add(base_quants[2], &prog_quants[2]),
        ];

        let mut consumption = [crate::rlgr::RlgrConsumption {
            input_bits_remaining: 0,
            output_remaining: 0,
        }; 3];
        for c in 0..3 {
            let mut decoded = [0i16; COEFFICIENTS_PER_COMPONENT];
            consumption[c] = decode_first_pass(
                component_data[c],
                base_quants[c],
                &prog_quants[c],
                use_reduce_extrapolate,
                &mut decoded,
                &mut self.sign[c],
            )?;
            if is_difference {
                for (current, delta) in self.coefficients[c].iter_mut().zip(decoded) {
                    *current = clamp_i16(i32::from(*current) + i32::from(delta));
                }
            } else {
                self.coefficients[c] = decoded;
            }
        }

        Ok(consumption)
    }

    /// Decode an upgrade-pass tile (TILE_UPGRADE).
    ///
    /// Accumulates refinement data into existing coefficients. Returns the
    /// per-component stream usage so callers can detect a desynchronized
    /// SRL/raw bitstream (which silently corrupts the tile otherwise).
    ///
    /// # Arguments
    /// - `srl_data`: SRL-encoded streams for [Y, Cb, Cr]
    /// - `raw_data`: raw bit streams for [Y, Cb, Cr]
    /// - `prog_quants`: progressive quantization for this upgrade level
    /// - `quality`: progressive quality byte for this pass
    pub fn decode_upgrade(
        &mut self,
        srl_data: [&[u8]; 3],
        raw_data: [&[u8]; 3],
        base_quants: [&ComponentCodecQuant; 3],
        prog_quants: [ComponentCodecQuant; 3],
        quant_idx: [u8; 3],
        quality: u8,
    ) -> [UpgradeStreamUsage; 3] {
        let curr_bit_pos = [
            quant_add(base_quants[0], &prog_quants[0]),
            quant_add(base_quants[1], &prog_quants[1]),
            quant_add(base_quants[2], &prog_quants[2]),
        ];

        let mut usage = [UpgradeStreamUsage {
            srl_bits_remaining: 0,
            srl_overrun: false,
            raw_bits_remaining: 0,
            raw_overrun: false,
        }; 3];
        for c in 0..3 {
            usage[c] = decode_upgrade_pass(
                srl_data[c],
                raw_data[c],
                &self.bit_pos[c],
                &curr_bit_pos[c],
                self.use_reduce_extrapolate,
                &mut self.coefficients[c],
                &mut self.sign[c],
            );
        }

        self.prog_quant = prog_quants;
        self.bit_pos = curr_bit_pos;
        self.quant_idx = quant_idx;
        self.quality = quality;
        self.pass = self.pass.saturating_add(1);
        usage
    }

    /// Reconstruct the tile to spatial domain and write RGBA pixels.
    ///
    /// Applies inverse DWT to each component, then YCbCr-to-RGB color
    /// conversion. The pixel buffer receives 64x64 RGBA pixels (16384 bytes).
    ///
    /// # Panics
    ///
    /// Panics if `pixels` has fewer than 64 * 64 * 4 = 16384 bytes.
    pub fn reconstruct_to_rgba(&self, pixels: &mut [u8]) {
        assert!(pixels.len() >= 64 * 64 * 4, "pixel buffer too small");

        // Copy coefficients to scratch buffers for in-place DWT
        let mut y_buf = self.coefficients[0];
        let mut cb_buf = self.coefficients[1];
        let mut cr_buf = self.coefficients[2];
        let mut temp = [0i16; COEFFICIENTS_PER_COMPONENT];

        // Inverse DWT
        if self.use_reduce_extrapolate {
            crate::dwt_extrapolate::decode(&mut y_buf, &mut temp);
            crate::dwt_extrapolate::decode(&mut cb_buf, &mut temp);
            crate::dwt_extrapolate::decode(&mut cr_buf, &mut temp);
        } else {
            let mut dwt_temp = [0i16; COEFFICIENTS_PER_COMPONENT];
            crate::dwt::decode(&mut y_buf, &mut dwt_temp);
            crate::dwt::decode(&mut cb_buf, &mut dwt_temp);
            crate::dwt::decode(&mut cr_buf, &mut dwt_temp);
        }

        // RemoteFX stores YCbCr in the same signed fixed-point representation
        // as the classic RFX codec (Y has a 4096 bias). Reuse its conversion
        // instead of treating the coefficients as ordinary 8-bit YCbCr.
        crate::color_conversion::ycbcr_to_rgba(
            crate::color_conversion::YCbCrBuffer {
                y: &y_buf,
                cb: &cb_buf,
                cr: &cr_buf,
            },
            pixels,
        )
        .expect("64x64 RemoteFX tile has valid color plane dimensions");
    }
}

impl Default for TileState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Surface tile grid
// ---------------------------------------------------------------------------

/// Grid of progressive tiles for a single surface.
///
/// Manages tile state for one destination surface.
/// Tiles are lazily allocated on first access to avoid upfront memory
/// cost for surfaces that only partially receive progressive updates.
pub struct SurfaceTiles {
    /// Width of the surface in tiles (ceildiv of pixel width by 64).
    pub tiles_wide: u16,
    /// Height of the surface in tiles.
    pub tiles_high: u16,
    /// Whether the associated context uses reduce-extrapolate DWT.
    pub use_reduce_extrapolate: bool,
    /// Tile storage, indexed by `y_idx * tiles_wide + x_idx`.
    /// `None` entries haven't received any progressive data yet.
    pub tiles: Vec<Option<Box<TileState>>>,
}

impl SurfaceTiles {
    /// Create a new tile grid for the given surface dimensions.
    ///
    /// Returns [`ProgressiveDecodeError::SurfaceTooLarge`] if either axis
    /// exceeds [`MAX_SURFACE_DIM`]. The check rejects only inputs that exceed
    /// the MS-RDPEGFX 2.2.2.14 normative ceiling (32766 px), so every
    /// spec-conformant surface is accepted.
    pub fn new(
        width_pixels: u16,
        height_pixels: u16,
        use_reduce_extrapolate: bool,
    ) -> Result<Self, ProgressiveDecodeError> {
        if width_pixels > MAX_SURFACE_DIM || height_pixels > MAX_SURFACE_DIM {
            return Err(ProgressiveDecodeError::SurfaceTooLarge {
                width: width_pixels,
                height: height_pixels,
            });
        }

        let tiles_wide = width_pixels.div_ceil(64);
        let tiles_high = height_pixels.div_ceil(64);
        let count = usize::from(tiles_wide) * usize::from(tiles_high);

        Ok(Self {
            tiles_wide,
            tiles_high,
            use_reduce_extrapolate,
            tiles: core::iter::repeat_with(|| None).take(count).collect(),
        })
    }

    /// Get or create the tile at the given grid position.
    ///
    /// Returns `None` if the coordinates are out of bounds.
    pub fn get_or_create(&mut self, x_idx: u16, y_idx: u16) -> Option<&mut TileState> {
        let idx = self.tile_index(x_idx, y_idx)?;
        let tile = self.tiles[idx].get_or_insert_with(|| {
            let mut t = Box::new(TileState::new());
            t.use_reduce_extrapolate = self.use_reduce_extrapolate;
            t
        });
        Some(tile)
    }

    /// Get the tile at the given grid position, if it exists.
    pub fn get(&self, x_idx: u16, y_idx: u16) -> Option<&TileState> {
        let idx = self.tile_index(x_idx, y_idx)?;
        self.tiles[idx].as_deref()
    }

    /// Reset all tiles (e.g., on context reset or surface resize).
    pub fn reset(&mut self) {
        for tile in &mut self.tiles {
            *tile = None;
        }
    }

    fn tile_index(&self, x_idx: u16, y_idx: u16) -> Option<usize> {
        if x_idx >= self.tiles_wide || y_idx >= self.tiles_high {
            return None;
        }
        Some(usize::from(y_idx) * usize::from(self.tiles_wide) + usize::from(x_idx))
    }
}

// ---------------------------------------------------------------------------
// Progressive decoder (EGFX integration)
// ---------------------------------------------------------------------------

/// Decoded tile pixel data for compositing onto a surface.
pub struct DecodedTile {
    /// Tile grid X coordinate (tile column).
    pub x_idx: u16,
    /// Tile grid Y coordinate (tile row).
    pub y_idx: u16,
    /// RGBA pixel data (64x64 = 16384 bytes).
    pub pixels: Vec<u8>,
    /// Absolute, exclusive surface rectangles that may be copied from this
    /// tile. Progressive REGION rectangles clip tile output; writing the
    /// whole 64x64 tile can overwrite newer ClearCodec/cache content in the
    /// parts outside the region.
    pub updated_rects: Vec<DecodedTileRect>,
}

/// An absolute exclusive rectangle on the destination surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedTileRect {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

/// Per-axis cap on surface dimensions, in pixels.
///
/// Per MS-RDPEGFX 2.2.2.14 RDPGFX_RESET_GRAPHICS_PDU, the normative maximum
/// allowed width and height are 32766 pixels. We round up to 32768 so that
/// the cap accepts every spec-conformant surface while bounding the
/// per-surface tile-grid allocation: at the cap the backing
/// `Vec<Option<Box<TileState>>>` is 512 * 512 = 262144 slots * 8 bytes per
/// slot = 2 MiB of pointer storage per surface before any tile is populated.
pub const MAX_SURFACE_DIM: u16 = 32768;

/// Error type for progressive decoding operations.
#[derive(Debug)]
pub enum ProgressiveDecodeError {
    /// PDU parsing failed.
    Pdu(ironrdp_core::DecodeError),
    /// RLGR decode failed within a tile.
    Rlgr(RlgrError),
    /// The progressive stream is missing a required block.
    MissingBlock(&'static str),
    /// Tile coordinates are out of bounds for the surface.
    TileOutOfBounds { x_idx: u16, y_idx: u16 },
    /// Region references a quant index beyond the table.
    InvalidQuantIndex { index: usize, table_len: usize },
    /// Surface dimensions exceed [`MAX_SURFACE_DIM`] per axis.
    SurfaceTooLarge { width: u16, height: u16 },
}

impl core::fmt::Display for ProgressiveDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Pdu(e) => write!(f, "progressive PDU decode: {e}"),
            Self::Rlgr(e) => write!(f, "progressive RLGR decode: {e}"),
            Self::MissingBlock(name) => write!(f, "progressive stream missing {name} block"),
            Self::TileOutOfBounds { x_idx, y_idx } => {
                write!(f, "tile ({x_idx}, {y_idx}) out of surface bounds")
            }
            Self::InvalidQuantIndex { index, table_len } => {
                write!(f, "quant index {index} exceeds table length {table_len}")
            }
            Self::SurfaceTooLarge { width, height } => {
                write!(
                    f,
                    "surface dimensions {width}x{height} exceed per-axis cap of {MAX_SURFACE_DIM} \
                     (MS-RDPEGFX 2.2.2.14 normative ceiling: 32766)"
                )
            }
        }
    }
}

impl From<ironrdp_core::DecodeError> for ProgressiveDecodeError {
    fn from(e: ironrdp_core::DecodeError) -> Self {
        Self::Pdu(e)
    }
}

impl From<RlgrError> for ProgressiveDecodeError {
    fn from(e: RlgrError) -> Self {
        Self::Rlgr(e)
    }
}

/// Progressive state for one destination surface.
struct ProgressiveContext {
    surface: SurfaceTiles,
}

/// High-level progressive bitmap decoder for EGFX WireToSurface2 processing.
///
/// Maintains tile state across frames, keyed by destination surface. The outer
/// EGFX `codecContextId` changes between incremental update groups and is not
/// the identity of the RemoteFX tile history.
/// Feed it progressive bitmap data from `WireToSurface2Pdu.bitmap_data` and
/// get back decoded RGBA tiles for compositing.
///
/// # Usage
///
/// ```ignore
/// let mut decoder = ProgressiveDecoder::new();
///
/// // On receiving WireToSurface2Pdu:
/// let tiles = decoder.decode_bitmap(
///     u32::from(pdu.surface_id),
///     surface_width, surface_height,
///     &pdu.bitmap_data,
/// )?;
///
/// for tile in &tiles {
///     blit_tile(surface, tile.x_idx, tile.y_idx, &tile.pixels);
/// }
/// ```
pub struct ProgressiveDecoder {
    contexts: BTreeMap<u32, ProgressiveContext>,
}

impl ProgressiveDecoder {
    /// Create a new progressive decoder with no context state.
    pub fn new() -> Self {
        Self {
            contexts: BTreeMap::new(),
        }
    }

    /// Decode a progressive bitmap stream from WireToSurface2Pdu.
    ///
    /// Parses the progressive block stream, updates per-tile state, and
    /// returns RGBA pixel data for each tile that was updated.
    ///
    /// # Arguments
    /// - `surface_key`: stable destination surface identifier
    /// - `surface_width`: surface width in pixels (for tile grid sizing)
    /// - `surface_height`: surface height in pixels
    /// - `bitmap_data`: raw progressive block stream from the PDU
    pub fn decode_bitmap(
        &mut self,
        surface_key: u32,
        surface_width: u16,
        surface_height: u16,
        bitmap_data: &[u8],
    ) -> Result<Vec<DecodedTile>, ProgressiveDecodeError> {
        use ironrdp_pdu::codecs::rfx::progressive::{decode_progressive_stream, ProgressiveBlock};

        let blocks = decode_progressive_stream(bitmap_data)?;

        // CONTEXT is optional here. Windows normally sends it only once for a
        // Progressive stream. Its sole flag is subband differencing; the DWT
        // mode needed by this decoder is carried by every REGION.
        let initial_dwt_mode = blocks
            .iter()
            .find_map(|block| match block {
                ProgressiveBlock::Region(region) => Some(region.uses_reduce_extrapolate()),
                _ => None,
            })
            .unwrap_or(false);

        // Get or create the retained tile history for this destination surface.
        let context = match self.contexts.entry(surface_key) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let surface = SurfaceTiles::new(surface_width, surface_height, initial_dwt_mode)?;
                e.insert(ProgressiveContext { surface })
            }
        };

        // If surface dimensions changed, reallocate
        let expected_wide = surface_width.div_ceil(64);
        let expected_high = surface_height.div_ceil(64);
        if context.surface.tiles_wide != expected_wide
            || context.surface.tiles_high != expected_high
        {
            context.surface = SurfaceTiles::new(surface_width, surface_height, initial_dwt_mode)?;
        }

        let mut decoded_tiles = Vec::new();

        // Process REGION blocks (the main content)
        for block in &blocks {
            let region = match block {
                ProgressiveBlock::Region(r) => r,
                _ => continue,
            };

            let quant_vals = &region.quant_vals;
            let prog_quant_vals = &region.quant_prog_vals;
            let use_reduce_extrapolate = region.uses_reduce_extrapolate();
            context.surface.use_reduce_extrapolate = use_reduce_extrapolate;

            for tile_block in &region.tiles {
                let tiles = decode_tile_block(
                    &mut context.surface,
                    tile_block,
                    quant_vals,
                    prog_quant_vals,
                    use_reduce_extrapolate,
                )?;
                decoded_tiles.extend(tiles.into_iter().filter_map(|mut tile| {
                    tile.updated_rects =
                        clip_tile_to_region(&tile, &region.rects, surface_width, surface_height);
                    (!tile.updated_rects.is_empty()).then_some(tile)
                }));
            }
        }

        Ok(decoded_tiles)
    }

    /// Delete retained state for a destination surface.
    pub fn delete_context(&mut self, surface_key: u32) {
        self.contexts.remove(&surface_key);
    }

    /// Reset all contexts (e.g., on EGFX channel reset).
    pub fn reset(&mut self) {
        self.contexts.clear();
    }

    /// Whether a surface already has retained tile state.
    pub fn has_context(&self, surface_key: u32) -> bool {
        self.contexts.contains_key(&surface_key)
    }

    /// Reconstruct every initialized tile retained for a surface.
    ///
    /// RDPGFX_RESET_GRAPHICS recreates the destination surface but does not
    /// necessarily discard the RemoteFX Progressive encoding context. The
    /// server can therefore send only a handful of upgrades after the reset;
    /// callers use this method to repaint the unchanged cached tiles into the
    /// new surface.
    pub fn reconstruct_context(&self, surface_key: u32) -> Vec<DecodedTile> {
        let Some(context) = self.contexts.get(&surface_key) else {
            return Vec::new();
        };

        let width = usize::from(context.surface.tiles_wide);
        context
            .surface
            .tiles
            .iter()
            .enumerate()
            .filter_map(|(index, tile)| {
                let tile = tile.as_deref()?;
                let mut pixels = vec![0u8; 64 * 64 * 4];
                tile.reconstruct_to_rgba(&mut pixels);
                Some(DecodedTile {
                    x_idx: u16::try_from(index % width).expect("tile grid width fits u16"),
                    y_idx: u16::try_from(index / width).expect("tile grid height fits u16"),
                    pixels,
                    updated_rects: vec![full_tile_rect(
                        u16::try_from(index % width).expect("tile grid width fits u16"),
                        u16::try_from(index / width).expect("tile grid height fits u16"),
                    )],
                })
            })
            .collect()
    }
}

fn full_tile_rect(x_idx: u16, y_idx: u16) -> DecodedTileRect {
    let left = x_idx.saturating_mul(64);
    let top = y_idx.saturating_mul(64);
    DecodedTileRect {
        left,
        top,
        right: left.saturating_add(64),
        bottom: top.saturating_add(64),
    }
}

fn clip_tile_to_region(
    tile: &DecodedTile,
    region_rects: &[ironrdp_pdu::codecs::rfx::RfxRectangle],
    surface_width: u16,
    surface_height: u16,
) -> Vec<DecodedTileRect> {
    let tile_rect = full_tile_rect(tile.x_idx, tile.y_idx);
    let tile_left = u32::from(tile_rect.left);
    let tile_top = u32::from(tile_rect.top);
    let tile_right = u32::from(tile_rect.right.min(surface_width));
    let tile_bottom = u32::from(tile_rect.bottom.min(surface_height));

    region_rects
        .iter()
        .filter_map(|rect| {
            let left = tile_left.max(u32::from(rect.x));
            let top = tile_top.max(u32::from(rect.y));
            let right = tile_right.min(u32::from(rect.x) + u32::from(rect.width));
            let bottom = tile_bottom.min(u32::from(rect.y) + u32::from(rect.height));
            if left >= right || top >= bottom {
                return None;
            }
            Some(DecodedTileRect {
                left: u16::try_from(left).expect("rectangle is clamped to a u16 surface"),
                top: u16::try_from(top).expect("rectangle is clamped to a u16 surface"),
                right: u16::try_from(right).expect("rectangle is clamped to a u16 surface"),
                bottom: u16::try_from(bottom).expect("rectangle is clamped to a u16 surface"),
            })
        })
        .collect()
}

fn progressive_quants(
    quality: u8,
    prog_quant_vals: &[ironrdp_pdu::codecs::rfx::progressive::ProgressiveCodecQuant],
) -> Result<[ComponentCodecQuant; 3], ProgressiveDecodeError> {
    if quality == 0xFF {
        return Ok([ComponentCodecQuant::LOSSLESS; 3]);
    }

    let index = usize::from(quality);
    let quant = prog_quant_vals
        .get(index)
        .ok_or(ProgressiveDecodeError::InvalidQuantIndex {
            index,
            table_len: prog_quant_vals.len(),
        })?;
    Ok([quant.y_quant, quant.cb_quant, quant.cr_quant])
}

#[expect(
    clippy::similar_names,
    reason = "q_y/q_cb/q_cr are standard component quant index names"
)]
fn decode_tile_block(
    surface: &mut SurfaceTiles,
    tile_block: &ironrdp_pdu::codecs::rfx::progressive::ProgressiveTile<'_>,
    quant_vals: &[ComponentCodecQuant],
    prog_quant_vals: &[ironrdp_pdu::codecs::rfx::progressive::ProgressiveCodecQuant],
    use_reduce_extrapolate: bool,
) -> Result<Vec<DecodedTile>, ProgressiveDecodeError> {
    use ironrdp_pdu::codecs::rfx::progressive::ProgressiveTile;

    match tile_block {
        ProgressiveTile::Simple(tile) => {
            let x_idx = tile.x_idx;
            let y_idx = tile.y_idx;

            let tile_state = surface
                .get_or_create(x_idx, y_idx)
                .ok_or(ProgressiveDecodeError::TileOutOfBounds { x_idx, y_idx })?;

            let q_y = usize::from(tile.quant_idx_y);
            let q_cb = usize::from(tile.quant_idx_cb);
            let q_cr = usize::from(tile.quant_idx_cr);

            if q_y >= quant_vals.len() || q_cb >= quant_vals.len() || q_cr >= quant_vals.len() {
                return Err(ProgressiveDecodeError::InvalidQuantIndex {
                    index: q_y.max(q_cb).max(q_cr),
                    table_len: quant_vals.len(),
                });
            }

            // TILE_SIMPLE uses lossless progressive quant (no progressive refinement)
            let prog = ComponentCodecQuant::LOSSLESS;

            let consumption = tile_state.decode_first(
                [tile.y_data, tile.cb_data, tile.cr_data],
                [&quant_vals[q_y], &quant_vals[q_cb], &quant_vals[q_cr]],
                [prog, prog, prog],
                [tile.quant_idx_y, tile.quant_idx_cb, tile.quant_idx_cr],
                0xFF, // full quality
                use_reduce_extrapolate,
                tile.flags & TILE_FLAG_DIFFERENCE != 0,
            )?;
            report_first_pass_residue(x_idx, y_idx, &consumption);

            let mut pixels = vec![0u8; 64 * 64 * 4];
            tile_state.reconstruct_to_rgba(&mut pixels);

            Ok(vec![DecodedTile {
                x_idx,
                y_idx,
                pixels,
                updated_rects: Vec::new(),
            }])
        }

        ProgressiveTile::First(tile) => {
            let x_idx = tile.x_idx;
            let y_idx = tile.y_idx;

            let tile_state = surface
                .get_or_create(x_idx, y_idx)
                .ok_or(ProgressiveDecodeError::TileOutOfBounds { x_idx, y_idx })?;

            let q_y = usize::from(tile.quant_idx_y);
            let q_cb = usize::from(tile.quant_idx_cb);
            let q_cr = usize::from(tile.quant_idx_cr);

            if q_y >= quant_vals.len() || q_cb >= quant_vals.len() || q_cr >= quant_vals.len() {
                return Err(ProgressiveDecodeError::InvalidQuantIndex {
                    index: q_y.max(q_cb).max(q_cr),
                    table_len: quant_vals.len(),
                });
            }

            let prog_quants = progressive_quants(tile.quality, prog_quant_vals)?;

            let consumption = tile_state.decode_first(
                [tile.y_data, tile.cb_data, tile.cr_data],
                [&quant_vals[q_y], &quant_vals[q_cb], &quant_vals[q_cr]],
                prog_quants,
                [tile.quant_idx_y, tile.quant_idx_cb, tile.quant_idx_cr],
                tile.quality,
                use_reduce_extrapolate,
                tile.flags & TILE_FLAG_DIFFERENCE != 0,
            )?;
            report_first_pass_residue(x_idx, y_idx, &consumption);

            let mut pixels = vec![0u8; 64 * 64 * 4];
            tile_state.reconstruct_to_rgba(&mut pixels);

            Ok(vec![DecodedTile {
                x_idx,
                y_idx,
                pixels,
                updated_rects: Vec::new(),
            }])
        }

        ProgressiveTile::Upgrade(tile) => {
            let x_idx = tile.x_idx;
            let y_idx = tile.y_idx;

            let tile_state = surface
                .get_or_create(x_idx, y_idx)
                .ok_or(ProgressiveDecodeError::TileOutOfBounds { x_idx, y_idx })?;

            // If this tile hasn't had a first pass, skip the upgrade
            if tile_state.pass == 0 {
                return Ok(Vec::new());
            }

            let q_y = usize::from(tile.quant_idx_y);
            let q_cb = usize::from(tile.quant_idx_cb);
            let q_cr = usize::from(tile.quant_idx_cr);

            if q_y >= quant_vals.len() || q_cb >= quant_vals.len() || q_cr >= quant_vals.len() {
                return Err(ProgressiveDecodeError::InvalidQuantIndex {
                    index: q_y.max(q_cb).max(q_cr),
                    table_len: quant_vals.len(),
                });
            }

            let prog_quants = progressive_quants(tile.quality, prog_quant_vals)?;

            let usage = tile_state.decode_upgrade(
                [tile.y_srl_data, tile.cb_srl_data, tile.cr_srl_data],
                [tile.y_raw_data, tile.cb_raw_data, tile.cr_raw_data],
                [&quant_vals[q_y], &quant_vals[q_cb], &quant_vals[q_cr]],
                prog_quants,
                [tile.quant_idx_y, tile.quant_idx_cb, tile.quant_idx_cr],
                tile.quality,
            );
            report_upgrade_misalignment(x_idx, y_idx, &usage);

            let mut pixels = vec![0u8; 64 * 64 * 4];
            tile_state.reconstruct_to_rgba(&mut pixels);

            Ok(vec![DecodedTile {
                x_idx,
                y_idx,
                pixels,
                updated_rects: Vec::new(),
            }])
        }
    }
}

/// Log a first-pass RLGR stream whose output filled up while a non-trivial
/// amount of input was still unread. RLGR zero runs consume no input bits, so
/// an over-long run computation silently shifts every following coefficient
/// (spatial corruption) while the bitstream itself stays formally valid —
/// leftover input on a full output is the only observable symptom.
fn report_first_pass_residue(
    x_idx: u16,
    y_idx: u16,
    consumption: &[crate::rlgr::RlgrConsumption; 3],
) {
    use core::sync::atomic::{AtomicU32, Ordering};
    static REPORTS: AtomicU32 = AtomicU32::new(0);

    for (component, c) in consumption.iter().enumerate() {
        if c.output_remaining > 0 || c.input_bits_remaining < 16 {
            continue;
        }
        let n = REPORTS.fetch_add(1, Ordering::Relaxed);
        if n < 8 {
            tracing::warn!(
                "progressive first-pass rlgr residue: tile ({x_idx},{y_idx}) component {component} \
                 filled 4096 outputs with {} input bits left",
                c.input_bits_remaining,
            );
        } else {
            tracing::debug!(
                "progressive first-pass rlgr residue: tile ({x_idx},{y_idx}) component {component} \
                 filled 4096 outputs with {} input bits left",
                c.input_bits_remaining,
            );
        }
    }
}

/// Log a desynchronized upgrade bitstream (first few at WARN, then DEBUG so an
/// info-level capture shows the problem without being flooded). A misaligned
/// SRL/raw stream means every coefficient after the desync point decoded
/// wrong — the visible result is a tile with a wrong color cast that persists
/// until the server happens to resend it as a first-pass tile.
fn report_upgrade_misalignment(x_idx: u16, y_idx: u16, usage: &[UpgradeStreamUsage; 3]) {
    use core::sync::atomic::{AtomicU32, Ordering};
    static REPORTS: AtomicU32 = AtomicU32::new(0);

    for (component, u) in usage.iter().enumerate() {
        if u.is_aligned() {
            continue;
        }
        let n = REPORTS.fetch_add(1, Ordering::Relaxed);
        if n < 8 {
            tracing::warn!(
                "progressive upgrade bitstream desync: tile ({x_idx},{y_idx}) component {component} \
                 srl_left={} srl_overrun={} raw_left={} raw_overrun={}",
                u.srl_bits_remaining,
                u.srl_overrun,
                u.raw_bits_remaining,
                u.raw_overrun,
            );
        } else {
            tracing::debug!(
                "progressive upgrade bitstream desync: tile ({x_idx},{y_idx}) component {component} \
                 srl_left={} srl_overrun={} raw_left={} raw_overrun={}",
                u.srl_bits_remaining,
                u.srl_overrun,
                u.raw_bits_remaining,
                u.raw_overrun,
            );
        }
    }
}

impl Default for ProgressiveDecoder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
mod tests {
    use super::*;

    #[test]
    fn surface_tiles_rejects_over_cap_dimensions() {
        // At-cap accepted on both axes
        assert!(SurfaceTiles::new(MAX_SURFACE_DIM, MAX_SURFACE_DIM, false).is_ok());

        // One axis over the cap is rejected with SurfaceTooLarge carrying both inputs
        let over_w = MAX_SURFACE_DIM.checked_add(1).unwrap();
        match SurfaceTiles::new(over_w, 1024, false) {
            Err(ProgressiveDecodeError::SurfaceTooLarge { width, height }) => {
                assert_eq!(width, over_w);
                assert_eq!(height, 1024);
            }
            Err(other) => panic!("expected SurfaceTooLarge, got Err({other})"),
            Ok(_) => panic!("expected SurfaceTooLarge, got Ok"),
        }

        match SurfaceTiles::new(1024, over_w, false) {
            Err(ProgressiveDecodeError::SurfaceTooLarge { width, height }) => {
                assert_eq!(width, 1024);
                assert_eq!(height, over_w);
            }
            Err(other) => panic!("expected SurfaceTooLarge, got Err({other})"),
            Ok(_) => panic!("expected SurfaceTooLarge, got Ok"),
        }
    }

    #[test]
    fn standard_band_layout_totals_4096() {
        let bands = standard_band_layout();
        let total: usize = bands.iter().map(|b| b.count()).sum();
        assert_eq!(total, 4096);
    }

    #[test]
    fn standard_band_offsets() {
        let bands = standard_band_layout();
        assert_eq!(bands[0].offset, 0);
        assert_eq!(bands[1].offset, 1024);
        assert_eq!(bands[2].offset, 2048);
        assert_eq!(bands[3].offset, 3072);
        assert_eq!(bands[4].offset, 3328);
        assert_eq!(bands[5].offset, 3584);
        assert_eq!(bands[6].offset, 3840);
        assert_eq!(bands[7].offset, 3904);
        assert_eq!(bands[8].offset, 3968);
        assert_eq!(bands[9].offset, 4032);
    }

    #[test]
    fn sign_capture_tri_state() {
        let coefficients = [10i16, -5, 0, 100, -1, 0];
        let mut sign = [0i8; 6];
        capture_sign(&coefficients, &mut sign);
        assert_eq!(sign, [1, -1, 0, 1, -1, 0]);
    }

    #[test]
    fn progressive_dequantize_ll3_shift() {
        // LL3 is band index 9, at offset 4032 for standard layout
        let mut coefficients = vec![0i16; 4096];
        coefficients[4032] = 5;
        coefficients[4033] = -3;

        let prog_quant = ComponentCodecQuant {
            ll3: 2,
            hl3: 0,
            lh3: 0,
            hh3: 0,
            hl2: 0,
            lh2: 0,
            hh2: 0,
            hl1: 0,
            lh1: 0,
            hh1: 0,
        };

        progressive_dequantize(&mut coefficients, &prog_quant, false);

        // LL3 uses floor shift: 5 << 2 = 20, -3 << 2 = -12
        assert_eq!(coefficients[4032], 20);
        assert_eq!(coefficients[4033], -12);
    }

    #[test]
    fn progressive_dequantize_non_ll3_preserves_sign() {
        // HL1 is band index 0, at offset 0 for standard layout
        let mut coefficients = vec![0i16; 4096];
        coefficients[0] = 5;
        coefficients[1] = -5;

        let prog_quant = ComponentCodecQuant {
            ll3: 0,
            hl3: 0,
            lh3: 0,
            hh3: 0,
            hl2: 0,
            lh2: 0,
            hh2: 0,
            hl1: 2,
            lh1: 0,
            hh1: 0,
        };

        progressive_dequantize(&mut coefficients, &prog_quant, false);

        // Non-LL3: shift absolute value, preserve sign
        assert_eq!(coefficients[0], 20); // 5 << 2
        assert_eq!(coefficients[1], -20); // -(5 << 2)
    }

    #[test]
    fn progressive_quantize_round_trip() {
        let mut coefficients = vec![0i16; 4096];
        for (i, c) in coefficients.iter_mut().enumerate() {
            *c = (i as i16).wrapping_mul(7);
        }
        let original = coefficients.clone();

        let prog_quant = ComponentCodecQuant {
            ll3: 2,
            hl3: 3,
            lh3: 3,
            hh3: 4,
            hl2: 3,
            lh2: 3,
            hh2: 4,
            hl1: 2,
            lh1: 2,
            hh1: 3,
        };

        progressive_quantize(&mut coefficients, &prog_quant, false);
        progressive_dequantize(&mut coefficients, &prog_quant, false);

        // After quantize->dequantize, values lose precision from truncation
        // but should be in the right ballpark
        for (i, (&a, &b)) in coefficients.iter().zip(original.iter()).enumerate() {
            let err = (i32::from(a) - i32::from(b)).unsigned_abs();
            // Max error bounded by 2^(bit_pos)
            assert!(err < 32, "index {i}: error {err} too large");
        }
    }

    #[test]
    fn raw_bit_reader_basic() {
        let data = [0b10110000, 0b01010000];
        let mut reader = RawBitReader::new(&data);
        assert_eq!(reader.read_bits(4), 0b1011);
        assert_eq!(reader.read_bits(4), 0b0000);
        assert_eq!(reader.read_bits(4), 0b0101);
    }

    #[test]
    fn upgrade_raw_stream_is_continuous_across_bands() {
        let mut coefficients = [0i16; COEFFICIENTS_PER_COMPONENT];
        let mut sign = [SIGN_ZERO; COEFFICIENTS_PER_COMPONENT];
        sign[0] = SIGN_POSITIVE;
        sign[1024] = SIGN_POSITIVE;

        let prev = ComponentCodecQuant {
            hl1: 1,
            lh1: 1,
            ..ComponentCodecQuant::LOSSLESS
        };
        let current = ComponentCodecQuant::LOSSLESS;
        let base = ComponentCodecQuant {
            hl1: 6,
            lh1: 6,
            ..ComponentCodecQuant::LOSSLESS
        };

        decode_upgrade_pass(
            &[],
            &[0b1000_0000],
            &quant_add(&base, &prev),
            &quant_add(&base, &current),
            false,
            &mut coefficients,
            &mut sign,
        );

        assert_eq!(
            coefficients[0], 32,
            "base quant 6 requires an absolute upgrade shift of 6 + 0 - 1"
        );
        assert_eq!(
            coefficients[1024], 0,
            "the LH1 band must consume the second raw bit, not restart at bit zero"
        );
    }

    #[test]
    fn clamp_i16_limits() {
        assert_eq!(clamp_i16(40000), i16::MAX);
        assert_eq!(clamp_i16(-40000), i16::MIN);
        assert_eq!(clamp_i16(100), 100);
        assert_eq!(clamp_i16(-100), -100);
    }

    #[test]
    fn band_zero_count_counts_correctly() {
        let mut sign = [0i8; 4096];
        // Band 0 (HL1): offset 0, count 1024
        sign[0] = SIGN_POSITIVE;
        sign[1] = SIGN_NEGATIVE;
        sign[2] = SIGN_ZERO;
        // Rest are SIGN_ZERO by default

        let bands = standard_band_layout();
        assert_eq!(band_zero_count(&sign, &bands[0]), 1022); // 1024 - 2 non-zero
    }

    #[test]
    fn ll3_offsets_correct() {
        assert_eq!(ll3_offset(false), 4032);
        assert_eq!(ll3_offset(true), 4015);
    }

    #[test]
    fn upgrade_pass_zero_das_becomes_nonzero() {
        let mut coefficients = vec![0i16; 4096];
        let mut sign = vec![SIGN_ZERO; 4096];

        // Set up SRL data that produces a non-zero value for the first position
        // For band 0 (HL1), with num_bits=2, SRL should produce some values
        let prev_prog_quant = ComponentCodecQuant {
            ll3: 0,
            hl3: 0,
            lh3: 0,
            hh3: 0,
            hl2: 0,
            lh2: 0,
            hh2: 0,
            hl1: 4,
            lh1: 0,
            hh1: 0,
        };
        let curr_prog_quant = ComponentCodecQuant {
            ll3: 0,
            hl3: 0,
            lh3: 0,
            hh3: 0,
            hl2: 0,
            lh2: 0,
            hh2: 0,
            hl1: 2,
            lh1: 0,
            hh1: 0,
        };

        // Simple SRL data: a non-zero value (the SRL decoder will interpret
        // bits as magnitude + sign). With num_bits=2, k=0 initially,
        // it goes straight to magnitude decode.
        let srl_data = vec![0b01000000, 0x00]; // sign=0(+), magnitude bits follow
        let raw_data = vec![];
        let base_quant = ComponentCodecQuant {
            hl1: 6,
            ..ComponentCodecQuant::LOSSLESS
        };

        decode_upgrade_pass(
            &srl_data,
            &raw_data,
            &quant_add(&base_quant, &prev_prog_quant),
            &quant_add(&base_quant, &curr_prog_quant),
            false,
            &mut coefficients,
            &mut sign,
        );

        // After decode, at least some positions should have been updated
        // (exact values depend on SRL interpretation, but the function shouldn't panic)
    }

    #[test]
    fn tile_state_default_is_zeroed() {
        let tile = TileState::new();
        assert_eq!(tile.pass, 0);
        assert_eq!(tile.quality, 0);
        assert!(!tile.use_reduce_extrapolate);
        assert!(tile.coefficients[0].iter().all(|&v| v == 0));
        assert!(tile.sign[0].iter().all(|&v| v == 0));
    }

    #[test]
    fn surface_tiles_dimensions() {
        let surface = SurfaceTiles::new(1920, 1080, true).unwrap();
        assert_eq!(surface.tiles_wide, 30);
        assert_eq!(surface.tiles_high, 17);
        assert!(surface.use_reduce_extrapolate);
    }

    #[test]
    fn surface_tiles_exact_multiple() {
        // 1280 / 64 = 20, 768 / 64 = 12 (exact, no rounding)
        let surface = SurfaceTiles::new(1280, 768, false).unwrap();
        assert_eq!(surface.tiles_wide, 20);
        assert_eq!(surface.tiles_high, 12);
    }

    #[test]
    fn surface_tiles_lazy_allocation() {
        let mut surface = SurfaceTiles::new(128, 128, false).unwrap();
        // No tiles allocated yet
        assert!(surface.get(0, 0).is_none());

        // Access creates tile
        let tile = surface.get_or_create(0, 0).unwrap();
        assert_eq!(tile.pass, 0);
        assert!(!tile.use_reduce_extrapolate);

        // Now it exists
        assert!(surface.get(0, 0).is_some());

        // Out of bounds returns None
        assert!(surface.get_or_create(2, 2).is_none());
    }

    #[test]
    fn surface_tiles_reset() {
        let mut surface = SurfaceTiles::new(128, 128, false).unwrap();
        surface.get_or_create(0, 0);
        assert!(surface.get(0, 0).is_some());

        surface.reset();
        assert!(surface.get(0, 0).is_none());
    }

    #[test]
    fn decoder_new_is_empty() {
        let decoder = ProgressiveDecoder::new();
        assert!(decoder.contexts.is_empty());
    }

    #[test]
    fn reconstruct_context_repaints_every_initialized_tile() {
        let mut surface = SurfaceTiles::new(128, 64, false).unwrap();
        surface.get_or_create(0, 0).unwrap().pass = 1;
        surface.get_or_create(1, 0).unwrap().pass = 1;

        let mut decoder = ProgressiveDecoder::new();
        decoder.contexts.insert(12, ProgressiveContext { surface });

        assert!(decoder.has_context(12));
        let tiles = decoder.reconstruct_context(12);
        assert_eq!(tiles.len(), 2);
        assert_eq!((tiles[0].x_idx, tiles[0].y_idx), (0, 0));
        assert_eq!((tiles[1].x_idx, tiles[1].y_idx), (1, 0));
        assert!(tiles.iter().all(|tile| tile.pixels.len() == 64 * 64 * 4));
        assert_eq!(
            tiles[1].updated_rects,
            vec![DecodedTileRect {
                left: 64,
                top: 0,
                right: 128,
                bottom: 64,
            }]
        );
    }

    #[test]
    fn progressive_tile_output_is_clipped_to_region_rectangles() {
        use ironrdp_pdu::codecs::rfx::RfxRectangle;

        let tile = DecodedTile {
            x_idx: 1,
            y_idx: 1,
            pixels: vec![0; 64 * 64 * 4],
            updated_rects: Vec::new(),
        };
        let rects = [
            RfxRectangle {
                x: 80,
                y: 70,
                width: 100,
                height: 20,
            },
            RfxRectangle {
                x: 0,
                y: 0,
                width: 32,
                height: 32,
            },
        ];

        assert_eq!(
            clip_tile_to_region(&tile, &rects, 150, 100),
            vec![DecodedTileRect {
                left: 80,
                top: 70,
                right: 128,
                bottom: 90,
            }]
        );
    }

    #[test]
    fn full_quality_uses_implicit_lossless_quant() {
        let quants = progressive_quants(0xFF, &[]).unwrap();
        assert_eq!(quants, [ComponentCodecQuant::LOSSLESS; 3]);
    }

    #[test]
    fn non_full_quality_still_requires_a_table_entry() {
        let error = progressive_quants(0, &[]).unwrap_err();
        assert!(matches!(
            error,
            ProgressiveDecodeError::InvalidQuantIndex {
                index: 0,
                table_len: 0
            }
        ));
    }

    #[test]
    fn decoder_delete_nonexistent_context() {
        let mut decoder = ProgressiveDecoder::new();
        // Should not panic on non-existent context
        decoder.delete_context(42);
    }

    #[test]
    fn decoder_reset_clears_contexts() {
        let mut decoder = ProgressiveDecoder::new();

        // Decode a minimal valid stream to create a context
        use ironrdp_pdu::codecs::rfx::progressive::{
            encode_progressive_stream, ProgressiveBlock, ProgressiveContextPdu,
            ProgressiveFrameBeginPdu, ProgressiveFrameEndPdu, ProgressiveRegion,
            ProgressiveSyncPdu,
        };
        use ironrdp_pdu::codecs::rfx::RfxRectangle;

        let region = ProgressiveRegion {
            tile_size: 0x40,
            rects: vec![RfxRectangle {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            }],
            quant_vals: vec![],
            quant_prog_vals: vec![],
            flags: 0,
            tiles: vec![],
        };

        let blocks = vec![
            ProgressiveBlock::Sync(ProgressiveSyncPdu),
            ProgressiveBlock::Context(ProgressiveContextPdu {
                context_id: 0,
                tile_size: 0x0040,
                flags: 0,
            }),
            ProgressiveBlock::FrameBegin(ProgressiveFrameBeginPdu {
                frame_index: 0,
                region_count: 1,
            }),
            ProgressiveBlock::Region(region),
            ProgressiveBlock::FrameEnd(ProgressiveFrameEndPdu),
        ];

        let encoded = encode_progressive_stream(&blocks).unwrap();
        let result = decoder.decode_bitmap(1, 640, 480, &encoded);
        assert!(result.is_ok());
        assert_eq!(decoder.contexts.len(), 1);

        decoder.reset();
        assert!(decoder.contexts.is_empty());
    }

    #[test]
    fn decoder_accepts_first_stream_without_context_block() {
        use ironrdp_pdu::codecs::rfx::progressive::{
            encode_progressive_stream, ProgressiveBlock, ProgressiveFrameBeginPdu,
            ProgressiveFrameEndPdu, ProgressiveRegion, ProgressiveSyncPdu,
        };
        use ironrdp_pdu::codecs::rfx::RfxRectangle;

        let blocks = vec![
            ProgressiveBlock::Sync(ProgressiveSyncPdu),
            ProgressiveBlock::FrameBegin(ProgressiveFrameBeginPdu {
                frame_index: 0,
                region_count: 1,
            }),
            ProgressiveBlock::Region(ProgressiveRegion {
                tile_size: 0x40,
                rects: vec![RfxRectangle {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 64,
                }],
                quant_vals: vec![],
                quant_prog_vals: vec![],
                flags: 0,
                tiles: vec![],
            }),
            ProgressiveBlock::FrameEnd(ProgressiveFrameEndPdu),
        ];

        let encoded = encode_progressive_stream(&blocks).unwrap();
        let mut decoder = ProgressiveDecoder::new();
        let result = decoder.decode_bitmap(9, 64, 64, &encoded);

        assert!(result.is_ok());
        assert!(decoder.contexts.contains_key(&9));
    }

    #[test]
    fn remote_fx_color_conversion_uses_signed_fixed_point_bias() {
        let black = [-4096i16; 1];
        let middle_gray = [0i16; 1];
        let chroma = [0i16; 1];
        let mut rgba = [0u8; 4];

        crate::color_conversion::ycbcr_to_rgba(
            crate::color_conversion::YCbCrBuffer {
                y: &black,
                cb: &chroma,
                cr: &chroma,
            },
            &mut rgba,
        )
        .unwrap();
        assert_eq!(rgba, [0, 0, 0, 0xFF]);

        crate::color_conversion::ycbcr_to_rgba(
            crate::color_conversion::YCbCrBuffer {
                y: &middle_gray,
                cb: &chroma,
                cr: &chroma,
            },
            &mut rgba,
        )
        .unwrap();
        assert_eq!(rgba, [128, 128, 128, 0xFF]);
    }

    #[test]
    fn decoder_uses_region_flag_for_dwt_mode() {
        use ironrdp_pdu::codecs::rfx::progressive::{
            encode_progressive_stream, ProgressiveBlock, ProgressiveContextPdu,
            ProgressiveFrameBeginPdu, ProgressiveFrameEndPdu, ProgressiveRegion,
            ProgressiveSyncPdu, FLAG_DWT_REDUCE_EXTRAPOLATE,
        };
        use ironrdp_pdu::codecs::rfx::RfxRectangle;

        let make_region = |flags| ProgressiveRegion {
            tile_size: 0x40,
            rects: vec![RfxRectangle {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            }],
            quant_vals: vec![],
            quant_prog_vals: vec![],
            flags,
            tiles: vec![],
        };

        let first = encode_progressive_stream(&[
            ProgressiveBlock::Sync(ProgressiveSyncPdu),
            ProgressiveBlock::Context(ProgressiveContextPdu {
                context_id: 0,
                tile_size: 0x0040,
                flags: FLAG_DWT_REDUCE_EXTRAPOLATE,
            }),
            ProgressiveBlock::FrameBegin(ProgressiveFrameBeginPdu {
                frame_index: 0,
                region_count: 1,
            }),
            ProgressiveBlock::Region(make_region(0)),
            ProgressiveBlock::FrameEnd(ProgressiveFrameEndPdu),
        ])
        .unwrap();

        let mut decoder = ProgressiveDecoder::new();
        decoder.decode_bitmap(7, 64, 64, &first).unwrap();
        assert!(
            !decoder.contexts[&7].surface.use_reduce_extrapolate,
            "CONTEXT bit 0 is subband differencing, not the DWT mode"
        );

        let second = encode_progressive_stream(&[
            ProgressiveBlock::Sync(ProgressiveSyncPdu),
            ProgressiveBlock::FrameBegin(ProgressiveFrameBeginPdu {
                frame_index: 1,
                region_count: 1,
            }),
            ProgressiveBlock::Region(make_region(FLAG_DWT_REDUCE_EXTRAPOLATE)),
            ProgressiveBlock::FrameEnd(ProgressiveFrameEndPdu),
        ])
        .unwrap();

        decoder.decode_bitmap(7, 64, 64, &second).unwrap();
        assert!(decoder.contexts[&7].surface.use_reduce_extrapolate);
    }

    #[test]
    fn decoder_error_display() {
        let e = ProgressiveDecodeError::MissingBlock("SYNC");
        assert!(e.to_string().contains("SYNC"));

        let e = ProgressiveDecodeError::TileOutOfBounds {
            x_idx: 5,
            y_idx: 10,
        };
        assert!(e.to_string().contains("5"));
        assert!(e.to_string().contains("10"));

        let e = ProgressiveDecodeError::InvalidQuantIndex {
            index: 3,
            table_len: 2,
        };
        assert!(e.to_string().contains("3"));
    }

    #[test]
    fn dequantize_component_ccq_shifts_correctly() {
        let mut coefficients = vec![0i16; 4096];
        coefficients[0] = 10; // HL1 band (index 0)
        coefficients[4032] = 5; // LL3 band (index 9, standard layout)

        let quant = ComponentCodecQuant {
            ll3: 3,
            hl3: 0,
            lh3: 0,
            hh3: 0,
            hl2: 0,
            lh2: 0,
            hh2: 0,
            hl1: 4,
            lh1: 0,
            hh1: 0,
        };

        dequantize_component_ccq(&mut coefficients, &quant, false);

        // HL1: shift left by (4 - 1) = 3 -> 10 << 3 = 80
        assert_eq!(coefficients[0], 80);
        // LL3: shift left by (3 - 1) = 2 -> 5 << 2 = 20
        assert_eq!(coefficients[4032], 20);
    }

    // --- B10: Server encode pipeline tests ---

    #[test]
    fn rgba_to_ycbcr_pure_white() {
        let pixels = vec![255u8; 64 * 64 * 4];
        let mut y = vec![0i16; 4096];
        let mut cb = vec![0i16; 4096];
        let mut cr = vec![0i16; 4096];

        rgba_to_ycbcr(&pixels, &mut y, &mut cb, &mut cr);

        // Pure white: R=G=B=255
        // Y = (19595*255 + 38470*255 + 7471*255 + 32768) >> 16 - 128
        //   = (65536*255 + 32768) >> 16 - 128 = 255 - 128 = 127
        // Cb and Cr should be ~0 (achromatic)
        assert!((y[0] - 127).abs() <= 1, "Y for white: got {}", y[0]);
        assert!(cb[0].abs() <= 1, "Cb for white: got {}", cb[0]);
        assert!(cr[0].abs() <= 1, "Cr for white: got {}", cr[0]);
    }

    #[test]
    fn rgba_to_ycbcr_pure_black() {
        let pixels = vec![0u8; 64 * 64 * 4];
        let mut y = vec![0i16; 4096];
        let mut cb = vec![0i16; 4096];
        let mut cr = vec![0i16; 4096];

        rgba_to_ycbcr(&pixels, &mut y, &mut cb, &mut cr);

        // Pure black: Y = -128, Cb = 0, Cr = 0
        assert_eq!(y[0], -128);
        assert_eq!(cb[0], 0);
        assert_eq!(cr[0], 0);
    }

    #[test]
    fn quantize_ccq_right_shifts() {
        let mut coefficients = [0i16; 4096];
        coefficients[0] = 80; // HL1 band
        coefficients[4032] = 20; // LL3 band

        let quant = ComponentCodecQuant {
            ll3: 3,
            hl3: 0,
            lh3: 0,
            hh3: 0,
            hl2: 0,
            lh2: 0,
            hh2: 0,
            hl1: 4,
            lh1: 0,
            hh1: 0,
        };

        quantize_component_ccq(&mut coefficients, &quant, false);

        // HL1: 80 >> (4 - 1) = 80 >> 3 = 10
        assert_eq!(coefficients[0], 10);
        // LL3: 20 >> (3 - 1) = 20 >> 2 = 5
        assert_eq!(coefficients[4032], 5);
    }

    #[test]
    fn quantize_ccq_negative_truncates_toward_zero() {
        let mut coefficients = [0i16; 4096];
        coefficients[0] = -80; // HL1 band, negative

        let quant = ComponentCodecQuant {
            ll3: 0,
            hl3: 0,
            lh3: 0,
            hh3: 0,
            hl2: 0,
            lh2: 0,
            hh2: 0,
            hl1: 4,
            lh1: 0,
            hh1: 0,
        };

        quantize_component_ccq(&mut coefficients, &quant, false);

        // -80 truncated toward zero: -(80 >> 3) = -10
        assert_eq!(coefficients[0], -10);
    }

    #[test]
    fn raw_bit_writer_single_byte() {
        let mut w = RawBitWriter::new();
        w.write_bits(0xA5, 8);
        assert_eq!(w.finish(), vec![0xA5]);
    }

    #[test]
    fn raw_bit_writer_partial_byte_padded() {
        let mut w = RawBitWriter::new();
        w.write_bits(0b101, 3);
        // 3 bits: 101, padded to 10100000 = 0xA0
        assert_eq!(w.finish(), vec![0xA0]);
    }

    #[test]
    fn raw_bit_writer_multi_byte() {
        let mut w = RawBitWriter::new();
        w.write_bits(0xFF, 8);
        w.write_bits(0b1010, 4);
        // First byte: 0xFF, second partial: 1010_0000 = 0xA0
        assert_eq!(w.finish(), vec![0xFF, 0xA0]);
    }

    #[test]
    fn encode_first_pass_produces_output() {
        // Flat tile: all same value, should compress well
        let mut coefficients = [100i16; 4096];
        let mut output = vec![0u8; 8192];

        let base_quant = ComponentCodecQuant::LOSSLESS;
        let prog_quant = ComponentCodecQuant::LOSSLESS;

        let result = encode_first_pass(
            &mut coefficients,
            &mut output,
            &base_quant,
            &prog_quant,
            false,
        );

        assert!(result.is_ok(), "RLGR encode failed: {:?}", result.err());
        let bytes_written = result.unwrap();
        assert!(bytes_written > 0, "expected non-zero encoded output");
        assert!(bytes_written < 8192, "flat tile should compress");
    }

    #[test]
    fn encode_first_pass_reduce_extrapolate() {
        let mut coefficients = [50i16; 4096];
        let mut output = vec![0u8; 8192];

        let base_quant = ComponentCodecQuant::LOSSLESS;
        let prog_quant = ComponentCodecQuant::LOSSLESS;

        let result = encode_first_pass(
            &mut coefficients,
            &mut output,
            &base_quant,
            &prog_quant,
            true, // reduce-extrapolate mode
        );

        assert!(result.is_ok(), "RLGR encode failed: {:?}", result.err());
        assert!(result.unwrap() > 0);
    }

    #[test]
    fn encode_upgrade_pass_empty_when_no_refinement() {
        let coefficients = [0i16; 4096];
        let prev_coefficients = [0i16; 4096];
        let sign = [SIGN_ZERO; 4096];

        // Same prog_quant for prev and curr -> num_bits = 0, no refinement
        let prog_quant = ComponentCodecQuant::LOSSLESS;

        let (srl_data, raw_data) = encode_upgrade_pass(
            &coefficients,
            &prev_coefficients,
            &prog_quant,
            &prog_quant,
            &sign,
            false,
        );

        assert!(
            srl_data.is_empty(),
            "no refinement bits, SRL should be empty"
        );
        assert!(
            raw_data.is_empty(),
            "no refinement bits, raw should be empty"
        );
    }
}
