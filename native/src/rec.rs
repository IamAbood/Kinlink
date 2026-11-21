//! Screenshots (PNG) and session recording (MP4 via Media Foundation: the H.264 stream is copied
//! as-is, audio is encoded to AAC).
use crate::codec::Frame;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::time::Instant;
use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// "2026-11-21 14-03-22" in local time, for file names.
pub fn stamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

pub fn user_dir(kind: &str) -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("USERPROFILE")?).join(kind).join("Kinlink"))
}

pub fn screenshots_dir() -> Option<PathBuf> {
    user_dir("Pictures")
}

pub fn recordings_dir() -> Option<PathBuf> {
    user_dir("Videos")
}

/// I420 (BT.601 limited, same as the encoders) -> RGB.
fn to_rgb(f: &Frame) -> Vec<u8> {
    let mut out = vec![0u8; f.w * f.h * 3];
    for y in 0..f.h {
        for x in 0..f.w {
            let yy = 1.164 * (f.y[y * f.w + x] as f32 - 16.0);
            let u = f.u[(y / 2) * (f.w / 2) + x / 2] as f32 - 128.0;
            let v = f.v[(y / 2) * (f.w / 2) + x / 2] as f32 - 128.0;
            let i = (y * f.w + x) * 3;
            out[i] = (yy + 1.596 * v).clamp(0.0, 255.0) as u8;
            out[i + 1] = (yy - 0.392 * u - 0.813 * v).clamp(0.0, 255.0) as u8;
            out[i + 2] = (yy + 2.017 * u).clamp(0.0, 255.0) as u8;
        }
    }
    out
}

pub fn save_png(f: &Frame) -> Result<PathBuf> {
    let dir = screenshots_dir().context("no Pictures folder")?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("Screenshot {}.png", stamp()));
    let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let mut enc = png::Encoder::new(file, f.w as u32, f.h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&to_rgb(f))?;
    Ok(path)
}

/// SPS + PPS NAL units (with start codes) from an Annex-B keyframe.
fn sequence_header(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    for (k, &s) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map(|n| n - 3).unwrap_or(data.len());
        let end = if end > s && data[end - 1] == 0 { end - 1 } else { end }; // 4-byte start code
        if s < data.len() && matches!(data[s] & 0x1F, 7 | 8) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&data[s..end]);
        }
    }
    out
}

pub struct Recorder {
    writer: IMFSinkWriter,
    video: u32,
    audio: Option<u32>,
    start: Instant,
    last_video: i64,
    audio_rate: u32,
    audio_frames: u64,
    pub path: PathBuf,
    pub size: (usize, usize),
}

// Used only from the client's receive thread (MTA).
unsafe impl Send for Recorder {}

fn media_type(major: &windows::core::GUID, sub: &windows::core::GUID) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, major)?;
        t.SetGUID(&MF_MT_SUBTYPE, sub)?;
        Ok(t)
    }
}

impl Recorder {
    /// Starts a recording from a keyframe (`key` = Annex-B data) of size w x h.
    pub fn start(key: &[u8], w: usize, h: usize, audio_rate: u32) -> Result<Recorder> {
        let dir = recordings_dir().context("no Videos folder")?;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("Recording {}.mp4", stamp()));
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
            let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, None).context("create mp4")?;

            let v = media_type(&MFMediaType_Video, &MFVideoFormat_H264)?;
            v.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
            v.SetUINT64(&MF_MT_FRAME_RATE, (60u64 << 32) | 1)?;
            v.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            v.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            v.SetUINT32(&MF_MT_AVG_BITRATE, 20_000_000)?;
            let seq = sequence_header(key);
            if seq.is_empty() {
                return Err(anyhow!("keyframe has no SPS/PPS"));
            }
            v.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &seq)?;
            let video = writer.AddStream(&v).context("add video")?;
            writer.SetInputMediaType(video, &v, None).context("video passthrough")?;

            let mut audio = None;
            if audio_rate > 0 {
                let a_out = media_type(&MFMediaType_Audio, &MFAudioFormat_AAC)?;
                a_out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                a_out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, audio_rate)?;
                a_out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
                a_out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24000)?; // 192 kbps
                let a_in = media_type(&MFMediaType_Audio, &MFAudioFormat_PCM)?;
                a_in.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                a_in.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, audio_rate)?;
                a_in.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
                a_in.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
                a_in.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, audio_rate * 4)?;
                // AAC only supports 44.1/48 kHz here; skip audio rather than fail the recording.
                if let Ok(idx) = writer.AddStream(&a_out) {
                    if writer.SetInputMediaType(idx, &a_in, None).is_ok() {
                        audio = Some(idx);
                    }
                }
            }
            writer.BeginWriting().context("begin writing")?;
            let mut r = Recorder { writer, video, audio, start: Instant::now(), last_video: -1, audio_rate, audio_frames: 0, path, size: (w, h) };
            r.video(key, true)?;
            Ok(r)
        }
    }

    fn sample(data: &[u8], time: i64, dur: i64) -> Result<IMFSample> {
        unsafe {
            let s = MFCreateSample()?;
            let b = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut p = std::ptr::null_mut();
            b.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            b.Unlock()?;
            b.SetCurrentLength(data.len() as u32)?;
            s.AddBuffer(&b)?;
            s.SetSampleTime(time)?;
            s.SetSampleDuration(dur)?;
            Ok(s)
        }
    }

    /// One encoded video frame (Annex-B).
    pub fn video(&mut self, data: &[u8], key: bool) -> Result<()> {
        let t = (self.start.elapsed().as_nanos() / 100) as i64;
        let t = t.max(self.last_video + 1); // strictly increasing
        self.last_video = t;
        let s = Self::sample(data, t, 166_667)?;
        unsafe {
            if key {
                s.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
            }
            self.writer.WriteSample(self.video, &s)?;
        }
        Ok(())
    }

    /// Interleaved stereo i16 at the host's rate.
    pub fn audio(&mut self, pcm: &[i16]) -> Result<()> {
        let Some(idx) = self.audio else { return Ok(()) };
        let frames = (pcm.len() / 2) as u64;
        let t = (self.audio_frames * 10_000_000 / self.audio_rate as u64) as i64;
        let d = (frames * 10_000_000 / self.audio_rate as u64) as i64;
        self.audio_frames += frames;
        let bytes = unsafe { std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 2) };
        let s = Self::sample(bytes, t, d)?;
        unsafe { self.writer.WriteSample(idx, &s)? };
        Ok(())
    }

    pub fn finish(self) -> Result<PathBuf> {
        unsafe { self.writer.Finalize().context("finalize mp4")? };
        Ok(self.path)
    }
}

/// Reads a recording back with Windows' media reader: (video frames, audio packets, seconds).
/// Used by the self-test to prove the file actually plays.
pub fn probe(path: &std::path::Path) -> Result<(u32, u32, f64)> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        let r = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)?;
        let (mut v, mut a, mut last) = (0u32, 0u32, 0i64);
        loop {
            let (mut idx, mut flags, mut ts) = (0u32, 0u32, 0i64);
            let mut s: Option<IMFSample> = None;
            r.ReadSample(MF_SOURCE_READER_ANY_STREAM.0 as u32, 0, Some(&mut idx), Some(&mut flags), Some(&mut ts), Some(&mut s))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            if s.is_some() {
                let mt = r.GetCurrentMediaType(idx)?;
                if mt.GetGUID(&MF_MT_MAJOR_TYPE)? == MFMediaType_Video { v += 1 } else { a += 1 }
                last = last.max(ts);
            }
        }
        Ok((v, a, last as f64 / 1e7))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_header_extraction() {
        let key = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9, 9];
        assert_eq!(sequence_header(&key), vec![0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]);
    }

    /// Encodes a few frames with openh264 and records them; the MP4 must finalize and be non-trivial.
    #[test]
    fn records_mp4() {
        let (w, h) = (320, 240);
        let mut enc = crate::codec::SoftEncoder::new(w, h, 30, 1_000_000).unwrap();
        use crate::codec::VideoEncoder;
        let mut frames = Vec::new();
        for i in 0..30u8 {
            let bgra: Vec<u8> = (0..w * h).flat_map(|p| [i.wrapping_mul(8), (p % 255) as u8, 90, 255]).collect();
            frames.push(enc.encode(&bgra, i == 0).unwrap());
        }
        unsafe { std::env::set_var("USERPROFILE", std::env::temp_dir()) };
        let mut r = Recorder::start(&frames[0].0, w, h, 48000).unwrap();
        for (d, k) in &frames[1..] {
            if !d.is_empty() {
                r.video(d, *k).unwrap();
            }
            r.audio(&vec![0i16; 960]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let path = r.finish().unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let head = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(len > 2000, "mp4 too small: {len}");
        assert_eq!(&head[4..8], b"ftyp", "valid MP4 header");
    }
}
