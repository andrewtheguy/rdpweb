use std::collections::BTreeMap;
use std::time::Instant;

use ironrdp::core::{Decode as _, ReadCursor, impl_as_any};
use ironrdp::dvc::{DvcClientProcessor, DvcMessage, DvcProcessor};
use ironrdp::graphics::zgfx;
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

pub struct EgfxPassthrough {
    tx: mpsc::UnboundedSender<GatewayEvent>,
    decompressor: zgfx::Decompressor,
    decompressed: Vec<u8>,
    surface_origins: BTreeMap<u16, (u32, u32)>,
    current_frame_id: u32,
    queued_frames: u32,
    total_frames: u32,
    clock: Instant,
    last_timestamp_us: u64,
    warned_progressive: bool,
    warned_unsupported_codec: bool,
}

impl EgfxPassthrough {
    pub fn new(tx: mpsc::UnboundedSender<GatewayEvent>) -> Self {
        Self {
            tx,
            decompressor: zgfx::Decompressor::new(),
            decompressed: Vec::new(),
            surface_origins: BTreeMap::new(),
            current_frame_id: 0,
            queued_frames: 0,
            total_frames: 0,
            clock: Instant::now(),
            last_timestamp_us: 0,
            warned_progressive: false,
            warned_unsupported_codec: false,
        }
    }

    fn control(&self, message: ControlMsg) {
        let _ = self.tx.send(GatewayEvent::Control(message));
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
                self.surface_origins.clear();
                self.control(ControlMsg::Resize {
                    width: reset.width,
                    height: reset.height,
                });
                Ok(Vec::new())
            }
            GfxPdu::MapSurfaceToOutput(mapping) => {
                self.surface_origins.insert(
                    mapping.surface_id,
                    (mapping.output_origin_x, mapping.output_origin_y),
                );
                Ok(Vec::new())
            }
            GfxPdu::StartFrame(start) => {
                self.current_frame_id = start.frame_id;
                self.queued_frames = self.queued_frames.saturating_add(1);
                Ok(Vec::new())
            }
            GfxPdu::WireToSurface1(wire) => {
                if wire.codec_id != Codec1Type::Avc420 {
                    if !self.warned_unsupported_codec {
                        self.warned_unsupported_codec = true;
                        self.control(ControlMsg::Warning {
                            message: format!(
                                "server sent {:?}; this POC deliberately negotiates AVC420 only",
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
                let (output_x, output_y) = self
                    .surface_origins
                    .get(&wire.surface_id)
                    .copied()
                    .unwrap_or_default();

                let elapsed = u64::try_from(self.clock.elapsed().as_micros()).unwrap_or(u64::MAX);
                let timestamp_us = elapsed.max(self.last_timestamp_us.saturating_add(1));
                self.last_timestamp_us = timestamp_us;

                let packet = VideoPacket {
                    codec: VideoCodec::Avc420,
                    key_frame: contains_idr(stream.data),
                    timestamp_us,
                    frame_id: self.current_frame_id,
                    surface_id: wire.surface_id,
                    x: rectangle.left,
                    y: rectangle.top,
                    width,
                    height,
                    output_x,
                    output_y,
                    data: stream.data.to_vec(),
                };
                let _ = self.tx.send(GatewayEvent::Video(packet));
                Ok(Vec::new())
            }
            GfxPdu::WireToSurface2(_) => {
                if !self.warned_progressive {
                    self.warned_progressive = true;
                    self.control(ControlMsg::Warning {
                        message: "server selected RemoteFX Progressive instead of AVC420"
                            .to_owned(),
                    });
                }
                Ok(Vec::new())
            }
            GfxPdu::EndFrame(end) => {
                self.current_frame_id = 0;
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
}
