//! Host: capture what the speakers play (WASAPI loopback). Client: play it back with a small buffer.
use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Starts loopback capture; `on_chunk` gets 5 ms of stereo i16 at a time.
/// Returns (stream, sample_rate). Keep the stream alive on the calling thread.
pub fn start_loopback(mut on_chunk: impl FnMut(Vec<i16>) + Send + 'static) -> Result<(cpal::Stream, u32)> {
    let dev = cpal::default_host().default_output_device().context("no audio output device")?;
    let cfg = dev.default_output_config()?;
    if cfg.sample_format() != cpal::SampleFormat::F32 {
        bail!("unsupported loopback format {:?}", cfg.sample_format());
    }
    let (rate, ch) = (cfg.sample_rate(), cfg.channels() as usize);
    let chunk = (rate as usize / 200) * 2; // 5 ms stereo
    let mut acc: Vec<i16> = Vec::with_capacity(chunk);
    let stream = dev.build_input_stream(
        cfg.config(),
        move |data: &[f32], _| {
            for f in data.chunks(ch) {
                let l = f[0];
                let r = if ch > 1 { f[1] } else { l };
                acc.push((l.clamp(-1.0, 1.0) * 32767.0) as i16);
                acc.push((r.clamp(-1.0, 1.0) * 32767.0) as i16);
                if acc.len() >= chunk {
                    on_chunk(std::mem::replace(&mut acc, Vec::with_capacity(chunk)));
                }
            }
        },
        |e| crate::plog!("audio capture error: {e}"),
        None,
    )?;
    stream.play()?;
    Ok((stream, rate))
}

pub struct PlayBuf {
    /// Volume multiplier set by the client (0..1).
    pub gain: f32,
    q: VecDeque<f32>,
    in_rate: f64,
    out_rate: f64,
    out_ch: usize,
    pos: f64,
    last: [f32; 2],
}

impl PlayBuf {
    pub fn new(in_rate: u32, out_rate: f64, out_ch: usize) -> Self {
        Self { gain: 1.0, q: VecDeque::new(), in_rate: in_rate as f64, out_rate, out_ch, pos: 0.0, last: [0.0; 2] }
    }

    pub fn pop(&mut self) -> Option<f32> {
        self.q.pop_front()
    }

    /// Queues interleaved stereo i16, resampling (linear) to the output device's rate/layout.
    pub fn push(&mut self, pcm: &[i16]) {
        let n = pcm.len() / 2;
        let frame = |i: usize| -> [f32; 2] {
            if i == 0 { self.last } else { [pcm[(i - 1) * 2] as f32 / 32768.0, pcm[(i - 1) * 2 + 1] as f32 / 32768.0] }
        };
        let step = self.in_rate / self.out_rate;
        while self.pos < n as f64 {
            let i = self.pos as usize;
            let t = (self.pos - i as f64) as f32;
            let (a, b) = (frame(i), frame(i + 1));
            let (l, r) = ((a[0] + (b[0] - a[0]) * t) * self.gain, (a[1] + (b[1] - a[1]) * t) * self.gain);
            match self.out_ch {
                1 => self.q.push_back((l + r) * 0.5),
                c => {
                    self.q.push_back(l);
                    self.q.push_back(r);
                    self.q.extend(std::iter::repeat_n(0.0, c - 2));
                }
            }
            self.pos += step;
        }
        self.pos -= n as f64;
        if n > 0 {
            self.last = frame(n);
        }
        // ponytail: fixed latency cap (drop to 40 ms when >120 ms queued); adaptive jitter buffer if audio stutters
        let per_sec = self.out_rate as usize * self.out_ch;
        if self.q.len() > per_sec * 120 / 1000 {
            let excess = self.q.len() - per_sec * 40 / 1000;
            self.q.drain(..excess - excess % self.out_ch);
        }
    }
}

/// Opens the default output device. Returns the stream (keep alive) and the buffer to push into.
pub fn start_player(in_rate: u32) -> Result<(cpal::Stream, Arc<Mutex<PlayBuf>>)> {
    let dev = cpal::default_host().default_output_device().context("no audio output device")?;
    let cfg = dev.default_output_config()?;
    if cfg.sample_format() != cpal::SampleFormat::F32 {
        bail!("unsupported output format {:?}", cfg.sample_format());
    }
    let buf = Arc::new(Mutex::new(PlayBuf {
        gain: 1.0,
        q: VecDeque::new(),
        in_rate: in_rate as f64,
        out_rate: cfg.sample_rate() as f64,
        out_ch: cfg.channels() as usize,
        pos: 0.0,
        last: [0.0; 2],
    }));
    let b = buf.clone();
    let stream = dev.build_output_stream(
        cfg.config(),
        move |out: &mut [f32], _| {
            let mut q = b.lock().unwrap();
            for s in out.iter_mut() {
                *s = q.q.pop_front().unwrap_or(0.0);
            }
        },
        |e| crate::plog!("audio output error: {e}"),
        None,
    )?;
    stream.play()?;
    Ok((stream, buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_counts() {
        let mut p = PlayBuf { gain: 1.0, q: VecDeque::new(), in_rate: 48000.0, out_rate: 44100.0, out_ch: 2, pos: 0.0, last: [0.0; 2] };
        for _ in 0..100 {
            p.push(&vec![1000i16; 480]); // 100 x 5 ms of 48 kHz stereo = 0.5 s
        }
        let frames = p.q.len() / 2;
        // 0.5 s at 44.1 kHz = 22050 frames, but the 120 ms cap trims to <= ~120 ms
        assert!(frames <= 44100 * 120 / 1000 + 300, "{frames}");
        let mut p2 = PlayBuf { gain: 1.0, q: VecDeque::new(), in_rate: 48000.0, out_rate: 44100.0, out_ch: 6, pos: 0.0, last: [0.0; 2] };
        p2.push(&vec![1000i16; 480]); // 5 ms
        assert_eq!(p2.q.len() % 6, 0);
        assert!((p2.q.len() / 6).abs_diff(220) <= 2, "{}", p2.q.len() / 6);
    }
}

// ---------------------------------------------------------------- process-excluding loopback
#[cfg(windows)]
pub use excl::*;

#[cfg(windows)]
mod excl {
use super::*;


use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{CoInitializeEx, BLOB, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows_core::Interface;

#[windows_core::implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(std::sync::mpsc::SyncSender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: windows_core::Ref<IActivateAudioInterfaceAsyncOperation>) -> windows_core::Result<()> {
        let _ = self.0.try_send(());
        Ok(())
    }
}

/// Keeps the capture thread alive; stops it on drop.
pub struct ExcludeLoopback {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ExcludeLoopback {
    fn drop(&mut self) {
        self.stop.store(true, SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Captures everything the PC plays EXCEPT this app (Windows 10 2004+), so voices we play
/// aren't streamed back to the guests. 5 ms stereo i16 chunks at 48 kHz; silence is skipped.
pub fn start_loopback_excluding_self(mut on_chunk: impl FnMut(Vec<i16>) + Send + 'static) -> Result<(ExcludeLoopback, u32)> {
    const RATE: u32 = 48_000;
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<std::result::Result<(), String>>(1);
    let st = stop.clone();
    let thread = std::thread::spawn(move || unsafe {
        let mut run = || -> Result<()> {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let params = AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        TargetProcessId: std::process::id(),
                        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                    },
                },
            };
            let pv = std::mem::ManuallyDrop::new(PROPVARIANT {
                Anonymous: PROPVARIANT_0 {
                    Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                        vt: VT_BLOB,
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: PROPVARIANT_0_0_0 {
                            blob: BLOB { cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32, pBlobData: &params as *const _ as *mut u8 },
                        },
                    }),
                },
            });
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let handler: IActivateAudioInterfaceCompletionHandler = Activated(tx).into();
            let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv), &handler)?;
            rx.recv_timeout(std::time::Duration::from_secs(5)).context("audio activation timed out")?;
            let (mut hr, mut unk) = (windows_core::HRESULT(0), None);
            op.GetActivateResult(&mut hr, &mut unk)?;
            hr.ok()?;
            let client: IAudioClient = unk.context("no audio client")?.cast()?;
            let fmt = WAVEFORMATEX { wFormatTag: WAVE_FORMAT_PCM as u16, nChannels: 2, nSamplesPerSec: RATE, nAvgBytesPerSec: RATE * 4, nBlockAlign: 4, wBitsPerSample: 16, cbSize: 0 };
            let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, &fmt, None)?;
            let event = CreateEventW(None, false, false, None)?;
            client.SetEventHandle(event)?;
            let cap: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            let _ = ready_tx.send(Ok(()));
            let chunk = (RATE as usize / 200) * 2;
            let mut acc: Vec<i16> = Vec::with_capacity(chunk * 2);
            while !st.load(SeqCst) {
                WaitForSingleObject(event, 100);
                while cap.GetNextPacketSize()? > 0 {
                    let (mut data, mut frames, mut fl) = (std::ptr::null_mut(), 0u32, 0u32);
                    cap.GetBuffer(&mut data, &mut frames, &mut fl, None, None)?;
                    let n = frames as usize * 2;
                    if fl & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        acc.extend(std::iter::repeat_n(0, n));
                    } else {
                        acc.extend_from_slice(std::slice::from_raw_parts(data as *const i16, n));
                    }
                    cap.ReleaseBuffer(frames)?;
                    while acc.len() >= chunk {
                        let rest = acc.split_off(chunk);
                        let c = std::mem::replace(&mut acc, rest);
                        if c.iter().any(|s| *s != 0) {
                            on_chunk(c); // silence isn't worth sending
                        }
                    }
                }
            }
            let _ = client.Stop();
            Ok(())
        };
        if let Err(e) = run() {
            let _ = ready_tx.send(Err(format!("{e:#}")));
        }
    });
    match ready_rx.recv_timeout(std::time::Duration::from_secs(6)) {
        Ok(Ok(())) => Ok((ExcludeLoopback { stop, thread: Some(thread) }, RATE)),
        Ok(Err(e)) => Err(anyhow::anyhow!(e)),
        Err(_) => Err(anyhow::anyhow!("process loopback didn't start")),
    }
}

}
