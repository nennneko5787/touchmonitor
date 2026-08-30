//! H.264 encoder backed by OpenH264. Accepts BGRA8 frames (the format produced by
//! the Windows Graphics Capture pipeline) and emits Annex-B H.264 access units.
//!
//! OpenH264 encodes Baseline profile, which every iOS device's VideoToolbox can
//! decode, and it is royalty-free to use.

use openh264::encoder::{Encoder, EncoderConfig, BitRate, FrameRate, FrameType};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::OpenH264API;

pub struct H264Encoder {
    encoder: Encoder,
    width: usize,
    height: usize,
    yuv: YUVBuffer,
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

impl H264Encoder {
    pub fn new(
        width: usize,
        height: usize,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate_kbps * 1000))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .num_threads(4);

        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| format!("create encoder: {e}"))?;

        let yuv = YUVBuffer::new(width, height);
        Ok(Self {
            encoder,
            width,
            height,
            yuv,
        })
    }

    /// Encode one BGRA8 frame. Returns the encoded access unit (Annex-B, with
    /// SPS/PPS automatically prepended on keyframes).
    pub fn encode_frame(&mut self, bgra: &[u8]) -> Result<EncodedFrame, Box<dyn std::error::Error>> {
        let src = BgraSliceU8::new(bgra, (self.width, self.height));
        self.yuv.read_bgra8(src);
        let bitstream = self
            .encoder
            .encode(&self.yuv)
            .map_err(|e| format!("encode: {e}"))?;

        let data = bitstream.to_vec();
        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        Ok(EncodedFrame { data, keyframe })
    }
}
