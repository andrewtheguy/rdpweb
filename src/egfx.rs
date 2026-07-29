use std::collections::BTreeMap;
use std::time::Instant;

use ironrdp::core::{Decode as _, ReadCursor, impl_as_any};
use ironrdp::dvc::{DvcClientProcessor, DvcMessage, DvcProcessor};
use ironrdp::graphics::clearcodec::ClearCodecDecoder;
use ironrdp::graphics::progressive::ProgressiveDecoder;
use ironrdp::graphics::zgfx;
use ironrdp::pdu::geometry::Rectangle as _;
use ironrdp::pdu::{PduResult, decode_cursor, decode_err};
use ironrdp_egfx::CHANNEL_NAME;
use ironrdp_egfx::pdu::{
    Avc420BitmapStream, CapabilitiesAdvertisePdu, CapabilitiesV81Flags, CapabilitySet, Codec1Type,
    FrameAcknowledgePdu, GfxPdu, QueueDepth,
};
use log::{debug, warn};
use tokio::sync::mpsc;

use crate::protocol::{ControlMsg, GatewayEvent, VideoCodec, VideoPacket};

const MAX_RETAINED_DECOMPRESSED_CAPACITY: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Default)]
struct SurfaceInfo {
    width: u16,
    height: u16,
    output_x: u32,
    output_y: u32,
}

#[derive(Clone, Copy)]
struct BitmapRegion {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

pub struct EgfxPassthrough {
    tx: mpsc::UnboundedSender<GatewayEvent>,
    decompressor: zgfx::Decompressor,
    clearcodec: ClearCodecDecoder,
    progressive: ProgressiveDecoder,
    decompressed: Vec<u8>,
    surfaces: BTreeMap<u16, SurfaceInfo>,
    queued_frames: u32,
    total_frames: u32,
    clock: Instant,
    last_timestamp_us: u64,
    warned_clearcodec_error: bool,
    warned_progressive_error: bool,
    warned_unsupported_codec: bool,
}

impl EgfxPassthrough {
    pub fn new(tx: mpsc::UnboundedSender<GatewayEvent>) -> Self {
        Self {
            tx,
            decompressor: zgfx::Decompressor::new(),
            clearcodec: ClearCodecDecoder::new(),
            progressive: ProgressiveDecoder::new(),
            decompressed: Vec::new(),
            surfaces: BTreeMap::new(),
            queued_frames: 0,
            total_frames: 0,
            clock: Instant::now(),
            last_timestamp_us: 0,
            warned_clearcodec_error: false,
            warned_progressive_error: false,
            warned_unsupported_codec: false,
        }
    }

    fn control(&self, message: ControlMsg) {
        let _ = self.tx.send(GatewayEvent::Control(message));
    }

    fn next_timestamp_us(&mut self) -> u64 {
        let elapsed = u64::try_from(self.clock.elapsed().as_micros()).unwrap_or(u64::MAX);
        let timestamp_us = elapsed.max(self.last_timestamp_us.saturating_add(1));
        self.last_timestamp_us = timestamp_us;
        timestamp_us
    }

    fn send_bitmap(
        &mut self,
        codec: VideoCodec,
        surface_id: u16,
        region: BitmapRegion,
        data: Vec<u8>,
    ) {
        let surface = self.surfaces.get(&surface_id).copied().unwrap_or_default();
        let packet = VideoPacket {
            codec,
            key_frame: false,
            timestamp_us: self.next_timestamp_us(),
            surface_id,
            x: region.x,
            y: region.y,
            width: region.width,
            height: region.height,
            output_x: surface.output_x,
            output_y: surface.output_y,
            data,
        };
        let _ = self.tx.send(GatewayEvent::Video(packet));
    }

    fn handle_pdu(&mut self, pdu: GfxPdu) -> PduResult<Vec<DvcMessage>> {
        match pdu {
            GfxPdu::CapabilitiesConfirm(confirm) => {
                let capability = match confirm.0.parsed() {
                    Ok(Some(CapabilitySet::V8_1 { .. })) => "V8.1".to_owned(),
                    Ok(Some(capability)) => format!("{:?}", capability.version()),
                    Ok(None) => format!("unknown 0x{:x}", confirm.0.version.0),
                    Err(error) => format!("malformed ({error})"),
                };
                self.control(ControlMsg::EgfxNegotiated { capability });
                Ok(Vec::new())
            }
            GfxPdu::ResetGraphics(reset) => {
                self.surfaces.clear();
                self.clearcodec = ClearCodecDecoder::new();
                self.progressive.reset();
                self.warned_clearcodec_error = false;
                self.warned_progressive_error = false;
                self.control(ControlMsg::Resize {
                    width: reset.width,
                    height: reset.height,
                });
                Ok(Vec::new())
            }
            GfxPdu::CreateSurface(surface) => {
                self.surfaces.insert(
                    surface.surface_id,
                    SurfaceInfo {
                        width: surface.width,
                        height: surface.height,
                        ..SurfaceInfo::default()
                    },
                );
                Ok(Vec::new())
            }
            GfxPdu::DeleteSurface(surface) => {
                self.surfaces.remove(&surface.surface_id);
                Ok(Vec::new())
            }
            GfxPdu::MapSurfaceToOutput(mapping) => {
                let surface = self.surfaces.entry(mapping.surface_id).or_default();
                surface.output_x = mapping.output_origin_x;
                surface.output_y = mapping.output_origin_y;
                Ok(Vec::new())
            }
            GfxPdu::StartFrame(_) => {
                self.queued_frames = self.queued_frames.saturating_add(1);
                Ok(Vec::new())
            }
            GfxPdu::WireToSurface1(wire) => {
                if wire.codec_id == Codec1Type::ClearCodec {
                    let width = wire.destination_rectangle.width();
                    let height = wire.destination_rectangle.height();
                    let mut pixels = match self
                        .clearcodec
                        .decode(&wire.bitmap_data, width, height)
                    {
                        Ok(pixels) => pixels,
                        Err(error) => {
                            if !self.warned_clearcodec_error {
                                self.warned_clearcodec_error = true;
                                warn!(
                                    "could not decode ClearCodec update; continuing with later EGFX updates: {error}"
                                );
                            }
                            return Ok(Vec::new());
                        }
                    };
                    bgra_to_rgba(&mut pixels);
                    self.send_bitmap(
                        VideoCodec::RgbaClearCodec,
                        wire.surface_id,
                        BitmapRegion {
                            x: wire.destination_rectangle.left,
                            y: wire.destination_rectangle.top,
                            width,
                            height,
                        },
                        pixels,
                    );
                    return Ok(Vec::new());
                }

                if wire.codec_id != Codec1Type::Avc420 {
                    if !self.warned_unsupported_codec {
                        self.warned_unsupported_codec = true;
                        self.control(ControlMsg::Warning {
                            message: format!(
                                "server sent unsupported EGFX codec {:?}",
                                wire.codec_id
                            ),
                        });
                    }
                    return Ok(Vec::new());
                }

                let mut cursor = ReadCursor::new(&wire.bitmap_data);
                let stream =
                    Avc420BitmapStream::decode(&mut cursor).map_err(|error| decode_err!(error))?;
                let rectangle = wire.destination_rectangle;
                let width = rectangle.right.saturating_sub(rectangle.left);
                let height = rectangle.bottom.saturating_sub(rectangle.top);
                let surface = self
                    .surfaces
                    .get(&wire.surface_id)
                    .copied()
                    .unwrap_or_default();

                let packet = VideoPacket {
                    codec: VideoCodec::Avc420,
                    key_frame: contains_idr(stream.data),
                    timestamp_us: self.next_timestamp_us(),
                    surface_id: wire.surface_id,
                    x: rectangle.left,
                    y: rectangle.top,
                    width,
                    height,
                    output_x: surface.output_x,
                    output_y: surface.output_y,
                    data: stream.data.to_vec(),
                };
                let _ = self.tx.send(GatewayEvent::Video(packet));
                Ok(Vec::new())
            }
            GfxPdu::WireToSurface2(wire) => {
                let Some(surface) = self.surfaces.get(&wire.surface_id).copied() else {
                    warn!(
                        "progressive update references unknown surface {}",
                        wire.surface_id
                    );
                    return Ok(Vec::new());
                };
                let tiles = match self
                    .progressive
                    .decode_bitmap(
                        wire.codec_context_id,
                        surface.width,
                        surface.height,
                        &wire.bitmap_data,
                    )
                {
                    Ok(tiles) => tiles,
                    Err(error) => {
                        if !self.warned_progressive_error {
                            self.warned_progressive_error = true;
                            warn!("could not decode RemoteFX Progressive update: {error}");
                        }
                        return Ok(Vec::new());
                    }
                };

                for tile in tiles {
                    let x = tile.x_idx.saturating_mul(64);
                    let y = tile.y_idx.saturating_mul(64);
                    let width = surface.width.saturating_sub(x).min(64);
                    let height = surface.height.saturating_sub(y).min(64);
                    let pixels = crop_rgba_tile(&tile.pixels, width, height);
                    self.send_bitmap(
                        VideoCodec::RgbaProgressive,
                        wire.surface_id,
                        BitmapRegion {
                            x,
                            y,
                            width,
                            height,
                        },
                        pixels,
                    );
                }
                Ok(Vec::new())
            }
            GfxPdu::DeleteEncodingContext(context) => {
                self.progressive.delete_context(context.codec_context_id);
                Ok(Vec::new())
            }
            GfxPdu::EndFrame(end) => {
                self.queued_frames = self.queued_frames.saturating_sub(1);
                self.total_frames = self.total_frames.wrapping_add(1);
                let acknowledge = GfxPdu::FrameAcknowledge(FrameAcknowledgePdu {
                    queue_depth: QueueDepth::from_u32(self.queued_frames),
                    frame_id: end.frame_id,
                    total_frames_decoded: self.total_frames,
                });
                Ok(vec![Box::new(acknowledge) as DvcMessage])
            }
            other => {
                debug!("unhandled EGFX PDU: {other:?}");
                Ok(Vec::new())
            }
        }
    }
}

impl_as_any!(EgfxPassthrough);

impl DvcProcessor for EgfxPassthrough {
    fn channel_name(&self) -> &str {
        CHANNEL_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.control(ControlMsg::Status {
            phase: "egfx",
            message: "graphics channel opened; advertising AVC420".to_owned(),
        });
        let capabilities = [CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::AVC420_ENABLED | CapabilitiesV81Flags::SMALL_CACHE,
        }];
        let advertise =
            GfxPdu::CapabilitiesAdvertise(CapabilitiesAdvertisePdu::from_typed(&capabilities));
        Ok(vec![Box::new(advertise) as DvcMessage])
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        self.decompressed.clear();
        if self.decompressed.capacity() > MAX_RETAINED_DECOMPRESSED_CAPACITY {
            self.decompressed
                .shrink_to(MAX_RETAINED_DECOMPRESSED_CAPACITY);
        }
        self.decompressor
            .decompress(payload, &mut self.decompressed)
            .map_err(|error| decode_err!(error))?;

        let mut pdus = Vec::new();
        let mut cursor = ReadCursor::new(&self.decompressed);
        while !cursor.is_empty() {
            pdus.push(decode_cursor::<GfxPdu>(&mut cursor).map_err(|error| decode_err!(error))?);
        }

        let mut responses = Vec::new();
        for pdu in pdus {
            responses.extend(self.handle_pdu(pdu)?);
        }
        Ok(responses)
    }

    fn close(&mut self, _channel_id: u32) {
        warn!("EGFX dynamic channel closed");
        self.control(ControlMsg::Warning {
            message: "graphics channel closed".to_owned(),
        });
    }
}

impl DvcClientProcessor for EgfxPassthrough {}

fn bgra_to_rgba(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

fn crop_rgba_tile(pixels: &[u8], width: u16, height: u16) -> Vec<u8> {
    let width = usize::from(width);
    let height = usize::from(height);
    if width == 64 && height == 64 {
        return pixels.to_vec();
    }

    let mut cropped = Vec::with_capacity(width.saturating_mul(height).saturating_mul(4));
    for row in pixels.chunks_exact(64 * 4).take(height) {
        cropped.extend_from_slice(&row[..width * 4]);
    }
    cropped
}

fn contains_idr(data: &[u8]) -> bool {
    let mut offset = 0usize;
    while offset.saturating_add(4) <= data.len() {
        let length = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;
        let Some(end) = offset.checked_add(length) else {
            return false;
        };
        if end > data.len() || length == 0 {
            return false;
        }
        if data[offset] & 0x1f == 5 {
            return true;
        }
        offset = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_idr_in_avc_access_unit() {
        let data = [0, 0, 0, 2, 0x67, 0, 0, 0, 0, 1, 0x65];
        assert!(contains_idr(&data));
    }

    #[test]
    fn rejects_truncated_avc_access_unit() {
        let data = [0, 0, 0, 8, 0x65];
        assert!(!contains_idr(&data));
    }

    #[test]
    fn converts_bgra_pixels_to_rgba() {
        let mut pixels = [1, 2, 3, 255, 4, 5, 6, 255];
        bgra_to_rgba(&mut pixels);
        assert_eq!(pixels, [3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn crops_edge_progressive_tile() {
        let pixels = vec![7; 64 * 64 * 4];
        let cropped = crop_rgba_tile(&pixels, 3, 2);
        assert_eq!(cropped, vec![7; 3 * 2 * 4]);
    }
}
