//! H.264 encode/decode. Software (openh264) always works; hardware (Media Foundation) is tried first.
use anyhow::{anyhow, Result};
use openh264::decoder::Decoder;
use openh264::encoder::{BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};
use openh264::OpenH264API;

pub trait VideoEncoder: Send {
    /// Encodes one BGRA frame (tightly packed, even dimensions) to an Annex-B bitstream.
    /// Returns (data, is_keyframe); empty data means the encoder skipped the frame.
    fn encode(&mut self, bgra: &[u8], force_key: bool) -> Result<(Vec<u8>, bool)>;
    fn set_bitrate(&mut self, bps: u32);
    fn name(&self) -> String;
}

/// Picks the best encoder for this machine: hardware via Media Foundation, else openh264.
#[allow(unused_variables)]
pub fn new_encoder(w: usize, h: usize, fps: u32, bps: u32, allow_hw: bool) -> Result<Box<dyn VideoEncoder>> {
    #[cfg(windows)]
    if allow_hw {
        match crate::mf::HwEncoder::new(w, h, fps, bps) {
            Ok(e) => return Ok(Box::new(e)),
            Err(e) => crate::plog!("hardware encoder unavailable, using software: {e:#}"),
        }
    }
    Ok(Box::new(SoftEncoder::new(w, h, fps, bps)?))
}

pub struct SoftEncoder {
    enc: Encoder,
    yuv: YUVBuffer,
    w: usize,
    h: usize,
}

impl SoftEncoder {
    pub fn new(w: usize, h: usize, fps: u32, bps: u32) -> Result<Self> {
        let cfg = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bps))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .usage_type(UsageType::CameraVideoRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(true)
            .complexity(Complexity::Low)
            .intra_frame_period(IntraFramePeriod::from_num_frames(0)) // keyframes only on request
            .num_threads(4);
        let enc = Encoder::with_api_config(OpenH264API::from_source(), cfg).map_err(|e| anyhow!("openh264: {e}"))?;
        Ok(Self { enc, yuv: YUVBuffer::new(w, h), w, h })
    }
}

impl VideoEncoder for SoftEncoder {
    fn encode(&mut self, bgra: &[u8], force_key: bool) -> Result<(Vec<u8>, bool)> {
        self.yuv.read_bgra8(BgraSliceU8::new(bgra, (self.w, self.h)));
        if force_key {
            self.enc.force_intra_frame();
        }
        let bs = self.enc.encode(&self.yuv).map_err(|e| anyhow!("encode: {e}"))?;
        let key = matches!(bs.frame_type(), FrameType::IDR | FrameType::I);
        Ok((bs.to_vec(), key))
    }

    fn set_bitrate(&mut self, bps: u32) {
        use openh264_sys2::{SBitrateInfo, ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL};
        let mut info = SBitrateInfo { iLayer: SPATIAL_LAYER_ALL, iBitrate: bps as i32 };
        unsafe {
            self.enc.raw_api().set_option(ENCODER_OPTION_BITRATE, &mut info as *mut _ as *mut _);
        }
    }

    fn name(&self) -> String {
        "software (openh264)".into()
    }
}

/// Decoded picture, I420 planes tightly packed.
pub struct Frame {
    pub w: usize,
    pub h: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

pub struct VideoDecoder(Decoder);

impl VideoDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self(Decoder::new().map_err(|e| anyhow!("decoder: {e}"))?))
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Frame>> {
        let Some(yuv) = self.0.decode(data).map_err(|e| anyhow!("decode: {e}"))? else { return Ok(None) };
        let (w, h) = yuv.dimensions();
        let (sy, su, sv) = yuv.strides();
        let plane = |src: &[u8], stride: usize, pw: usize, ph: usize| {
            let mut out = Vec::with_capacity(pw * ph);
            for r in 0..ph {
                out.extend_from_slice(&src[r * stride..r * stride + pw]);
            }
            out
        };
        Ok(Some(Frame {
            w,
            h,
            y: plane(yuv.y(), sy, w, h),
            u: plane(yuv.u(), su, w / 2, h / 2),
            v: plane(yuv.v(), sv, w / 2, h / 2),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a synthetic image and decode it back; colors must survive within codec tolerance.
    #[test]
    fn soft_roundtrip() {
        let (w, h) = (320, 240);
        let mut bgra = vec![0u8; w * h * 4];
        for px in bgra.chunks_mut(4) {
            px.copy_from_slice(&[200, 100, 50, 255]); // B G R A
        }
        let mut enc = SoftEncoder::new(w, h, 30, 2_000_000).unwrap();
        let (data, key) = enc.encode(&bgra, true).unwrap();
        assert!(key && !data.is_empty());
        let f = VideoDecoder::new().unwrap().decode(&data).unwrap().expect("frame");
        assert_eq!((f.w, f.h), (w, h));
        // BT.601 limited: R=50,G=100,B=200 -> Y ~ 0.257*50+0.504*100+0.098*200+16 = 98.9
        let y = f.y[w * h / 2 + w / 2] as i32;
        assert!((y - 99).abs() <= 4, "Y={y}");
    }
}
