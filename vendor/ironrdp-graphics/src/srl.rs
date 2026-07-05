//! SRL (Simplified Run-Length) entropy codec for progressive upgrade passes.
//!
//! Used during progressive TILE_UPGRADE decoding where the tri-state sign
//! array (DAS) indicates zero-valued coefficients. SRL encodes/decodes
//! magnitudes for coefficients that were previously zero.
//!
//! The algorithm is similar to RLGR's zero-run mode with a simpler structure:
//! adaptive K parameter controlling zero-run lengths, followed by unary-coded
//! magnitudes with sign bits.

/// Stateful SRL decoder for one component upgrade stream.
///
/// The adaptive state and bit position span all ten DWT bands. Resetting them
/// per band replays the beginning of the stream and corrupts every refinement
/// after the first band.
pub(crate) struct SrlDecoder<'a> {
    reader: BitReader<'a>,
    kp: u32,
    remaining_zeros: u32,
    unary_mode: bool,
}

impl<'a> SrlDecoder<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self {
            reader: BitReader::new(data),
            kp: 8,
            remaining_zeros: 0,
            unary_mode: false,
        }
    }

    /// Unconsumed bits left in the SRL stream.
    ///
    /// A correctly aligned decode leaves less than one byte of padding plus at
    /// most one whole padding byte (the reference implementation tolerates
    /// exactly that). More indicates the consumer under-read the stream.
    pub(crate) fn bits_remaining(&self) -> usize {
        self.reader.bits_remaining()
    }

    /// Whether a read went past the end of the stream (over-read).
    pub(crate) fn overrun(&self) -> bool {
        self.reader.overrun()
    }

    /// Decode the next value for a zero-DAS coefficient.
    pub(crate) fn read(&mut self, num_bits: u8) -> i16 {
        if num_bits == 0 {
            return 0;
        }

        if self.remaining_zeros > 0 {
            self.remaining_zeros -= 1;
            return 0;
        }

        let k = self.kp / 8;
        if !self.unary_mode {
            if !self.reader.read_bit() {
                self.remaining_zeros = 1u32.checked_shl(k).unwrap_or(u32::MAX);
                self.kp = self.kp.saturating_add(4).min(80);
                self.remaining_zeros = self.remaining_zeros.saturating_sub(1);
                return 0;
            }

            self.remaining_zeros = self.reader.read_bits(k);
            self.unary_mode = true;
            if self.remaining_zeros > 0 {
                self.remaining_zeros -= 1;
                return 0;
            }
        }

        self.unary_mode = false;
        let negative = self.reader.read_bit();
        self.kp = self.kp.saturating_sub(6);

        let max_magnitude = 1u32
            .checked_shl(u32::from(num_bits))
            .unwrap_or(u32::MAX)
            .saturating_sub(1);
        let mut magnitude = 1u32;
        while magnitude < max_magnitude {
            if self.reader.read_bit() {
                break;
            }
            magnitude += 1;
        }

        let max_i16 = u32::try_from(i16::MAX).unwrap_or(u32::MAX);
        let magnitude = i16::try_from(magnitude.min(max_i16)).unwrap_or(i16::MAX);
        if negative {
            -magnitude
        } else {
            magnitude
        }
    }
}

/// Decode SRL data for a set of zero-valued (DAS=0) coefficient positions.
pub fn decode_srl(data: &[u8], num_values: usize, num_bits: u8) -> Vec<i16> {
    let mut decoder = SrlDecoder::new(data);
    core::iter::repeat_with(|| decoder.read(num_bits))
        .take(num_values)
        .collect()
}

/// Encode coefficient magnitudes using the SRL algorithm.
///
/// `values` contains signed coefficient values (non-zero = needs encoding,
/// zero = contributes to zero runs).
/// `num_bits` is the bit width for magnitude encoding.
///
/// Returns the encoded SRL byte stream (with trailing 0x00 sentinel).
pub fn encode_srl(values: &[i16], num_bits: u8) -> Vec<u8> {
    if values.is_empty() {
        return vec![0x00];
    }
    let values_with_widths: Vec<_> = values
        .iter()
        .copied()
        .map(|value| (value, num_bits))
        .collect();
    encode_srl_with_bit_widths(&values_with_widths)
}

pub(crate) fn encode_srl_with_bit_widths(values: &[(i16, u8)]) -> Vec<u8> {
    if values.is_empty() {
        return Vec::new();
    }

    let mut writer = BitWriter::new();
    let mut kp: u32 = 8;
    let mut idx = 0;

    while idx < values.len() {
        let mut unary_ready = false;
        // Count leading zeros (may be 0)
        let mut zero_count: u32 = 0;
        while idx + usize::try_from(zero_count).unwrap_or(usize::MAX) < values.len()
            && values[idx + usize::try_from(zero_count).unwrap_or(usize::MAX)].0 == 0
        {
            zero_count += 1;
        }

        // Encode zero run one chunk at a time, recomputing k after
        // each kp update to stay in sync with the decoder.
        while zero_count > 0 {
            let cur_k = kp >> 3;
            let chunk_size = 1u32.checked_shl(cur_k).unwrap_or(u32::MAX);
            if zero_count >= chunk_size {
                writer.write_bit(false);
                kp = kp.saturating_add(4).min(80);
                zero_count -= chunk_size;
                idx += usize::try_from(chunk_size).unwrap_or(usize::MAX);
            } else {
                // Remaining zeros < chunk: escape bit + count
                writer.write_bit(true);
                writer.write_bits(zero_count, cur_k);
                idx += usize::try_from(zero_count).unwrap_or(usize::MAX);
                zero_count = 0;
                unary_ready = true;
            }
        }

        if idx >= values.len() {
            break;
        }

        if !unary_ready {
            // No preceding short zero run: enter unary mode explicitly.
            let cur_k = kp >> 3;
            writer.write_bit(true);
            writer.write_bits(0, cur_k);
        }

        // Encode non-zero value
        kp = kp.saturating_sub(6);
        let (value, num_bits) = values[idx];
        let sign = value < 0;
        let magnitude = u32::from(value.unsigned_abs()).max(1);

        writer.write_bit(sign);

        let max_magnitude = 1u32
            .checked_shl(u32::from(num_bits))
            .unwrap_or(u32::MAX)
            .saturating_sub(1);
        for _ in 1..magnitude.min(max_magnitude) {
            writer.write_bit(false);
        }
        if magnitude < max_magnitude {
            writer.write_bit(true);
        }

        idx += 1;
    }

    let mut result = writer.finish();
    result.push(0x00);
    result
}

// ---------------------------------------------------------------------------
// Bit-level I/O helpers
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    byte_idx: usize,
    bit_idx: u8, // 0..7, MSB first
    overrun: bool,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_idx: 0,
            bit_idx: 0,
            overrun: false,
        }
    }

    fn bits_remaining(&self) -> usize {
        if self.byte_idx >= self.data.len() {
            return 0;
        }
        (self.data.len() - self.byte_idx) * 8 - usize::from(self.bit_idx)
    }

    fn overrun(&self) -> bool {
        self.overrun
    }

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

    fn read_bits(&mut self, count: u32) -> u32 {
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1) | u32::from(self.read_bit());
        }
        value
    }
}

struct BitWriter {
    bytes: Vec<u8>,
    current: u8,
    bit_count: u8, // bits written in current byte (0..7)
}

impl BitWriter {
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

    fn write_bits(&mut self, value: u32, count: u32) {
        for i in (0..count).rev() {
            self.write_bit((value >> i) & 1 != 0);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bit_count > 0 {
            // Pad remaining bits with zeros (MSB aligned)
            self.current <<= 8 - self.bit_count;
            self.bytes.push(self.current);
        }
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_empty() {
        let result = decode_srl(&[], 0, 1);
        assert!(result.is_empty());
    }

    #[test]
    fn decode_empty_data() {
        // With no data (empty slice), all positions default to zero
        let result = decode_srl(&[], 5, 1);
        assert_eq!(result, vec![0, 0, 0, 0, 0]);
    }

    #[test]
    fn encode_empty() {
        let encoded = encode_srl(&[], 1);
        assert_eq!(encoded, vec![0x00]); // just sentinel
    }

    #[test]
    fn encode_all_zeros() {
        let encoded = encode_srl(&[0, 0, 0], 1);
        // Sentinel must be present
        assert_eq!(*encoded.last().unwrap(), 0x00);
        // Round-trip: all zeros must survive
        let decoded = decode_srl(&encoded, 3, 1);
        assert_eq!(decoded, vec![0, 0, 0]);
    }

    #[test]
    fn round_trip_single_positive() {
        let original = vec![1];
        let encoded = encode_srl(&original, 1);
        let decoded = decode_srl(&encoded, 1, 1);
        assert_eq!(decoded, original);
    }

    #[test]
    fn round_trip_single_negative() {
        let original = vec![-1];
        let encoded = encode_srl(&original, 1);
        let decoded = decode_srl(&encoded, 1, 1);
        assert_eq!(decoded, original);
    }

    #[test]
    fn round_trip_mixed_zeros() {
        // Zeros at the start (where k=0) must survive the round-trip
        let original = vec![0, 0, 1, -1, 0, 3];
        let encoded = encode_srl(&original, 4);
        let decoded = decode_srl(&encoded, original.len(), 4);
        assert_eq!(decoded, original);
    }

    #[test]
    fn round_trip_nonzero_only() {
        let original = vec![1, -1, 2, -3, 1];
        let encoded = encode_srl(&original, 4);
        let decoded = decode_srl(&encoded, original.len(), 4);
        assert_eq!(decoded, original);
    }

    #[test]
    fn decodes_reference_bit_sequence_with_persistent_state() {
        // kp starts at 8. This byte encodes one zero, +1, then -2 at
        // numBits=2 while carrying kp/mode/nz across all three values.
        let encoded = [0b1101_1101, 0x00];
        assert_eq!(decode_srl(&encoded, 3, 2), vec![0, 1, -2]);
        assert_eq!(encode_srl(&[0, 1, -2], 2), encoded);
    }

    #[test]
    fn bit_reader_basic() {
        let data = [0b10110000];
        let mut reader = BitReader::new(&data);
        assert!(reader.read_bit()); // 1
        assert!(!reader.read_bit()); // 0
        assert!(reader.read_bit()); // 1
        assert!(reader.read_bit()); // 1
    }

    #[test]
    fn bit_writer_basic() {
        let mut writer = BitWriter::new();
        writer.write_bit(true);
        writer.write_bit(false);
        writer.write_bit(true);
        writer.write_bit(true);
        writer.write_bit(false);
        writer.write_bit(false);
        writer.write_bit(false);
        writer.write_bit(false);
        let result = writer.finish();
        assert_eq!(result, vec![0b10110000]);
    }

    #[test]
    fn bit_writer_multi_byte() {
        let mut writer = BitWriter::new();
        writer.write_bits(0xFF, 8);
        writer.write_bits(0x00, 8);
        let result = writer.finish();
        assert_eq!(result, vec![0xFF, 0x00]);
    }
}
