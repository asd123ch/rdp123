//! NSCodec decoder used by ClearCodec subcodec regions.
//!
//! NSCodec stores four separately compressed AYCoCg planes. The decoder
//! reconstructs those planes, optionally expands 2x2 chroma subsampling, and
//! converts the result to straight-alpha BGRA pixels.

use ironrdp_core::{DecodeResult, ReadCursor, ensure_size, invalid_field_err};

const PLANE_COUNT: usize = 4;
const NSCODEC_HEADER_SIZE: usize = 20;

pub(crate) fn decode(data: &[u8], width: u16, height: u16) -> DecodeResult<Vec<u8>> {
    let mut src = ReadCursor::new(data);
    ensure_size!(ctx: "NSCodecHeader", in: src, size: NSCODEC_HEADER_SIZE);

    let mut encoded_sizes = [0usize; PLANE_COUNT];
    for size in &mut encoded_sizes {
        *size = usize::try_from(src.read_u32())
            .map_err(|_| invalid_field_err!("planeByteCount", "plane size does not fit usize"))?;
    }

    let color_loss_level = src.read_u8();
    if !(1..=7).contains(&color_loss_level) {
        return Err(invalid_field_err!(
            "colorLossLevel",
            "color loss level must be in the range 1..=7"
        ));
    }

    let chroma_subsampling = src.read_u8();
    if chroma_subsampling > 1 {
        return Err(invalid_field_err!(
            "chromaSubsamplingLevel",
            "unsupported chroma subsampling level"
        ));
    }

    let _reserved = src.read_u16();
    let encoded_total = encoded_sizes.iter().try_fold(0usize, |total, size| {
        total
            .checked_add(*size)
            .ok_or_else(|| invalid_field_err!("planeByteCount", "encoded plane sizes overflow"))
    })?;
    ensure_size!(ctx: "NSCodecPlanes", in: src, size: encoded_total);

    let width = usize::from(width);
    let height = usize::from(height);
    let pixel_count = width
        .checked_mul(height)
        .ok_or_else(|| invalid_field_err!("dimensions", "width * height overflow"))?;

    let rounded_width = width
        .checked_add(7)
        .map(|value| value / 8 * 8)
        .ok_or_else(|| invalid_field_err!("dimensions", "rounded width overflow"))?;
    let rounded_height = height
        .checked_add(1)
        .map(|value| value / 2 * 2)
        .ok_or_else(|| invalid_field_err!("dimensions", "rounded height overflow"))?;

    let mut original_sizes = [pixel_count; PLANE_COUNT];
    if chroma_subsampling != 0 {
        original_sizes[0] = rounded_width
            .checked_mul(height)
            .ok_or_else(|| invalid_field_err!("dimensions", "Y plane size overflow"))?;
        let chroma_size = (rounded_width / 2)
            .checked_mul(rounded_height / 2)
            .ok_or_else(|| invalid_field_err!("dimensions", "chroma plane size overflow"))?;
        original_sizes[1] = chroma_size;
        original_sizes[2] = chroma_size;
    }

    let mut planes = Vec::with_capacity(PLANE_COUNT);
    for (encoded_size, original_size) in encoded_sizes.into_iter().zip(original_sizes) {
        let encoded = src.read_slice(encoded_size);
        planes.push(decode_plane(encoded, original_size)?);
    }

    let output_len = pixel_count
        .checked_mul(4)
        .ok_or_else(|| invalid_field_err!("dimensions", "BGRA output size overflow"))?;
    let mut output = vec![0u8; output_len];
    let shift = color_loss_level - 1;

    for y in 0..height {
        let y_row = if chroma_subsampling != 0 {
            y * rounded_width
        } else {
            y * width
        };
        let chroma_row = if chroma_subsampling != 0 {
            (y / 2) * (rounded_width / 2)
        } else {
            y * width
        };
        let alpha_row = y * width;

        for x in 0..width {
            let chroma_x = if chroma_subsampling != 0 { x / 2 } else { x };
            let luma = i16::from(planes[0][y_row + x]);
            let co = recover_chroma(planes[1][chroma_row + chroma_x], shift);
            let cg = recover_chroma(planes[2][chroma_row + chroma_x], shift);
            let alpha = planes[3][alpha_row + x];

            let red = luma + co - cg;
            let green = luma + cg;
            let blue = luma - co - cg;
            let dst = (y * width + x) * 4;
            output[dst] = clamp_u8(blue);
            output[dst + 1] = clamp_u8(green);
            output[dst + 2] = clamp_u8(red);
            output[dst + 3] = alpha;
        }
    }

    Ok(output)
}

fn decode_plane(encoded: &[u8], original_size: usize) -> DecodeResult<Vec<u8>> {
    if encoded.is_empty() {
        return Ok(vec![0xFF; original_size]);
    }

    if encoded.len() >= original_size {
        return Ok(encoded[..original_size].to_vec());
    }

    let mut output = Vec::with_capacity(original_size);
    let mut offset = 0usize;

    while original_size.saturating_sub(output.len()) > 4 {
        let value = *encoded
            .get(offset)
            .ok_or_else(|| invalid_field_err!("planeData", "truncated NSCodec RLE value"))?;
        offset += 1;

        let remaining = original_size - output.len();
        if remaining == 5 {
            output.push(value);
            continue;
        }

        if encoded.get(offset) == Some(&value) {
            offset += 1;
            let count = *encoded
                .get(offset)
                .ok_or_else(|| invalid_field_err!("planeData", "truncated NSCodec RLE count"))?;
            offset += 1;

            let run_length = if count < 0xFF {
                usize::from(count) + 2
            } else {
                let bytes = encoded.get(offset..offset + 4).ok_or_else(|| {
                    invalid_field_err!("planeData", "truncated NSCodec long RLE count")
                })?;
                offset += 4;
                usize::try_from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                    .map_err(|_| {
                        invalid_field_err!("planeData", "NSCodec RLE count does not fit usize")
                    })?
            };

            if run_length == 0 || run_length > remaining {
                return Err(invalid_field_err!(
                    "planeData",
                    "NSCodec RLE run exceeds plane size"
                ));
            }
            output.resize(output.len() + run_length, value);
        } else {
            output.push(value);
        }
    }

    let tail = encoded
        .get(offset..offset + 4)
        .ok_or_else(|| invalid_field_err!("planeData", "truncated NSCodec RLE tail"))?;
    output.extend_from_slice(tail);
    if output.len() != original_size {
        return Err(invalid_field_err!(
            "planeData",
            "NSCodec RLE output does not match plane size"
        ));
    }

    Ok(output)
}

fn recover_chroma(value: u8, shift: u8) -> i16 {
    let shifted = (u16::from(value) << shift).to_le_bytes()[0];
    i16::from(i8::from_ne_bytes([shifted]))
}

fn clamp_u8(value: i16) -> u8 {
    u8::try_from(value.clamp(0, 255)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_uncompressed_planes_without_subsampling() {
        let mut data = Vec::new();
        for _ in 0..4 {
            data.extend_from_slice(&2u32.to_le_bytes());
        }
        data.extend_from_slice(&[1, 0, 0, 0]);
        data.extend_from_slice(&[100, 100]);
        data.extend_from_slice(&[50, 0]);
        data.extend_from_slice(&[0, 50]);
        data.extend_from_slice(&[255, 255]);

        let decoded = decode(&data, 2, 1).unwrap();
        assert_eq!(
            decoded,
            [
                50, 100, 150, 255, // Y=100, Co=50, Cg=0
                50, 150, 50, 255, // Y=100, Co=0, Cg=50
            ]
        );
    }

    #[test]
    fn preserves_alpha_plane() {
        let mut data = Vec::new();
        for _ in 0..4 {
            data.extend_from_slice(&2u32.to_le_bytes());
        }
        data.extend_from_slice(&[1, 0, 0, 0]);
        data.extend_from_slice(&[100, 100]);
        data.extend_from_slice(&[50, 0]);
        data.extend_from_slice(&[0, 50]);
        data.extend_from_slice(&[0, 128]);

        let decoded = decode(&data, 2, 1).unwrap();
        assert_eq!(decoded[3], 0);
        assert_eq!(decoded[7], 128);
    }

    #[test]
    fn expands_subsampled_chroma_across_two_by_two_pixels() {
        let plane_sizes = [16u32, 4, 4, 6];
        let mut data = Vec::new();
        for size in plane_sizes {
            data.extend_from_slice(&size.to_le_bytes());
        }
        data.extend_from_slice(&[1, 1, 0, 0]);
        data.extend_from_slice(&[
            100, 100, 100, 0, 0, 0, 0, 0, // Y row 0, padded to 8
            100, 100, 100, 0, 0, 0, 0, 0, // Y row 1, padded to 8
        ]);
        data.extend_from_slice(&[50, 0, 0, 0]);
        data.extend_from_slice(&[0, 50, 0, 0]);
        data.extend_from_slice(&[255; 6]);

        let decoded = decode(&data, 3, 2).unwrap();
        let expected_row = [
            50, 100, 150, 255, // first chroma sample
            50, 100, 150, 255, // repeated horizontally
            50, 150, 50, 255, // second chroma sample
        ];
        assert_eq!(&decoded[..12], &expected_row);
        assert_eq!(&decoded[12..], &expected_row);
    }

    #[test]
    fn decodes_rle_plane_with_literal_tail() {
        let decoded = decode_plane(&[7, 7, 2, 7, 7, 7, 7], 8).unwrap();
        assert_eq!(decoded, vec![7; 8]);
    }
}
