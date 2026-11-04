//! EGFX capabilities. IronRDP owns codec decoding, surface state, and compositing.

use ironrdp_egfx::client::GraphicsPipelineHandler;
use ironrdp_egfx::pdu::{CapabilitiesV81Flags, CapabilitiesV8Flags, CapabilitySet};

pub struct GfxHandler;

impl GraphicsPipelineHandler for GfxHandler {
    fn capabilities(&self) -> Vec<CapabilitySet> {
        // Keep the tested AVC420 negotiation; advertising AVC444 is a separate change.
        vec![
            CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::AVC420_ENABLED | CapabilitiesV81Flags::SMALL_CACHE,
            },
            CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::SMALL_CACHE,
            },
        ]
    }

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
    use ironrdp_egfx::pdu::{
        CacheToSurfacePdu, CapabilitiesConfirmPdu, Codec1Type, CreateSurfacePdu, EndFramePdu,
        GfxPdu, MapSurfaceToOutputPdu, PixelFormat, Point, ResetGraphicsPdu, StartFramePdu,
        SurfaceToCachePdu, Timestamp, WireToSurface1Pdu,
    };
    use ironrdp_graphics::clearcodec::ClearCodecEncoder;
    use ironrdp_graphics::image_processing::PixelFormat as ImageFormat;
    use ironrdp_pdu::geometry::ExclusiveRectangle;
    use ironrdp_session::{composite_graphics_pipeline, image::DecodedImage};

    fn send(pipeline: &mut GraphicsPipelineClient, pdu: GfxPdu) {
        let bytes = encode_vec(&pdu).unwrap();
        let frame = ironrdp_graphics::zgfx::wrap_uncompressed(&bytes);
        pipeline.process(1, &frame).unwrap();
    }

    fn pipeline() -> (GraphicsPipelineClient, DecodedImage) {
        let mut pipeline = GraphicsPipelineClient::new(Box::new(GfxHandler), None);
        let caps = CapabilitySet::V8 {
            flags: CapabilitiesV8Flags::SMALL_CACHE,
        };
        send(
            &mut pipeline,
            GfxPdu::CapabilitiesConfirm(CapabilitiesConfirmPdu((&caps).into())),
        );
        send(
            &mut pipeline,
            GfxPdu::ResetGraphics(ResetGraphicsPdu {
                width: 4,
                height: 2,
                monitors: vec![],
            }),
        );
        send(
            &mut pipeline,
            GfxPdu::CreateSurface(CreateSurfacePdu {
                surface_id: 1,
                width: 4,
                height: 2,
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
        for pixel in image.data().chunks_exact(4) {
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
