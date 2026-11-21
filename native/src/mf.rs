//! Hardware H.264 encoder through Media Foundation (NVENC / AMF / QuickSync expose themselves as async MFTs).
use crate::codec::VideoEncoder;
use anyhow::{bail, Context, Result};
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};

fn var_u32(v: u32) -> VARIANT {
    VARIANT { Anonymous: VARIANT_0 { Anonymous: ManuallyDrop::new(VARIANT_0_0 { vt: VT_UI4, wReserved1: 0, wReserved2: 0, wReserved3: 0, Anonymous: VARIANT_0_0_0 { ulVal: v } }) } }
}

fn var_bool(v: bool) -> VARIANT {
    let b = windows::Win32::Foundation::VARIANT_BOOL(if v { -1 } else { 0 });
    VARIANT { Anonymous: VARIANT_0 { Anonymous: ManuallyDrop::new(VARIANT_0_0 { vt: VT_BOOL, wReserved1: 0, wReserved2: 0, wReserved3: 0, Anonymous: VARIANT_0_0_0 { boolVal: b } }) } }
}

pub struct HwEncoder {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    w: usize,
    h: usize,
    fps: u32,
    frame_no: i64,
    need_input: u32,
    provides_samples: bool,
    out_size: u32,
    seq_header: Vec<u8>,
    nv12: Vec<u8>,
    name: String,
}

// MFT is created and used on one thread at a time (MTA); moving it between threads is fine.
unsafe impl Send for HwEncoder {}

fn set_attr_size(t: &IMFMediaType, key: &GUID, a: u32, b: u32) -> windows::core::Result<()> {
    unsafe { t.SetUINT64(key, ((a as u64) << 32) | b as u64) }
}

impl HwEncoder {
    pub fn new(w: usize, h: usize, fps: u32, bps: u32) -> Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;

            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
            let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &mut list,
                &mut count,
            )
            .context("MFTEnumEx")?;
            let activates: Vec<IMFActivate> = (0..count as usize).filter_map(|i| (*list.add(i)).take()).collect();
            CoTaskMemFree(Some(list as _));

            let mut last_err = anyhow::anyhow!("no hardware H.264 encoder found");
            for act in activates {
                let mut name = String::from("hardware encoder");
                let mut buf = [0u16; 256];
                let mut len = 0;
                if act.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)).is_ok() {
                    name = String::from_utf16_lossy(&buf[..len as usize]);
                }
                match Self::setup(&act, w, h, fps, bps, name.clone()) {
                    Ok(e) => return Ok(e),
                    Err(e) => last_err = e.context(name),
                }
            }
            Err(last_err)
        }
    }

    unsafe fn setup(act: &IMFActivate, w: usize, h: usize, fps: u32, bps: u32, name: String) -> Result<Self> {
        let mft: IMFTransform = act.ActivateObject()?;
        let attrs = mft.GetAttributes()?;
        attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("async unlock")?;
        let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, bps)?;
        set_attr_size(&out, &MF_MT_FRAME_SIZE, w as u32, h as u32)?;
        set_attr_size(&out, &MF_MT_FRAME_RATE, fps, 1)?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // High profile: ~17% less error than Baseline at the same bitrate (see hw_roundtrip);
        // the openh264 decoder reads it fine.
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        mft.SetOutputType(0, &out, 0).context("SetOutputType")?;

        let mut set_in = false;
        for i in 0.. {
            let Ok(t) = mft.GetInputAvailableType(0, i) else { break };
            if t.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                set_attr_size(&t, &MF_MT_FRAME_SIZE, w as u32, h as u32)?;
                set_attr_size(&t, &MF_MT_FRAME_RATE, fps, 1)?;
                mft.SetInputType(0, &t, 0).context("SetInputType")?;
                set_in = true;
                break;
            }
        }
        if !set_in {
            bail!("encoder does not accept NV12");
        }

        let codec: Option<ICodecAPI> = mft.cast().ok();
        if let Some(c) = &codec {
            let set = |k: &GUID, v: VARIANT| {
                let _ = c.SetValue(k, &v);
            };
            set(&CODECAPI_AVLowLatencyMode, var_bool(true));
            set(&CODECAPI_AVEncCommonRateControlMode, var_u32(eAVEncCommonRateControlMode_CBR.0 as u32));
            set(&CODECAPI_AVEncCommonMeanBitRate, var_u32(bps));
            set(&CODECAPI_AVEncMPVDefaultBPictureCount, var_u32(0u32));
            set(&CODECAPI_AVEncMPVGOPSize, var_u32(fps * 10)); // periodic refresh every ~10s
        }

        let info = mft.GetOutputStreamInfo(0)?;
        let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;

        let mut seq_header = Vec::new();
        if let Ok(cur) = mft.GetOutputCurrentType(0) {
            if let Ok(n) = cur.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
                seq_header.resize(n as usize, 0);
                if cur.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut seq_header, None).is_err() {
                    seq_header.clear();
                }
            }
        }

        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        let events: IMFMediaEventGenerator = mft.cast()?;
        Ok(Self {
            mft,
            events,
            codec,
            w,
            h,
            fps,
            frame_no: 0,
            need_input: 0,
            provides_samples,
            out_size: info.cbSize.max((w * h) as u32),
            seq_header,
            nv12: vec![0; w * h * 3 / 2],
            name,
        })
    }

    /// Non-blocking poll of the MFT's async events; collects encoded output into `out`.
    unsafe fn pump(&mut self, out: &mut Vec<u8>, key: &mut bool) -> Result<bool> {
        let ev = match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
            Ok(e) => e,
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let t = ev.GetType()?;
        if t == METransformNeedInput.0 as u32 {
            self.need_input += 1;
        } else if t == METransformHaveOutput.0 as u32 {
            self.collect_output(out, key)?;
            return Ok(true);
        }
        Ok(false)
    }

    unsafe fn collect_output(&mut self, out: &mut Vec<u8>, key: &mut bool) -> Result<()> {
        let sample = if self.provides_samples {
            None
        } else {
            let s = MFCreateSample()?;
            s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
            Some(s)
        };
        let mut buf = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(sample), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
        let mut status = 0;
        let r = self.mft.ProcessOutput(0, &mut buf, &mut status);
        let sample = ManuallyDrop::take(&mut buf[0].pSample);
        drop(ManuallyDrop::take(&mut buf[0].pEvents));
        match r {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                let t = self.mft.GetOutputAvailableType(0, 0)?;
                self.mft.SetOutputType(0, &t, 0)?;
                return Ok(());
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let Some(sample) = sample else { return Ok(()) };
        let is_key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
        let mb = sample.ConvertToContiguousBuffer()?;
        let (mut p, mut len) = (std::ptr::null_mut(), 0u32);
        mb.Lock(&mut p, None, Some(&mut len))?;
        let data = std::slice::from_raw_parts(p, len as usize);
        if is_key && !starts_with_sps(data) && !self.seq_header.is_empty() {
            out.extend_from_slice(&self.seq_header);
        }
        out.extend_from_slice(data);
        mb.Unlock()?;
        *key |= is_key;
        Ok(())
    }
}

/// Every H.264 / HEVC / AV1 encoder and decoder Windows offers (for the codec probe).
pub fn list_codecs() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
        let codecs = [("H.264", MFVideoFormat_H264), ("HEVC", MFVideoFormat_HEVC), ("AV1", MFVideoFormat_AV1)];
        for (cat, cat_name) in [(MFT_CATEGORY_VIDEO_ENCODER, "encoder"), (MFT_CATEGORY_VIDEO_DECODER, "decoder")] {
            for (name, guid) in codecs {
                let reg = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: guid };
                let (inp, outp) = if cat_name == "encoder" { (None, Some(&reg as *const _)) } else { (Some(&reg as *const _), None) };
                for (flags, kind) in [(MFT_ENUM_FLAG_HARDWARE, "hardware"), (MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT, "software")] {
                    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
                    let mut count = 0u32;
                    if MFTEnumEx(cat, flags | MFT_ENUM_FLAG_SORTANDFILTER, inp, outp, &mut list, &mut count).is_err() {
                        continue;
                    }
                    for i in 0..count as usize {
                        if let Some(act) = (*list.add(i)).take() {
                            let mut buf = [0u16; 256];
                            let mut len = 0;
                            let n = if act.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)).is_ok() { String::from_utf16_lossy(&buf[..len as usize]) } else { "?".into() };
                            out.push(format!("{name:5} {cat_name} ({kind}): {n}"));
                        }
                    }
                    if !list.is_null() {
                        CoTaskMemFree(Some(list as _));
                    }
                }
            }
        }
    }
    out
}

/// True if the first NAL unit of an Annex-B stream is an SPS.
fn starts_with_sps(d: &[u8]) -> bool {
    let i = if d.starts_with(&[0, 0, 0, 1]) { 4 } else if d.starts_with(&[0, 0, 1]) { 3 } else { return false };
    d.get(i).is_some_and(|b| b & 0x1F == 7)
}

impl VideoEncoder for HwEncoder {
    fn encode(&mut self, bgra: &[u8], force_key: bool) -> Result<(Vec<u8>, bool)> {
        bgra_to_nv12(bgra, self.w, self.h, &mut self.nv12);
        let (mut out, mut key) = (Vec::new(), false);
        unsafe {
            // Wait (briefly) until the MFT asks for input.
            let deadline = Instant::now() + Duration::from_millis(50);
            while self.need_input == 0 {
                if !self.pump(&mut out, &mut key)? {
                    if Instant::now() > deadline {
                        return Ok((out, key)); // encoder busy: drop this frame
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
            }
            if force_key {
                if let Some(c) = &self.codec {
                    let _ = c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &var_u32(1u32));
                }
            }
            let sample = MFCreateSample()?;
            let mb = MFCreateMemoryBuffer(self.nv12.len() as u32)?;
            let mut p = std::ptr::null_mut();
            mb.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), p, self.nv12.len());
            mb.Unlock()?;
            mb.SetCurrentLength(self.nv12.len() as u32)?;
            sample.AddBuffer(&mb)?;
            let dur = 10_000_000 / self.fps as i64;
            sample.SetSampleTime(self.frame_no * dur)?;
            sample.SetSampleDuration(dur)?;
            self.frame_no += 1;
            self.mft.ProcessInput(0, &sample, 0)?;
            self.need_input -= 1;

            // Low-latency mode yields the frame almost immediately; wait a bit for it.
            let deadline = Instant::now() + Duration::from_millis(40);
            while out.is_empty() && Instant::now() < deadline {
                if !self.pump(&mut out, &mut key)? {
                    std::thread::sleep(Duration::from_micros(200));
                }
            }
        }
        Ok((out, key))
    }

    fn set_bitrate(&mut self, bps: u32) {
        if let Some(c) = &self.codec {
            unsafe {
                let _ = c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &var_u32(bps));
            }
        }
    }

    fn name(&self) -> String {
        format!("hardware ({})", self.name)
    }
}

impl Drop for HwEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

/// BGRA -> NV12 (BT.601 limited range, same matrix as openh264 so colors match either path).
pub fn bgra_to_nv12(bgra: &[u8], w: usize, h: usize, nv12: &mut [u8]) {
    let (y_plane, uv_plane) = nv12.split_at_mut(w * h);
    let bands = 8;
    let rows_per = (h / 2).div_ceil(bands) * 2; // even rows per band
    std::thread::scope(|s| {
        for (band, (ys, uvs)) in y_plane.chunks_mut(rows_per * w).zip(uv_plane.chunks_mut(rows_per / 2 * w)).enumerate() {
            let y0 = band * rows_per;
            s.spawn(move || {
                let rows = ys.len() / w;
                for r in 0..rows {
                    let src = &bgra[(y0 + r) * w * 4..(y0 + r + 1) * w * 4];
                    for x in 0..w {
                        let (b, g, rr) = (src[x * 4] as i32, src[x * 4 + 1] as i32, src[x * 4 + 2] as i32);
                        ys[r * w + x] = (((66 * rr + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                    }
                    if r % 2 == 0 {
                        let uv = &mut uvs[(r / 2) * w..(r / 2) * w + w];
                        for x in (0..w).step_by(2) {
                            let (b, g, rr) = (src[x * 4] as i32, src[x * 4 + 1] as i32, src[x * 4 + 2] as i32);
                            uv[x] = (((-38 * rr - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                            uv[x + 1] = (((112 * rr - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
                        }
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::VideoDecoder;

    #[test]
    fn nv12_colors() {
        let (w, h) = (4, 4);
        let bgra: Vec<u8> = (0..w * h).flat_map(|_| [200u8, 100, 50, 255]).collect();
        let mut nv = vec![0; w * h * 3 / 2];
        bgra_to_nv12(&bgra, w, h, &mut nv);
        assert!((nv[0] as i32 - 99).abs() <= 2, "Y={}", nv[0]);
        // openh264's matrix: 0.4375*200 - 0.1484*50 - 0.2891*100 + 128 = 179
        assert!((nv[w * h] as i32 - 179).abs() <= 2, "U={}", nv[w * h]);
    }

    /// Runs only where a hardware encoder exists; decodes its output with openh264.
    #[test]
    fn hw_roundtrip() {
        let (w, h) = (1280, 720);
        let mut enc = match HwEncoder::new(w, h, 60, 8_000_000) {
            Ok(e) => e,
            Err(e) => return crate::plog!("skipping: {e:#}"),
        };
        crate::plog!("using {}", enc.name());
        let mut dec = VideoDecoder::new().unwrap();
        let mut decoded = 0;
        let mut worst = 0f64;
        let mut t_enc = std::time::Duration::ZERO;
        for i in 0..30u8 {
            // Detailed content (gradients + fine pattern), like text/UI on a desktop.
            let bgra: Vec<u8> = (0..w * h).flat_map(|p| {
                let (x, y) = (p % w, p / w);
                [(x * 255 / w) as u8, ((x ^ y) & 0xFF) as u8 / 2 + i, (y * 255 / h) as u8, 255]
            }).collect();
            let t0 = std::time::Instant::now();
            let (data, _key) = enc.encode(&bgra, i == 0).unwrap();
            t_enc += t0.elapsed();
            if data.is_empty() {
                continue;
            }
            if let Some(f) = dec.decode(&data).unwrap() {
                decoded += 1;
                // Compare the decoded luma with what the encoder was given.
                let mut nv = vec![0u8; w * h * 3 / 2];
                bgra_to_nv12(&bgra, w, h, &mut nv);
                let mad = f.y.iter().zip(&nv[..w * h]).map(|(a, b)| (*a as i32 - *b as i32).abs() as f64).sum::<f64>() / (w * h) as f64;
                worst = worst.max(mad);
            }
        }
        crate::plog!("decoded {decoded}/30, worst mean luma error {worst:.2}, encode {:.2} ms/frame", t_enc.as_secs_f64() * 1000.0 / 30.0);
        assert!(decoded >= 25, "decoded only {decoded}/30 frames");
        assert!(worst < 6.0, "decoded picture doesn't match (mean error {worst:.2}): decoder can't read this profile");
    }
}
