//! Synchronous VideoToolbox decoding with a reference-preserving OpenH264 fallback.

use std::collections::BTreeMap;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use ironrdp_egfx::decode::{
    DecodedFrame, DecoderError, DecoderResult, H264Decoder, OpenH264Decoder, Yuv420Frame,
};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, CMVideoFormatDescriptionGetDimensions,
};
use objc2_core_video::{
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVImageBuffer,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount,
    CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder, VTDecodeFrameFlags,
    VTDecodeInfoFlags, VTDecompressionSession,
};

const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_PARAMETER_BYTES: usize = 256 * 1024;
const MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;
const MAX_HISTORY_UNITS: usize = 512;

type ParameterSets = BTreeMap<(u8, u32), Vec<u8>>;

struct AccessUnit {
    bytes: Vec<u8>,
    has_picture: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum DecodeMode {
    Hardware,
    SoftwareUntilKeyframe,
    Software,
}

pub(crate) struct VideoToolboxDecoder {
    hardware: Option<HardwareSession>,
    software: OpenH264Decoder,
    mode: DecodeMode,
    parameters: ParameterSets,
    format_dirty: bool,
    // A fresh decoder needs all reference pictures, not just SPS/PPS. Switch to
    // software before either bound is exceeded rather than lose a safe fallback.
    history: Vec<AccessUnit>,
    history_bytes: usize,
    #[cfg(test)]
    hardware_required: bool,
}

impl VideoToolboxDecoder {
    pub(crate) fn new() -> DecoderResult<Self> {
        Ok(Self {
            hardware: None,
            software: OpenH264Decoder::new()?,
            mode: DecodeMode::Hardware,
            parameters: BTreeMap::new(),
            format_dirty: false,
            history: Vec::new(),
            history_bytes: 0,
            #[cfg(test)]
            hardware_required: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn requiring_hardware(mut self) -> Self {
        self.hardware_required = true;
        self
    }

    fn fall_back(
        &mut self,
        current: &[u8],
        cause: &str,
        mode: DecodeMode,
    ) -> DecoderResult<Yuv420Frame> {
        #[cfg(test)]
        assert!(!self.hardware_required, "hardware test fell back: {cause}");
        tracing::warn!(?mode, "egfx: {cause}; switching to OpenH264");
        self.mode = mode;
        self.hardware = None;
        if self.mode == DecodeMode::Software {
            self.parameters.clear();
        }
        self.history_bytes = 0;
        for unit in std::mem::take(&mut self.history) {
            let result = self.software.decode_yuv420(&unit.bytes);
            if unit.has_picture {
                result?;
            }
        }
        self.software.decode_yuv420(current)
    }
}

impl H264Decoder for VideoToolboxDecoder {
    fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame> {
        self.decode_yuv420(data).map(|picture| picture.to_rgba())
    }

    fn supports_yuv420(&self) -> bool {
        true
    }

    fn decode_yuv420(&mut self, data: &[u8]) -> DecoderResult<Yuv420Frame> {
        if self.mode == DecodeMode::Software {
            return self.software.decode_yuv420(data);
        }
        let units = nal_units(data)?;
        let has_picture = units.iter().any(|nal| matches!(nal[0] & 31, 1..=5));
        let key_frame = units.iter().any(|nal| nal[0] & 31 == 5);
        for nal in &units {
            let kind = nal[0] & 31;
            if matches!(kind, 7 | 8) {
                let id = parameter_id(nal)?;
                if self
                    .parameters
                    .get(&(kind, id))
                    .is_none_or(|old| old != nal)
                {
                    let bytes: usize = self.parameters.values().map(Vec::len).sum();
                    if bytes + nal.len() > MAX_PARAMETER_BYTES {
                        return self.fall_back(
                            data,
                            "H.264 parameter-set limit reached",
                            DecodeMode::Software,
                        );
                    }
                    self.parameters.insert((kind, id), nal.to_vec());
                    self.format_dirty = true;
                }
            }
        }
        let mut annex_b = Vec::new();
        if key_frame {
            if self.mode == DecodeMode::SoftwareUntilKeyframe {
                self.mode = DecodeMode::Hardware;
                self.software.reset();
            }
            self.history.clear();
            self.history_bytes = 0;
            for parameter in self.parameters.values() {
                append_annex_b(&mut annex_b, parameter);
            }
        }
        for nal in &units {
            append_annex_b(&mut annex_b, nal);
        }
        if self.mode == DecodeMode::SoftwareUntilKeyframe {
            return self.software.decode_yuv420(&annex_b);
        }
        if self.history.len() >= MAX_HISTORY_UNITS
            || self.history_bytes + annex_b.len() > MAX_HISTORY_BYTES
        {
            return self.fall_back(
                &annex_b,
                "H.264 reference-history limit reached",
                DecodeMode::SoftwareUntilKeyframe,
            );
        }
        if !has_picture {
            // Parameter-only units are not pictures. Preserve in-stream changes
            // for replay; pre-stream sets will be prepended to the first IDR.
            if !self.history.is_empty() {
                self.history_bytes += annex_b.len();
                self.history.push(AccessUnit {
                    bytes: annex_b,
                    has_picture,
                });
            }
            return Err(DecoderError::msg("H.264 access unit contains no picture"));
        }
        if self.history.is_empty() && !key_frame {
            return self.fall_back(
                &annex_b,
                "H.264 stream has no initial keyframe",
                DecodeMode::Software,
            );
        }
        if self.format_dirty || self.hardware.is_none() {
            let update = match &mut self.hardware {
                Some(session) => session.update_format(&self.parameters, key_frame),
                None => HardwareSession::new(&self.parameters)
                    .map(|session| self.hardware = Some(session)),
            };
            if let Err(error) = update {
                return self.fall_back(&annex_b, &error.to_string(), DecodeMode::Software);
            }
            self.format_dirty = false;
        }
        let mut sample = Vec::new();
        for nal in units {
            if !matches!(nal[0] & 31, 7..=9) {
                let len = u32::try_from(nal.len())
                    .map_err(|_| DecoderError::msg("H.264 NAL too large"))?;
                sample.extend_from_slice(&len.to_be_bytes());
                sample.extend_from_slice(nal);
            }
        }
        let result = self
            .hardware
            .as_ref()
            .ok_or_else(|| DecoderError::msg("VideoToolbox session unavailable"))?
            .decode(&sample)
            .and_then(NativePlanes::into_yuv420);
        match result {
            Ok(picture) => {
                self.history_bytes += annex_b.len();
                self.history.push(AccessUnit {
                    bytes: annex_b,
                    has_picture,
                });
                Ok(picture)
            }
            Err(error) => self.fall_back(&annex_b, &error.to_string(), DecodeMode::Software),
        }
    }

    fn reset(&mut self) {
        self.hardware = None;
        self.parameters.clear();
        self.format_dirty = false;
        self.history.clear();
        self.history_bytes = 0;
        self.software.reset();
        // A failed hardware backend stays disabled until the next connection.
        if self.mode == DecodeMode::SoftwareUntilKeyframe {
            self.mode = DecodeMode::Hardware;
        }
    }
}

struct HardwareSession {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMFormatDescription>,
}

// SAFETY: VideoToolbox permits sessions on any thread. H264Decoder requires
// Send, not Sync, and exclusive decoder access serializes every session call.
unsafe impl Send for HardwareSession {}

impl HardwareSession {
    fn new(parameters: &ParameterSets) -> DecoderResult<Self> {
        let format = format_description(parameters)?;
        // Require real hardware; use our tested OpenH264 backend otherwise.
        let key = unsafe { kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder };
        let specification =
            CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[CFBoolean::new(true)]);
        let mut session = ptr::null_mut();
        // SAFETY: valid retained format/dictionary and an initialized out pointer.
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                Some(specification.as_ref()),
                None,
                ptr::null(),
                NonNull::from(&mut session),
            )
        };
        let session = retained(status, session, "VTDecompressionSessionCreate")?;
        tracing::info!("egfx: H.264 hardware decoding with VideoToolbox");
        Ok(Self { session, format })
    }

    fn update_format(&mut self, parameters: &ParameterSets, key_frame: bool) -> DecoderResult<()> {
        let format = format_description(parameters)?;
        // SAFETY: both CoreFoundation objects are live for this call.
        if unsafe { self.session.can_accept_format_description(&format) } {
            self.format = format;
        } else if key_frame {
            *self = Self::new(parameters)?;
        } else {
            return Err(DecoderError::msg(
                "VideoToolbox format changed without a keyframe",
            ));
        }
        Ok(())
    }

    fn decode(&self, bytes: &[u8]) -> DecoderResult<NativePlanes> {
        let mut block = ptr::null_mut();
        // SAFETY: CoreMedia allocates and owns this buffer; it never borrows Rust
        // storage, including if the platform retains compressed data internally.
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                ptr::null_mut(),
                bytes.len(),
                None,
                ptr::null(),
                0,
                bytes.len(),
                0,
                NonNull::from(&mut block),
            )
        };
        let block = retained(status, block, "CMBlockBufferCreateWithMemoryBlock")?;
        // SAFETY: input covers bytes.len(), and the block has that exact capacity.
        let status = unsafe {
            CMBlockBuffer::replace_data_bytes(NonNull::from(bytes).cast(), &block, 0, bytes.len())
        };
        check_status(status, "CMBlockBufferReplaceDataBytes")?;
        let mut sample = ptr::null_mut();
        let size = bytes.len();
        // SAFETY: one complete sample in the live block, with one size entry.
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(&self.format),
                1,
                0,
                ptr::null(),
                1,
                &size,
                NonNull::from(&mut sample),
            )
        };
        let sample = retained(status, sample, "CMSampleBufferCreateReady")?;
        let output = Arc::new(Mutex::new(None));
        let callback_output = Arc::clone(&output);
        let callback = RcBlock::new(
            move |status: i32,
                  _flags: VTDecodeInfoFlags,
                  image: *mut CVImageBuffer,
                  _pts: CMTime,
                  _duration: CMTime| {
                let result = check_status(status, "VideoToolbox output").and_then(|()| {
                    // SAFETY: VideoToolbox owns the image and keeps it live during
                    // the callback. Copy planes here; no native pointer escapes.
                    let image = unsafe { image.as_ref() }
                        .ok_or_else(|| DecoderError::msg("VideoToolbox returned no picture"))?;
                    copy_nv12(image)
                });
                *callback_output.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            },
        );
        // SAFETY: both asynchronous and temporal processing flags are clear.
        // Apple's contract completes the callback before decode returns, so
        // AVC444 always receives this sub-stream's picture, never a prior one.
        let status = unsafe {
            self.session.decode_frame_with_output_handler(
                &sample,
                VTDecodeFrameFlags(0),
                ptr::null_mut(),
                RcBlock::as_ptr(&callback),
            )
        };
        check_status(status, "VTDecompressionSessionDecodeFrame")?;
        let result = output.lock().unwrap_or_else(|e| e.into_inner()).take();
        result.unwrap_or_else(|| Err(DecoderError::msg("VideoToolbox did not deliver a picture")))
    }
}

impl Drop for HardwareSession {
    fn drop(&mut self) {
        // SAFETY: synchronous decoding has completed; invalidate before release.
        unsafe { self.session.invalidate() };
    }
}

fn check_status(status: i32, call: &str) -> DecoderResult<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(DecoderError::msg(format!("{call} failed: {status}")))
    }
}

fn retained<T: objc2_core_foundation::Type>(
    status: i32,
    object: *mut T,
    call: &str,
) -> DecoderResult<CFRetained<T>> {
    check_status(status, call)?;
    // SAFETY: a successful Create call returns a +1 owned reference.
    let value = NonNull::new(object).map(|p| unsafe { CFRetained::from_raw(p) });
    value.ok_or_else(|| DecoderError::msg(format!("{call} returned no object")))
}

fn format_description(
    parameters: &ParameterSets,
) -> DecoderResult<CFRetained<CMFormatDescription>> {
    if !parameters.keys().any(|(kind, _)| *kind == 7)
        || !parameters.keys().any(|(kind, _)| *kind == 8)
    {
        return Err(DecoderError::msg("H.264 SPS/PPS not received"));
    }
    let pointers: Vec<_> = parameters
        .values()
        .map(|p| NonNull::new(p.as_ptr().cast_mut()).expect("nonempty parameter set"))
        .collect();
    let sizes: Vec<_> = parameters.values().map(Vec::len).collect();
    let mut format = ptr::null();
    // SAFETY: equally sized, nonempty arrays point to validated, live NALs.
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            pointers.len(),
            NonNull::from(&pointers[0]),
            NonNull::from(&sizes[0]),
            4,
            NonNull::from(&mut format),
        )
    };
    let format = retained(
        status,
        format.cast_mut(),
        "CMVideoFormatDescriptionCreateFromH264ParameterSets",
    )?;
    // SAFETY: CreateFromH264ParameterSets returned a video format description.
    let dimensions = unsafe { CMVideoFormatDescriptionGetDimensions(&format) };
    if dimensions.width <= 0
        || dimensions.height <= 0
        || dimensions.width > 16384
        || dimensions.height > 16384
        || i64::from(dimensions.width) * i64::from(dimensions.height) > 64 * 1024 * 1024
    {
        return Err(DecoderError::msg("VideoToolbox format exceeds size limit"));
    }
    Ok(format)
}

fn append_annex_b(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

fn nal_units(data: &[u8]) -> DecoderResult<Vec<&[u8]>> {
    if data.is_empty() || data.len() > MAX_INPUT_BYTES {
        return Err(DecoderError::msg("invalid H.264 access-unit size"));
    }
    let mut units = Vec::new();
    let leading_zeros = data.iter().take_while(|&&byte| byte == 0).count();
    if leading_zeros >= 2 && data.get(leading_zeros) == Some(&1) {
        let mut start = None;
        let mut zeros = 0;
        for (index, &byte) in data.iter().enumerate() {
            if byte == 1 && zeros >= 2 {
                if let Some(start) = start {
                    units.push(&data[start..index - zeros]);
                }
                start = Some(index + 1);
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            if units.len() > 4096 {
                return Err(DecoderError::msg("too many H.264 NAL units"));
            }
        }
        if let Some(start) = start {
            units.push(&data[start..data.len() - zeros]);
        }
    } else {
        let mut remaining = data;
        while !remaining.is_empty() {
            let header = remaining
                .get(..4)
                .ok_or_else(|| DecoderError::msg("truncated H.264 length"))?;
            let len = usize::try_from(u32::from_be_bytes(header.try_into().unwrap()))
                .map_err(|_| DecoderError::msg("H.264 NAL too large"))?;
            remaining = &remaining[4..];
            let nal = remaining
                .get(..len)
                .ok_or_else(|| DecoderError::msg("truncated H.264 NAL"))?;
            units.push(nal);
            remaining = &remaining[len..];
            if units.len() > 4096 {
                return Err(DecoderError::msg("too many H.264 NAL units"));
            }
        }
    }
    if units.is_empty()
        || units
            .iter()
            .any(|nal| nal.is_empty() || nal[0] & 0x80 != 0 || !(1..=23).contains(&(nal[0] & 31)))
    {
        return Err(DecoderError::msg("invalid H.264 NAL header"));
    }
    Ok(units)
}

fn parameter_id(nal: &[u8]) -> DecoderResult<u32> {
    if nal.len() > MAX_PARAMETER_BYTES {
        return Err(DecoderError::msg("H.264 parameter set too large"));
    }
    let mut rbsp = Vec::new();
    let mut zeros = 0;
    for &byte in &nal[1..] {
        if zeros == 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        rbsp.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    let kind = nal[0] & 31;
    let mut bit = if kind == 7 { 24 } else { 0 };
    let mut next_bit = || {
        let value = rbsp.get(bit / 8).map(|byte| (byte >> (7 - bit % 8)) & 1);
        bit += 1;
        value.ok_or_else(|| DecoderError::msg("truncated H.264 parameter ID"))
    };
    let mut leading = 0;
    while next_bit()? == 0 {
        leading += 1;
        if leading > 8 {
            return Err(DecoderError::msg("H.264 parameter ID out of range"));
        }
    }
    let mut id = 1u32;
    for _ in 0..leading {
        id = (id << 1) | u32::from(next_bit()?);
    }
    id -= 1;
    if id > if kind == 7 { 31 } else { 255 } {
        return Err(DecoderError::msg("H.264 parameter ID out of range"));
    }
    Ok(id)
}

struct NativePlanes {
    width: u16,
    height: u16,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl NativePlanes {
    fn into_yuv420(self) -> DecoderResult<Yuv420Frame> {
        Yuv420Frame::new(
            self.width,
            self.height,
            self.y,
            usize::from(self.width),
            self.u,
            self.v,
            usize::from(self.width).div_ceil(2),
        )
    }
}

fn copy_nv12(image: &CVImageBuffer) -> DecoderResult<NativePlanes> {
    let format = CVPixelBufferGetPixelFormatType(image);
    if ![
        kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    ]
    .contains(&format)
        || CVPixelBufferGetPlaneCount(image) != 2
    {
        return Err(DecoderError::msg("VideoToolbox output is not NV12"));
    }
    let (width, height) = (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image));
    // Match IronRDP's surface dimension limit and bound native plane copies.
    if width == 0
        || height == 0
        || width > 16384
        || height > 16384
        || width * height > 64 * 1024 * 1024
    {
        return Err(DecoderError::msg("VideoToolbox picture exceeds size limit"));
    }
    // SAFETY: the image is live for the callback and unlocked on every return.
    check_status(
        unsafe { CVPixelBufferLockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) },
        "CVPixelBufferLockBaseAddress",
    )?;
    struct Unlock<'a>(&'a CVImageBuffer);
    impl Drop for Unlock<'_> {
        fn drop(&mut self) {
            // SAFETY: paired with the successful read-only lock above.
            unsafe { CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly) };
        }
    }
    let _unlock = Unlock(image);
    let copy_plane = |plane, columns: usize, rows: usize| -> DecoderResult<Vec<u8>> {
        let stride = CVPixelBufferGetBytesPerRowOfPlane(image, plane);
        let plane_width = CVPixelBufferGetWidthOfPlane(image, plane);
        let plane_height = CVPixelBufferGetHeightOfPlane(image, plane);
        let pointer = CVPixelBufferGetBaseAddressOfPlane(image, plane).cast::<u8>();
        let pixels = if plane == 0 { columns } else { columns / 2 };
        if pointer.is_null()
            || stride < columns
            || plane_width < pixels
            || plane_height < rows
            || stride
                .checked_mul(plane_height)
                .is_none_or(|size| size > isize::MAX as usize)
        {
            return Err(DecoderError::msg("invalid VideoToolbox plane layout"));
        }
        let mut data = Vec::with_capacity(columns * rows);
        for row in 0..rows {
            // SAFETY: locked native plane, validated row count/stride/width.
            // Copy only active pixels, never allocation padding after a row.
            data.extend_from_slice(unsafe {
                std::slice::from_raw_parts(pointer.add(row * stride), columns)
            });
        }
        Ok(data)
    };
    let y = copy_plane(0, width, height)?;
    let chroma_width = width.div_ceil(2);
    let uv = copy_plane(1, chroma_width * 2, height.div_ceil(2))?;
    let mut u = Vec::with_capacity(uv.len() / 2);
    let mut v = Vec::with_capacity(uv.len() / 2);
    for pair in uv.as_chunks::<2>().0 {
        u.push(pair[0]);
        v.push(pair[1]);
    }
    // Preserve native sample values. RDP uses full-range BT.709 even when the
    // H.264 VUI labels the buffer as limited-range; do not ask VT to rescale it.
    Ok(NativePlanes {
        width: u16::try_from(width).unwrap(),
        height: u16::try_from(height).unwrap(),
        y,
        u,
        v,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_egfx::encode::{EncodeFrame, H264Encoder, OpenH264Encoder};

    fn frames(width: u32, height: u32, count: usize) -> Vec<Vec<u8>> {
        let mut encoder = OpenH264Encoder::new().unwrap();
        (0..count)
            .map(|n| {
                let pixels: Vec<_> = (0..width * height)
                    .flat_map(|i| {
                        [
                            u8::try_from((i % width + u32::try_from(n).unwrap() * 17) % 256)
                                .unwrap(),
                            u8::try_from((i / width * 3) % 256).unwrap(),
                            80,
                            255,
                        ]
                    })
                    .collect();
                let data = encoder
                    .encode(EncodeFrame {
                        data: &pixels,
                        width,
                        height,
                    })
                    .unwrap();
                assert!(!data.is_empty());
                data
            })
            .collect()
    }

    fn assert_picture_eq(got: &Yuv420Frame, expected: &Yuv420Frame) {
        assert_eq!(
            (got.width(), got.height()),
            (expected.width(), expected.height())
        );
        for row in 0..usize::from(got.height()) {
            assert_eq!(got.luma_row(row), expected.luma_row(row), "luma row {row}");
        }
        for row in 0..usize::from(got.height()).div_ceil(2) {
            assert_eq!(got.u_row(row), expected.u_row(row), "U row {row}");
            assert_eq!(got.v_row(row), expected.v_row(row), "V row {row}");
        }
    }

    #[test]
    fn parses_annex_b_and_length_prefixes_without_accepting_truncation() {
        let annex = [0, 0, 0, 1, 0x09, 0x10, 0, 0, 1, 0x65, 0xab, 0, 0];
        let prefixed = [0, 0, 0, 2, 0x09, 0x10, 0, 0, 0, 2, 0x65, 0xab];
        assert_eq!(nal_units(&annex).unwrap(), nal_units(&prefixed).unwrap());
        for invalid in [
            &[][..],
            &[0, 0, 1][..],
            &[0, 0, 0, 9, 0x65][..],
            &[0, 0, 0, 0][..],
            &[0, 0, 1, 0x80][..],
        ] {
            assert!(nal_units(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(parameter_id(&[0x67, 66, 0, 30, 0x80]).unwrap(), 0);
        assert_eq!(parameter_id(&[0x68, 0x40]).unwrap(), 1);
        assert_eq!(parameter_id(&[0x68, 0x20]).unwrap(), 3);
        assert!(parameter_id(&[0x67, 66, 0, 30]).is_err());
    }

    #[test]
    fn parses_annex_b_with_leading_zero_bytes() {
        for zeros in [1, 2, 4] {
            let mut data = vec![0; zeros];
            data.extend_from_slice(&[0, 0, 0, 1, 0x65, 0xab]);
            assert_eq!(nal_units(&data).unwrap(), vec![&[0x65, 0xab][..]]);
        }
    }

    #[test]
    fn software_fallback_replays_reference_frames_and_releases_history() {
        let input = frames(64, 48, 8);
        assert!(!nal_units(&input[4])
            .unwrap()
            .iter()
            .any(|nal| nal[0] & 31 == 5));
        let mut decoder = VideoToolboxDecoder::new().unwrap();
        let mut reference = OpenH264Decoder::new().unwrap();
        for data in &input[..4] {
            reference.decode_yuv420(data).unwrap();
            decoder.history_bytes += data.len();
            decoder.history.push(AccessUnit {
                bytes: data.clone(),
                has_picture: true,
            });
        }
        let got = decoder
            .fall_back(&input[4], "test failure", DecodeMode::Software)
            .unwrap();
        assert_picture_eq(&got, &reference.decode_yuv420(&input[4]).unwrap());
        assert_eq!(decoder.mode, DecodeMode::Software);
        assert!(decoder.history.is_empty());
        assert_eq!(decoder.history_bytes, 0);
        for data in &input[5..] {
            assert_picture_eq(
                &decoder.decode_yuv420(data).unwrap(),
                &reference.decode_yuv420(data).unwrap(),
            );
        }
        decoder.reset();
        assert_eq!(decoder.mode, DecodeMode::Software);
        reference.reset();
        assert_picture_eq(
            &decoder.decode_yuv420(&input[0]).unwrap(),
            &reference.decode_yuv420(&input[0]).unwrap(),
        );
    }

    #[test]
    fn history_limit_switches_before_losing_references() {
        let input = frames(64, 48, 2);
        let mut decoder = VideoToolboxDecoder::new().unwrap();
        decoder.history.push(AccessUnit {
            bytes: input[0].clone(),
            has_picture: true,
        });
        decoder.history_bytes = MAX_HISTORY_BYTES;
        let mut reference = OpenH264Decoder::new().unwrap();
        reference.decode_yuv420(&input[0]).unwrap();
        assert_picture_eq(
            &decoder.decode_yuv420(&input[1]).unwrap(),
            &reference.decode_yuv420(&input[1]).unwrap(),
        );
        assert_eq!(decoder.mode, DecodeMode::SoftwareUntilKeyframe);
        assert!(decoder.history.is_empty());
        decoder.reset();
        assert_eq!(decoder.mode, DecodeMode::Hardware);
    }

    #[test]
    #[ignore = "requires a local Mac with VideoToolbox hardware decoding"]
    fn history_limit_resumes_hardware_at_next_keyframe() {
        let input = frames(320, 240, 4);
        let mut decoder = VideoToolboxDecoder::new().unwrap();
        let mut reference = OpenH264Decoder::new().unwrap();
        assert_picture_eq(
            &decoder.decode_yuv420(&input[0]).unwrap(),
            &reference.decode_yuv420(&input[0]).unwrap(),
        );
        assert!(decoder.hardware.is_some());
        decoder.history_bytes = MAX_HISTORY_BYTES;
        for data in &input[1..] {
            assert_picture_eq(
                &decoder.decode_yuv420(data).unwrap(),
                &reference.decode_yuv420(data).unwrap(),
            );
            assert!(decoder.hardware.is_none());
            assert!(decoder.history.is_empty());
        }
        reference.reset();
        for (index, data) in input.iter().enumerate() {
            let mut picture = Vec::new();
            for nal in nal_units(data).unwrap() {
                if !matches!(nal[0] & 31, 7 | 8) {
                    append_annex_b(&mut picture, nal);
                }
            }
            assert_picture_eq(
                &decoder.decode_yuv420(&picture).unwrap(),
                &reference.decode_yuv420(data).unwrap(),
            );
            assert!(
                decoder.hardware.is_some(),
                "hardware must resume at the keyframe"
            );
            if index == 0 {
                assert_eq!(decoder.history.len(), 1);
            }
        }
    }

    #[test]
    #[ignore = "requires a local Mac with VideoToolbox hardware decoding"]
    fn hardware_matches_openh264_for_consecutive_pictures_and_rgba() {
        let input = frames(320, 240, 8);
        let mut decoder = VideoToolboxDecoder::new().unwrap();
        let mut reference = OpenH264Decoder::new().unwrap();
        for data in &input {
            assert_picture_eq(
                &decoder.decode_yuv420(data).unwrap(),
                &reference.decode_yuv420(data).unwrap(),
            );
            assert_eq!(decoder.mode, DecodeMode::Hardware);
            assert!(decoder.hardware.is_some());
        }
        decoder.reset();
        reference.reset();
        for data in &input {
            let got = decoder.decode(data).unwrap();
            let expected = reference.decode_yuv420(data).unwrap().to_rgba();
            assert_eq!(got.data(), expected.data());
            assert_eq!(decoder.mode, DecodeMode::Hardware);
        }
    }

    #[test]
    #[ignore = "requires a local Mac with VideoToolbox hardware decoding"]
    fn invalidated_hardware_session_falls_back_on_an_interframe() {
        let input = frames(320, 240, 8);
        let mut decoder = VideoToolboxDecoder::new().unwrap();
        let mut reference = OpenH264Decoder::new().unwrap();
        for data in &input[..4] {
            assert_picture_eq(
                &decoder.decode_yuv420(data).unwrap(),
                &reference.decode_yuv420(data).unwrap(),
            );
            assert_eq!(decoder.mode, DecodeMode::Hardware);
        }
        // SAFETY: deliberately simulate a native session invalidated by the OS.
        unsafe { decoder.hardware.as_ref().unwrap().session.invalidate() };
        for data in &input[4..] {
            assert_picture_eq(
                &decoder.decode_yuv420(data).unwrap(),
                &reference.decode_yuv420(data).unwrap(),
            );
            assert_eq!(decoder.mode, DecodeMode::Software);
        }
    }

    #[test]
    #[ignore = "requires a local Mac with VideoToolbox hardware decoding"]
    fn hardware_handles_parameter_only_units_resize_and_reset() {
        let mut decoder = VideoToolboxDecoder::new().unwrap();
        for (width, height) in [(320, 240), (640, 360)] {
            let input = frames(width, height, 3);
            let mut reference = OpenH264Decoder::new().unwrap();
            let mut parameters = Vec::new();
            let mut picture = Vec::new();
            for nal in nal_units(&input[0]).unwrap() {
                append_annex_b(
                    if matches!(nal[0] & 31, 7 | 8) {
                        &mut parameters
                    } else {
                        &mut picture
                    },
                    nal,
                );
            }
            assert!(decoder.decode_yuv420(&parameters).is_err());
            assert_eq!(decoder.mode, DecodeMode::Hardware);
            assert_picture_eq(
                &decoder.decode_yuv420(&picture).unwrap(),
                &reference.decode_yuv420(&input[0]).unwrap(),
            );
            for data in &input[1..] {
                let mut prefixed = Vec::new();
                for nal in nal_units(data).unwrap() {
                    prefixed.extend_from_slice(&u32::try_from(nal.len()).unwrap().to_be_bytes());
                    prefixed.extend_from_slice(nal);
                }
                assert_picture_eq(
                    &decoder.decode_yuv420(&prefixed).unwrap(),
                    &reference.decode_yuv420(data).unwrap(),
                );
            }
            assert_eq!(decoder.mode, DecodeMode::Hardware);
        }
        decoder.reset();
        assert!(decoder.hardware.is_none());
        assert!(decoder.history.is_empty());
        let input = frames(320, 240, 1);
        decoder.decode_yuv420(&input[0]).unwrap();
        assert_eq!(decoder.mode, DecodeMode::Hardware);
    }

    #[test]
    #[ignore = "requires a local Mac with VideoToolbox hardware decoding"]
    fn long_stream_resumes_hardware_without_losing_reference_pictures() {
        let mut encoder = OpenH264Encoder::new().unwrap();
        let mut decoder = VideoToolboxDecoder::new().unwrap();
        let mut reference = OpenH264Decoder::new().unwrap();
        let pixels = [100, 140, 180, 255].repeat(320 * 240);
        let resume_at = MAX_HISTORY_UNITS + 4;
        for index in 0..resume_at * 2 + 4 {
            let phase = index % resume_at;
            if phase == 0 {
                encoder.request_key_frame();
            }
            let data = encoder
                .encode(EncodeFrame {
                    data: &pixels,
                    width: 320,
                    height: 240,
                })
                .unwrap();
            assert_eq!(
                nal_units(&data).unwrap().iter().any(|nal| nal[0] & 31 == 5),
                phase == 0,
                "unexpected keyframe at {index}",
            );
            assert_picture_eq(
                &decoder.decode_yuv420(&data).unwrap(),
                &reference.decode_yuv420(&data).unwrap(),
            );
            if phase >= MAX_HISTORY_UNITS {
                assert_eq!(decoder.mode, DecodeMode::SoftwareUntilKeyframe);
                assert!(decoder.history.is_empty());
                assert_eq!(decoder.history_bytes, 0);
            } else {
                assert_eq!(decoder.mode, DecodeMode::Hardware);
                assert!(decoder.hardware.is_some());
                assert!(decoder.history.len() <= MAX_HISTORY_UNITS);
                assert!(decoder.history_bytes <= MAX_HISTORY_BYTES);
            }
        }
    }
}
