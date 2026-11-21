//! Voice chat: microphone -> 24 kHz mono chunks (only while someone is actually talking), and a
//! mixer that plays every speaker at once. No codec: 24 kHz mono i16 is ~384 kbps while talking.
use crate::audio::PlayBuf;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const RATE: u32 = 24_000;
const CHUNK: usize = 480; // 20 ms
const LOUD: i64 = 250; // average |sample| that counts as talking
const HANGOVER: Duration = Duration::from_millis(300);

struct Inner {
    mic: AtomicBool,
    stop: AtomicBool,
    sources: Mutex<HashMap<String, PlayBuf>>,
    out_fmt: Mutex<Option<(f64, usize)>>,
    speaking: Mutex<HashMap<String, Instant>>,
    error: Mutex<Option<String>>,
}

pub struct Voice(Arc<Inner>);

fn level(pcm: &[i16]) -> i64 {
    pcm.iter().map(|s| (*s as i64).abs()).sum::<i64>() / pcm.len().max(1) as i64
}

/// Opens the default microphone; `on_chunk` gets 20 ms of 24 kHz mono while the user talks.
fn open_mic(inner: Arc<Inner>, mut on_chunk: impl FnMut(Vec<i16>) + Send + 'static) -> anyhow::Result<cpal::Stream> {
    let dev = cpal::default_host().default_input_device().ok_or_else(|| anyhow::anyhow!("no microphone found"))?;
    let cfg = dev.default_input_config()?;
    let (rate, ch) = (cfg.sample_rate() as f64, cfg.channels() as usize);
    let step = rate / RATE as f64;
    let (mut pos, mut acc, mut last_loud) = (0f64, Vec::with_capacity(CHUNK), Instant::now() - HANGOVER);
    let mut feed = move |mono: &mut dyn Iterator<Item = f32>| {
        for s in mono {
            pos += 1.0;
            if pos >= step {
                pos -= step;
                acc.push((s.clamp(-1.0, 1.0) * 32767.0) as i16);
                if acc.len() == CHUNK {
                    let c = std::mem::replace(&mut acc, Vec::with_capacity(CHUNK));
                    if level(&c) > LOUD {
                        last_loud = Instant::now();
                    }
                    if last_loud.elapsed() < HANGOVER {
                        inner.speaking.lock().unwrap().insert("You".into(), Instant::now());
                        on_chunk(c);
                    }
                }
            }
        }
    };
    let err = |e| crate::plog!("mic error: {e}");
    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => dev.build_input_stream(cfg.config(), move |d: &[f32], _| feed(&mut d.chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32)), err, None)?,
        cpal::SampleFormat::I16 => dev.build_input_stream(cfg.config(), move |d: &[i16], _| feed(&mut d.chunks(ch).map(|f| f.iter().map(|s| *s as f32 / 32768.0).sum::<f32>() / ch as f32)), err, None)?,
        f => anyhow::bail!("unsupported microphone format {f:?}"),
    };
    stream.play()?;
    Ok(stream)
}

impl Voice {
    /// `send` is called with each chunk of our microphone audio while the mic is on.
    pub fn start(send: impl Fn(Vec<i16>) + Send + Sync + 'static) -> Voice {
        let inner = Arc::new(Inner {
            mic: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            sources: Mutex::new(HashMap::new()),
            out_fmt: Mutex::new(None),
            speaking: Mutex::new(HashMap::new()),
            error: Mutex::new(None),
        });
        let i2 = inner.clone();
        let send = Arc::new(send);
        // cpal streams can't move between threads: this thread owns both of them.
        std::thread::spawn(move || {
            let _out = (|| -> anyhow::Result<cpal::Stream> {
                let dev = cpal::default_host().default_output_device().ok_or_else(|| anyhow::anyhow!("no speakers"))?;
                let cfg = dev.default_output_config()?;
                *i2.out_fmt.lock().unwrap() = Some((cfg.sample_rate() as f64, cfg.channels() as usize));
                let i3 = i2.clone();
                let s = dev.build_output_stream(
                    cfg.config(),
                    move |out: &mut [f32], _| {
                        let mut src = i3.sources.lock().unwrap();
                        for o in out.iter_mut() {
                            *o = src.values_mut().map(|b| b.pop().unwrap_or(0.0)).sum::<f32>().clamp(-1.0, 1.0);
                        }
                    },
                    |e| crate::plog!("voice output error: {e}"),
                    None,
                )?;
                s.play()?;
                Ok(s)
            })()
            .map_err(|e| *i2.error.lock().unwrap() = Some(format!("{e:#}")))
            .ok();
            let mut mic: Option<cpal::Stream> = None;
            while !i2.stop.load(SeqCst) {
                let want = i2.mic.load(SeqCst);
                if want && mic.is_none() {
                    let s = send.clone();
                    match open_mic(i2.clone(), move |c| s(c)) {
                        Ok(m) => mic = Some(m),
                        Err(e) => {
                            *i2.error.lock().unwrap() = Some(format!("Microphone: {e:#}"));
                            i2.mic.store(false, SeqCst);
                        }
                    }
                } else if !want {
                    mic = None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        Voice(inner)
    }

    pub fn set_mic(&self, on: bool) {
        *self.0.error.lock().unwrap() = None;
        self.0.mic.store(on, SeqCst);
    }

    pub fn mic_on(&self) -> bool {
        self.0.mic.load(SeqCst)
    }

    /// Someone else's voice (24 kHz mono).
    pub fn play(&self, from: &str, pcm: &[i16]) {
        let Some((out_rate, out_ch)) = *self.0.out_fmt.lock().unwrap() else { return };
        if level(pcm) > LOUD {
            self.0.speaking.lock().unwrap().insert(from.to_string(), Instant::now());
        }
        let stereo: Vec<i16> = pcm.iter().flat_map(|s| [*s, *s]).collect();
        let mut src = self.0.sources.lock().unwrap();
        src.entry(from.to_string()).or_insert_with(|| PlayBuf::new(RATE, out_rate, out_ch)).push(&stereo);
    }

    /// Names heard in the last moment ("You" = our own mic).
    pub fn speaking(&self) -> Vec<String> {
        let mut s = self.0.speaking.lock().unwrap();
        s.retain(|_, t| t.elapsed() < Duration::from_millis(400));
        let mut v: Vec<String> = s.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn error(&self) -> Option<String> {
        self.0.error.lock().unwrap().clone()
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        self.0.stop.store(true, SeqCst);
    }
}
