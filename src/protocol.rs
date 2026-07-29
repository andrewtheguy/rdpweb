use serde::{Deserialize, Serialize};

pub const VIDEO_MAGIC: &[u8; 4] = b"RDPH";
pub const VIDEO_VERSION: u8 = 1;
pub const VIDEO_HEADER_LEN: usize = 38;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientMsg {
    MouseMove { x: i32, y: i32 },
    MouseButton { button: MouseButton, pressed: bool },
    Wheel { dx: f32, dy: f32 },
    Key { code: String, pressed: bool },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ControlMsg {
    Status {
        phase: &'static str,
        message: String,
    },
    Resize {
        width: u32,
        height: u32,
    },
    EgfxNegotiated {
        capability: String,
    },
    Warning {
        message: String,
    },
    Error {
        message: String,
    },
}

pub enum GatewayEvent {
    Control(ControlMsg),
    Video(VideoPacket),
}

pub struct VideoPacket {
    pub codec: VideoCodec,
    pub key_frame: bool,
    pub timestamp_us: u64,
    pub surface_id: u16,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub output_x: u32,
    pub output_y: u32,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum VideoCodec {
    Avc420 = 1,
    RgbaClearCodec = 2,
    RgbaProgressive = 3,
}

impl VideoPacket {
    pub fn encode(self) -> Vec<u8> {
        let data_len = u32::try_from(self.data.len()).unwrap_or(u32::MAX);
        let mut bytes = Vec::with_capacity(VIDEO_HEADER_LEN + self.data.len());
        bytes.extend_from_slice(VIDEO_MAGIC);
        bytes.push(VIDEO_VERSION);
        bytes.push(self.codec as u8);
        bytes.push(u8::from(self.key_frame));
        bytes.push(0);
        bytes.extend_from_slice(&self.timestamp_us.to_le_bytes());
        bytes.extend_from_slice(&self.surface_id.to_le_bytes());
        bytes.extend_from_slice(&self.x.to_le_bytes());
        bytes.extend_from_slice(&self.y.to_le_bytes());
        bytes.extend_from_slice(&self.width.to_le_bytes());
        bytes.extend_from_slice(&self.height.to_le_bytes());
        bytes.extend_from_slice(&self.output_x.to_le_bytes());
        bytes.extend_from_slice(&self.output_y.to_le_bytes());
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.extend_from_slice(&self.data);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_header_is_stable() {
        let bytes = VideoPacket {
            codec: VideoCodec::Avc420,
            key_frame: true,
            timestamp_us: 99,
            surface_id: 2,
            x: 3,
            y: 4,
            width: 1280,
            height: 800,
            output_x: 5,
            output_y: 6,
            data: vec![1, 2, 3],
        }
        .encode();

        assert_eq!(&bytes[..4], VIDEO_MAGIC);
        assert_eq!(bytes[4], VIDEO_VERSION);
        assert_eq!(bytes[5], VideoCodec::Avc420 as u8);
        assert_eq!(bytes[6], 1);
        assert_eq!(bytes.len(), VIDEO_HEADER_LEN + 3);
        assert_eq!(&bytes[VIDEO_HEADER_LEN..], &[1, 2, 3]);
    }
}
