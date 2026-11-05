//! EGFX capabilities. IronRDP owns codec decoding, surface state, and compositing.

use ironrdp_egfx::client::GraphicsPipelineHandler;
use ironrdp_egfx::decode::{DecoderResult, H264Decoder};
use ironrdp_egfx::pdu::CapabilitySet;

pub struct GfxHandler;

pub(crate) fn h264_decoder() -> DecoderResult<Box<dyn H264Decoder>> {
    #[cfg(target_os = "macos")]
    let decoder = crate::videotoolbox::VideoToolboxDecoder::new()?;
    #[cfg(not(target_os = "macos"))]
    let decoder = ironrdp_egfx::decode::OpenH264Decoder::new()?;
    Ok(Box::new(decoder))
}

impl GraphicsPipelineHandler for GfxHandler {
    fn on_capabilities_confirmed(&mut self, caps: &CapabilitySet) {
        tracing::info!("egfx: server confirmed capability set {caps:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_core::encode_vec;
    use ironrdp_dvc::DvcProcessor as _;
    use ironrdp_egfx::client::GraphicsPipelineClient;
    use ironrdp_egfx::decode::{H264Decoder, OpenH264Decoder};
    use ironrdp_egfx::encode::{EncodeFrame, H264Encoder, OpenH264Encoder};
    use ironrdp_egfx::pdu::{
        Avc420BitmapStream, Avc444BitmapStream, CacheToSurfacePdu, CapabilitiesConfirmPdu,
        CapabilitiesV107Flags, CapabilitiesV8Flags, Codec1Type, CreateSurfacePdu, Encoding,
        EndFramePdu, GfxPdu, MapSurfaceToOutputPdu, PixelFormat, Point, QuantQuality,
        ResetGraphicsPdu, StartFramePdu, SurfaceToCachePdu, Timestamp, WireToSurface1Pdu,
    };
    use ironrdp_graphics::clearcodec::ClearCodecEncoder;
    use ironrdp_graphics::image_processing::PixelFormat as ImageFormat;
    use ironrdp_pdu::geometry::ExclusiveRectangle;
    use ironrdp_session::{composite_graphics_pipeline, image::DecodedImage};

    fn advertised_capabilities(decoder: Option<Box<dyn H264Decoder>>) -> Vec<CapabilitySet> {
        let mut pipeline = GraphicsPipelineClient::new(Box::new(GfxHandler), decoder);
        let messages = pipeline.start(1).unwrap();
        assert_eq!(messages.len(), 1);
        let bytes = encode_vec(&*messages[0]).unwrap();
        let GfxPdu::CapabilitiesAdvertise(advertised) =
            ironrdp_core::decode::<GfxPdu>(&bytes).unwrap()
        else {
            panic!("expected graphics capabilities");
        };
        advertised
            .0
            .iter()
            .map(|cap| cap.parsed().unwrap().unwrap())
            .collect()
    }

    #[test]
    fn available_decoder_advertises_avc444_with_older_fallbacks() {
        let caps = advertised_capabilities(Some(Box::new(OpenH264Decoder::new().unwrap())));
        assert!(matches!(caps[0], CapabilitySet::V10_7 { .. }));
        assert!(caps
            .iter()
            .any(|cap| matches!(cap, CapabilitySet::V8_1 { .. })));
        assert!(caps
            .iter()
            .any(|cap| matches!(cap, CapabilitySet::V8 { .. })));
    }

    #[test]
    fn missing_decoder_advertises_only_non_avc_graphics() {
        let caps = advertised_capabilities(None);
        assert_eq!(caps.len(), 1);
        assert!(matches!(caps[0], CapabilitySet::V8 { .. }));
        assert_eq!(
            crate::RdpOptions::default().graphics,
            crate::GraphicsMode::Classic
        );
    }

    fn send(pipeline: &mut GraphicsPipelineClient, pdu: GfxPdu) {
        let bytes = encode_vec(&pdu).unwrap();
        let frame = ironrdp_graphics::zgfx::wrap_uncompressed(&bytes);
        pipeline.process(1, &frame).unwrap();
    }

    fn pipeline() -> (GraphicsPipelineClient, DecodedImage) {
        pipeline_with_decoder(4, 2, None)
    }

    fn pipeline_with_decoder(
        width: u16,
        height: u16,
        decoder: Option<Box<dyn H264Decoder>>,
    ) -> (GraphicsPipelineClient, DecodedImage) {
        let caps = if decoder.is_some() {
            CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::SMALL_CACHE,
            }
        } else {
            CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::SMALL_CACHE,
            }
        };
        let mut pipeline = GraphicsPipelineClient::new(Box::new(GfxHandler), decoder);
        send(
            &mut pipeline,
            GfxPdu::CapabilitiesConfirm(CapabilitiesConfirmPdu((&caps).into())),
        );
        send(
            &mut pipeline,
            GfxPdu::ResetGraphics(ResetGraphicsPdu {
                width: u32::from(width),
                height: u32::from(height),
                monitors: vec![],
            }),
        );
        send(
            &mut pipeline,
            GfxPdu::CreateSurface(CreateSurfacePdu {
                surface_id: 1,
                width,
                height,
                pixel_format: PixelFormat::XRgb,
            }),
        );
        send(
            &mut pipeline,
            GfxPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
                surface_id: 1,
                output_origin_x: 0,
                output_origin_y: 0,
            }),
        );
        (pipeline, DecodedImage::new(ImageFormat::BgrX32, 1, 1))
    }

    fn rectangle(left: u16, right: u16) -> ExclusiveRectangle {
        ExclusiveRectangle {
            left,
            top: 0,
            right,
            bottom: 2,
        }
    }

    fn encode_avc_view(encoder: &mut OpenH264Encoder, grey: u8) -> Vec<u8> {
        let pixels = [grey, grey, grey, 255].repeat(320 * 240);
        let encoded = encoder
            .encode(EncodeFrame {
                data: &pixels,
                width: 320,
                height: 240,
            })
            .unwrap();
        assert!(!encoded.is_empty());
        encoded
    }

    fn avc_stream<'a>(data: &'a [u8], rect: &ExclusiveRectangle) -> Avc420BitmapStream<'a> {
        Avc420BitmapStream {
            rectangles: vec![rect.clone()],
            quant_qual_vals: vec![QuantQuality {
                quantization_parameter: 22,
                progressive: false,
                quality: 100,
            }],
            data,
        }
    }

    #[test]
    fn real_avc444_decodes_both_layouts_and_preserves_pixels_outside_partial_updates() {
        check_avc444(|| Box::new(OpenH264Decoder::new().unwrap()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a local Mac with VideoToolbox hardware decoding"]
    fn videotoolbox_avc444_reaches_the_session_image() {
        check_avc444(|| {
            Box::new(
                crate::videotoolbox::VideoToolboxDecoder::new()
                    .unwrap()
                    .requiring_hardware(),
            )
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "local release-mode pipeline timing; requires VideoToolbox hardware"]
    fn videotoolbox_avc420_pipeline_matches_software_and_reports_time() {
        let full = ExclusiveRectangle {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let mut encoder = OpenH264Encoder::new().unwrap();
        let frames: Vec<_> = (0u8..24)
            .map(|index| {
                let pixels = [100 + index, 140, 180, 255].repeat(1920 * 1080);
                let mut data = vec![0; 4];
                data.extend(
                    encoder
                        .encode(EncodeFrame {
                            data: &pixels,
                            width: 1920,
                            height: 1080,
                        })
                        .unwrap(),
                );
                let frame_id = u32::from(index);
                let pdus = [
                    GfxPdu::StartFrame(StartFramePdu {
                        timestamp: Timestamp {
                            milliseconds: 0,
                            seconds: 0,
                            minutes: 0,
                            hours: 0,
                        },
                        frame_id,
                    }),
                    GfxPdu::WireToSurface1(WireToSurface1Pdu {
                        surface_id: 1,
                        codec_id: Codec1Type::Avc420,
                        pixel_format: PixelFormat::XRgb,
                        destination_rectangle: full.clone(),
                        bitmap_data: encode_vec(&avc_stream(&data, &full)).unwrap(),
                    }),
                    GfxPdu::EndFrame(EndFramePdu { frame_id }),
                ];
                let bytes: Vec<_> = pdus
                    .iter()
                    .flat_map(|pdu| encode_vec(pdu).unwrap())
                    .collect();
                ironrdp_graphics::zgfx::wrap_uncompressed(&bytes)
            })
            .collect();
        let hardware = crate::videotoolbox::VideoToolboxDecoder::new()
            .unwrap()
            .requiring_hardware();
        let mut expected = Vec::new();
        for (name, decoder) in [
            (
                "OpenH264",
                Box::new(OpenH264Decoder::new().unwrap()) as Box<dyn H264Decoder>,
            ),
            ("VideoToolbox", Box::new(hardware) as Box<dyn H264Decoder>),
        ] {
            let (mut pipeline, mut image) = pipeline_with_decoder(1920, 1080, Some(decoder));
            composite_graphics_pipeline(&mut pipeline, &mut image).unwrap();
            let mut elapsed = std::time::Duration::ZERO;
            for (index, frame) in frames.iter().enumerate() {
                let start = std::time::Instant::now();
                pipeline.process(1, frame).unwrap();
                let output = composite_graphics_pipeline(&mut pipeline, &mut image).unwrap();
                elapsed += start.elapsed();
                assert!(output.region.is_some(), "{name} frame {index}");
                assert_eq!(pipeline.dropped_updates(), 0);
                if name == "OpenH264" {
                    expected.push(image.data().to_vec());
                } else {
                    assert_eq!(image.data(), expected[index], "frame {index}");
                }
            }
            eprintln!("{name}: {elapsed:?} for 24 1080p AVC420 pipeline updates");
        }
    }

    fn check_avc444(decoder: fn() -> Box<dyn H264Decoder>) {
        for codec_id in [Codec1Type::Avc444, Codec1Type::Avc444v2] {
            let (mut pipeline, mut image) = pipeline_with_decoder(320, 240, Some(decoder()));
            let mut encoder = OpenH264Encoder::new().unwrap();
            let full = ExclusiveRectangle {
                left: 0,
                top: 0,
                right: 320,
                bottom: 240,
            };
            let partial = ExclusiveRectangle {
                left: 0,
                top: 0,
                right: 160,
                bottom: 240,
            };
            let main = encode_avc_view(&mut encoder, 160);
            let auxiliary = encode_avc_view(&mut encoder, 128);
            let initial = encode_vec(&Avc444BitmapStream {
                encoding: Encoding::LUMA_AND_CHROMA,
                stream1: avc_stream(&main, &full),
                stream2: Some(avc_stream(&auxiliary, &full)),
            })
            .unwrap();

            for (frame_id, encoding, grey) in [
                (1, Encoding::LUMA_AND_CHROMA, 0),
                (2, Encoding::LUMA, 64),
                (3, Encoding::CHROMA, 220),
            ] {
                let before = image.data().to_vec();
                let bitmap_data = if frame_id == 1 {
                    initial.clone()
                } else {
                    let data = encode_avc_view(&mut encoder, grey);
                    encode_vec(&Avc444BitmapStream {
                        encoding,
                        stream1: avc_stream(&data, &partial),
                        stream2: None,
                    })
                    .unwrap()
                };
                send(
                    &mut pipeline,
                    GfxPdu::StartFrame(StartFramePdu {
                        timestamp: Timestamp {
                            milliseconds: 0,
                            seconds: 0,
                            minutes: 0,
                            hours: 0,
                        },
                        frame_id,
                    }),
                );
                send(
                    &mut pipeline,
                    GfxPdu::WireToSurface1(WireToSurface1Pdu {
                        surface_id: 1,
                        codec_id,
                        pixel_format: PixelFormat::XRgb,
                        destination_rectangle: full.clone(),
                        bitmap_data,
                    }),
                );
                send(&mut pipeline, GfxPdu::EndFrame(EndFramePdu { frame_id }));
                let output = composite_graphics_pipeline(&mut pipeline, &mut image).unwrap();
                assert!(output.region.is_some(), "{codec_id:?} {encoding:?}");
                assert_eq!((image.width(), image.height()), (320, 240));
                assert_eq!(pipeline.dropped_updates(), 0);
                if frame_id == 1 {
                    assert!(image
                        .data()
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .all(|p| p[..3].iter().all(|value| (140..=180).contains(value))));
                } else {
                    let mut changed = false;
                    for (old, new) in before
                        .as_chunks::<1280>()
                        .0
                        .iter()
                        .zip(image.data().as_chunks::<1280>().0)
                    {
                        assert_eq!(&old[160 * 4..], &new[160 * 4..], "outside update rectangle");
                        changed |= old[..160 * 4] != new[..160 * 4];
                    }
                    assert!(changed, "{codec_id:?} {encoding:?} did not update pixels");
                }
            }
        }
    }

    #[test]
    fn clearcodec_and_cached_updates_reach_the_session_image() {
        let (mut pipeline, mut image) = pipeline();
        send(
            &mut pipeline,
            GfxPdu::StartFrame(StartFramePdu {
                timestamp: Timestamp {
                    milliseconds: 0,
                    seconds: 0,
                    minutes: 0,
                    hours: 0,
                },
                frame_id: 1,
            }),
        );
        let pixels = [0x11, 0x22, 0x33, 0xff].repeat(4);
        let mut encoder = ClearCodecEncoder::new();
        send(
            &mut pipeline,
            GfxPdu::WireToSurface1(WireToSurface1Pdu {
                surface_id: 1,
                codec_id: Codec1Type::ClearCodec,
                pixel_format: PixelFormat::XRgb,
                destination_rectangle: rectangle(0, 2),
                bitmap_data: encoder.encode(&pixels, 2, 2),
            }),
        );
        send(
            &mut pipeline,
            GfxPdu::SurfaceToCache(SurfaceToCachePdu {
                surface_id: 1,
                cache_key: 1,
                cache_slot: 1,
                source_rectangle: rectangle(0, 2),
            }),
        );
        send(
            &mut pipeline,
            GfxPdu::CacheToSurface(CacheToSurfacePdu {
                surface_id: 1,
                cache_slot: 1,
                destination_points: vec![Point { x: 2, y: 0 }],
            }),
        );
        send(&mut pipeline, GfxPdu::EndFrame(EndFramePdu { frame_id: 1 }));
        let output = composite_graphics_pipeline(&mut pipeline, &mut image).unwrap();
        assert!(output.reset);
        assert!(output.region.is_some());
        assert_eq!((image.width(), image.height()), (4, 2));
        for pixel in image.data().as_chunks::<4>().0 {
            assert_eq!(&pixel[..3], &[0x11, 0x22, 0x33]);
        }
        assert_eq!(pipeline.dropped_updates(), 0);
        assert!(composite_graphics_pipeline(&mut pipeline, &mut image)
            .unwrap()
            .region
            .is_none());
    }

    #[test]
    fn graphics_reset_updates_image_dimensions() {
        let (mut pipeline, mut image) = pipeline();
        composite_graphics_pipeline(&mut pipeline, &mut image).unwrap();
        send(
            &mut pipeline,
            GfxPdu::ResetGraphics(ResetGraphicsPdu {
                width: 8,
                height: 6,
                monitors: vec![],
            }),
        );
        assert!(
            composite_graphics_pipeline(&mut pipeline, &mut image)
                .unwrap()
                .reset
        );
        assert_eq!((image.width(), image.height()), (8, 6));
    }
}
