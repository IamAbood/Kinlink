//! Client: punches through to the host's candidate addresses, then runs the encrypted session:
//! decode video, play audio (with volume), send input/gamepads, sync clipboard.
use crate::clip::Clip;
use crate::files::{Progress, Transfers};
use crate::codec::{Frame, VideoDecoder};
use crate::crypto::{self, Cipher, PKT_PUNCH};
use crate::net::{local_ip, now_us};
#[allow(unused_imports)]
use crate::proto::{decode, encode, Input, Msg, PadState, PAD_PLAYSTATION, PAD_XBOX, VERSION};
use std::collections::BTreeMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::*};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Connecting,
    Connected { monitors: Vec<String>, current: u8, mouse: bool, keyboard: bool, gamepad: bool },
    Failed(String),
}

/// Host cursor as last reported (position 0..1 within the streamed monitor).
#[derive(Clone, Copy, Debug)]
pub struct RemoteCursor {
    pub x: f32,
    pub y: f32,
    pub visible: bool,
}

pub struct CursorShape {
    pub version: u32,
    pub w: usize,
    pub h: usize,
    pub hx: f32,
    pub hy: f32,
    pub rgba: Vec<u8>,
}

#[derive(Default, Clone)]
pub struct Stats {
    pub fps: f64,
    pub mbps: f64,
    pub rtt_ms: f64,
    pub decode_ms: f64,
    /// Capture -> decoded. Only meaningful when host and client share a clock (same PC).
    pub e2e_ms: f64,
    pub lost: u32,
    pub audio_packets: u32,
}

/// Binds the client socket and learns its public address (STUN, ~1 s). Call off the UI thread.
pub fn prepare_socket() -> std::io::Result<(UdpSocket, Vec<SocketAddr>)> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    let port = sock.local_addr()?.port();
    let mut cands = Vec::new();
    match crate::stun::query(&sock, Duration::from_millis(1500)) {
        Some(a) => {
            crate::plog!("connect: my public address is {a} (STUN)");
            cands.push(a);
        }
        None => crate::plog!("connect: STUN failed (no public address; only LAN will work)"),
    }
    if let Some(ip) = local_ip(false) {
        cands.push(SocketAddr::new(ip, port));
    }
    crate::plog!("connect: local UDP port {port}, my addresses {cands:?}");
    Ok((sock, cands))
}

struct Shared {
    stop: AtomicBool,
    sock: UdpSocket,
    cipher: Cipher,
    targets: Vec<SocketAddr>,
    peer: String,
    host: Mutex<Option<SocketAddr>>,
    state: Mutex<State>,
    frame: Mutex<Option<Arc<Frame>>>,
    /// Last decoded frame (for screenshots).
    last: Mutex<Option<Arc<Frame>>>,
    /// 1 = start recording, 2 = stop (handled on the receive thread).
    rec_cmd: AtomicU32,
    recording: AtomicBool,
    /// Results for the UI ("Recording saved to ...").
    notices: Mutex<Vec<String>>,
    stats: Mutex<Stats>,
    lost: AtomicU32,
    last_pong: Mutex<Instant>,
    clip: Mutex<Clip>,
    audio: Mutex<Option<Arc<Mutex<crate::audio::PlayBuf>>>>,
    volume: AtomicU32,
    files: Mutex<Transfers>,
    chat: Mutex<Vec<(String, String)>>,
    unread: AtomicU32,
    voice: Mutex<Option<crate::voice::Voice>>,
    /// For pad slots 4-7: (product id, index among identical Sony controllers) if PlayStation.
    ps_pads: Mutex<[Option<(u16, usize)>; 4]>,
    cursor: Mutex<Option<RemoteCursor>>,
    shape: Mutex<Option<Arc<CursorShape>>>,
    wake: Box<dyn Fn() + Send + Sync>,
    /// Last quality / sharp-text choice, re-sent once connected (sends before that go nowhere).
    quality: Mutex<Option<(u32, u32)>>,
    sharp: AtomicBool,
}

impl Shared {
    fn send(&self, m: &Msg) {
        if let Some(h) = *self.host.lock().unwrap() {
            let _ = self.sock.send_to(&self.cipher.seal(&encode(m)), h);
        }
    }
    fn fail(&self, why: String) {
        crate::plog!("session: ended: {why}");
        *self.state.lock().unwrap() = State::Failed(why);
        self.stop.store(true, SeqCst);
        (self.wake)();
    }
}

pub struct Client {
    sh: Arc<Shared>,
}

impl Client {
    /// `sock` from prepare_socket; `targets` = host candidates from signaling; `peer` = friend's name.
    pub fn start(sock: UdpSocket, targets: Vec<SocketAddr>, cipher: Cipher, peer: String, wake: impl Fn() + Send + Sync + 'static) -> Client {
        let _ = sock.set_read_timeout(Some(Duration::from_millis(30)));
        crate::plog!("connect: host {peer} said yes; its addresses {targets:?}; punching through...");
        let sh = Arc::new(Shared {
            stop: AtomicBool::new(false),
            sock,
            cipher,
            targets: targets.into_iter().filter(|a| a.is_ipv4()).collect(), // ponytail: client is IPv4-only
            peer,
            host: Mutex::new(None),
            state: Mutex::new(State::Connecting),
            frame: Mutex::new(None),
            last: Mutex::new(None),
            rec_cmd: AtomicU32::new(0),
            recording: AtomicBool::new(false),
            notices: Mutex::new(Vec::new()),
            stats: Mutex::new(Stats::default()),
            lost: AtomicU32::new(0),
            last_pong: Mutex::new(Instant::now()),
            clip: Mutex::new(Clip::default()),
            audio: Mutex::new(None),
            volume: AtomicU32::new(100),
            files: Mutex::new(Transfers::default()),
            chat: Mutex::new(Vec::new()),
            unread: AtomicU32::new(0),
            voice: Mutex::new(None),
            ps_pads: Mutex::new([None; 4]),
            cursor: Mutex::new(None),
            shape: Mutex::new(None),
            quality: Mutex::new(None),
            sharp: AtomicBool::new(false),
            wake: Box::new(wake),
        });
        {
            let weak = Arc::downgrade(&sh);
            let v = crate::voice::Voice::start(move |pcm| {
                if let Some(s) = weak.upgrade() {
                    s.send(&Msg::Voice { from: String::new(), pcm });
                }
            });
            *sh.voice.lock().unwrap() = Some(v);
        }
        let s = sh.clone();
        thread::spawn(move || recv_loop(s));
        let s = sh.clone();
        thread::spawn(move || control_loop(s));
        let s = sh.clone();
        thread::spawn(move || gamepad_loop(s));
        let s = sh.clone();
        thread::spawn(move || {
            // File chunks/acks.
            while !s.stop.load(SeqCst) {
                thread::sleep(Duration::from_millis(5));
                if s.host.lock().unwrap().is_some() {
                    let msgs = s.files.lock().unwrap().pump();
                    for m in msgs {
                        s.send(&m);
                    }
                }
            }
        });
        Client { sh }
    }

    pub fn state(&self) -> State {
        self.sh.state.lock().unwrap().clone()
    }
    pub fn take_frame(&self) -> Option<Arc<Frame>> {
        self.sh.frame.lock().unwrap().take()
    }
    pub fn cursor(&self) -> (Option<RemoteCursor>, Option<Arc<CursorShape>>) {
        (*self.sh.cursor.lock().unwrap(), self.sh.shape.lock().unwrap().clone())
    }
    pub fn stats(&self) -> Stats {
        self.sh.stats.lock().unwrap().clone()
    }
    pub fn input(&self, i: Input) {
        self.sh.send(&Msg::Input(i));
    }
    pub fn select_monitor(&self, n: u8) {
        self.sh.send(&Msg::SelectMonitor(n));
        if let State::Connected { current, .. } = &mut *self.sh.state.lock().unwrap() {
            *current = n;
        }
    }
    pub fn screenshot(&self) -> Result<std::path::PathBuf, String> {
        let f = self.sh.last.lock().unwrap().clone().ok_or("No picture yet")?;
        crate::rec::save_png(&f).map_err(|e| format!("{e:#}"))
    }
    pub fn start_recording(&self) {
        self.sh.rec_cmd.store(1, SeqCst);
        self.sh.send(&Msg::KeyframeRequest); // recordings start on a full frame
    }
    pub fn stop_recording(&self) {
        self.sh.rec_cmd.store(2, SeqCst);
    }
    pub fn is_recording(&self) -> bool {
        self.sh.recording.load(SeqCst)
    }
    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut self.sh.notices.lock().unwrap())
    }

    /// Sends a file to the host.
    pub fn send_file(&self, path: &std::path::Path) -> Result<(), String> {
        let offer = self.sh.files.lock().unwrap().send(path)?;
        self.sh.send(&offer);
        Ok(())
    }
    pub fn cancel_file(&self, id: u32) {
        let m = self.sh.files.lock().unwrap().cancel(id);
        self.sh.send(&m);
    }
    /// Files the host offered that wait for Accept/Decline: (id, name, size).
    pub fn pending_files(&self) -> Vec<(u32, String, u64)> {
        self.sh.files.lock().unwrap().pending()
    }
    pub fn answer_file(&self, id: u32, accept: bool) {
        let msgs = {
            let mut f = self.sh.files.lock().unwrap();
            if accept { f.accept(id) } else { vec![f.decline(id)] }
        };
        for m in msgs {
            self.sh.send(&m);
        }
    }
    pub fn transfers(&self) -> Vec<Progress> {
        self.sh.files.lock().unwrap().active()
    }
    pub fn take_finished_files(&self) -> Vec<Progress> {
        self.sh.files.lock().unwrap().take_finished()
    }
    /// Sends voice samples directly (self-test: no microphone needed).
    pub fn send_voice(&self, pcm: Vec<i16>) {
        self.sh.send(&Msg::Voice { from: String::new(), pcm });
    }
    pub fn set_mic(&self, on: bool) {
        if let Some(v) = &*self.sh.voice.lock().unwrap() {
            v.set_mic(on);
        }
    }
    pub fn mic_on(&self) -> bool {
        self.sh.voice.lock().unwrap().as_ref().is_some_and(|v| v.mic_on())
    }
    pub fn voice_status(&self) -> (Vec<String>, Option<String>) {
        self.sh.voice.lock().unwrap().as_ref().map(|v| (v.speaking(), v.error())).unwrap_or_default()
    }
    pub fn say(&self, text: &str) {
        self.sh.send(&Msg::Chat { from: String::new(), text: text.to_string() });
        self.sh.chat.lock().unwrap().push(("You".into(), text.to_string()));
    }
    pub fn chat_log(&self) -> Vec<(String, String)> {
        self.sh.chat.lock().unwrap().clone()
    }
    /// Messages received since the last call.
    pub fn take_unread(&self) -> u32 {
        self.sh.unread.swap(0, SeqCst)
    }
    pub fn set_sharp(&self, on: bool) {
        self.sh.sharp.store(on, SeqCst);
        self.sh.send(&Msg::SetSharp(on));
    }

    pub fn set_quality(&self, max_height: u32, mbps: u32) {
        *self.sh.quality.lock().unwrap() = Some((max_height, mbps));
        self.sh.send(&Msg::SetQuality { max_height, mbps });
    }
    /// 0..=100
    pub fn set_volume(&self, v: u32) {
        self.sh.volume.store(v.min(100), SeqCst);
    }
    pub fn disconnect(&self) {
        self.sh.voice.lock().unwrap().take(); // stops mic + voice output
        for pad in 0..8 {
            rumble_pad(&self.sh, pad, 0, 0); // never leave a controller buzzing
        }
        self.sh.send(&Msg::Input(Input::ReleaseAll));
        self.sh.send(&Msg::Bye);
        self.sh.stop.store(true, SeqCst);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if !self.sh.stop.load(SeqCst) {
            self.disconnect();
        }
    }
}

/// Punch + Hello until welcomed, then ping + clipboard; detects a dead host.
fn control_loop(sh: Arc<Shared>) {
    let start = Instant::now();
    let mut last_clip = Instant::now();
    let punch = crypto::punch_packet(sh.cipher.session);
    let mut last_note = u64::MAX;
    while !sh.stop.load(SeqCst) {
        if sh.host.lock().unwrap().is_none() {
            if start.elapsed() > Duration::from_secs(15) {
                return sh.fail(format!(
                    "Couldn't reach {}'s PC. One of the two networks blocks direct connections (strict router, mobile or work network).",
                    sh.peer
                ));
            }
            let hello = sh.cipher.seal(&encode(&Msg::Hello { version: VERSION }));
            let mut errs = Vec::new();
            for t in &sh.targets {
                if let Err(e) = sh.sock.send_to(&punch, t) {
                    errs.push(format!("{t}: {e}"));
                }
                let _ = sh.sock.send_to(&hello, t);
            }
            let secs = start.elapsed().as_secs();
            if secs != last_note {
                last_note = secs;
                if sh.targets.is_empty() {
                    crate::plog!("connect: the host sent no addresses to try");
                } else if secs % 3 == 0 {
                    crate::plog!("connect: {secs}s, still no answer from {:?}{}", sh.targets, if errs.is_empty() { String::new() } else { format!(" (send errors: {})", errs.join(", ")) });
                }
            }
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        sh.send(&Msg::Ping { t: now_us(), lost: sh.lost.load(SeqCst) });
        if sh.last_pong.lock().unwrap().elapsed() > Duration::from_secs(6) {
            return sh.fail("Connection lost".into());
        }
        if last_clip.elapsed() > Duration::from_millis(400) {
            last_clip = Instant::now();
            let m = sh.clip.lock().unwrap().poll();
            if let Some(m) = m {
                sh.send(&m);
            }
            let rich = sh.clip.lock().unwrap().poll_rich();
            if let Some(rich) = rich {
                for (path, name) in crate::clip::marked_names(&rich) {
                    let offer = sh.files.lock().unwrap().send_as(&path, &name);
                    if let Ok(offer) = offer {
                        sh.send(&offer);
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
}

struct Assembly {
    parts: Vec<Option<Vec<u8>>>,
    got: u16,
    key: bool,
    t: u64,
    first_seen: Instant,
}

fn recv_loop(sh: Arc<Shared>) {
    let mut buf = vec![0u8; 65536];
    let mut decoder = match VideoDecoder::new() {
        Ok(d) => d,
        Err(e) => return sh.fail(format!("{e:#}")),
    };
    let mut frames: BTreeMap<u32, Assembly> = BTreeMap::new();
    let mut last_done: Option<u32> = None;
    let mut need_key = true;
    let mut last_key_req = Instant::now() - Duration::from_secs(1);
    let mut _audio_stream: Option<cpal::Stream> = None;
    let mut recorder: Option<crate::rec::Recorder> = None;
    let mut want_rec = false;
    let mut audio_rate_seen = 0u32;
    let mut seen_punch = false;
    let finish = |r: crate::rec::Recorder, why: &str| {
        let msg = match r.finish() {
            Ok(p) => format!("Recording saved{why}: {}", p.display()),
            Err(e) => format!("Recording failed: {e:#}"),
        };
        sh.notices.lock().unwrap().push(msg);
        sh.recording.store(false, SeqCst);
        (sh.wake)();
    };
    let (mut n, mut bytes, mut dec_time, mut e2e, mut tick) = (0u32, 0usize, 0f64, 0f64, Instant::now());

    let request_key = |last: &mut Instant| {
        if last.elapsed() > Duration::from_millis(250) {
            *last = Instant::now();
            sh.send(&Msg::KeyframeRequest);
        }
    };

    while !sh.stop.load(SeqCst) {
        match sh.rec_cmd.swap(0, SeqCst) {
            1 => want_rec = true,
            2 => {
                want_rec = false;
                if let Some(r) = recorder.take() {
                    finish(r, "");
                }
            }
            _ => {}
        }
        let res = sh.sock.recv_from(&mut buf);
        // A frame stuck incomplete (lost packet, static screen): ask for a fresh keyframe.
        if frames.values().next().is_some_and(|a| a.first_seen.elapsed() > Duration::from_millis(150)) {
            frames.clear();
            sh.lost.fetch_add(1, SeqCst);
            need_key = true;
            request_key(&mut last_key_req);
        }
        let Ok((len, from)) = res else { continue };
        let pkt = &buf[..len];
        if pkt.first() == Some(&PKT_PUNCH) {
            if sh.host.lock().unwrap().is_none() && !seen_punch {
                seen_punch = true;
                crate::plog!("connect: got a punch packet from {from} (path is open, waiting for welcome)");
            }
            continue;
        }
        let Some(msg) = sh.cipher.open(pkt).and_then(|p| decode(&p)) else { continue };
        let host = *sh.host.lock().unwrap();
        match msg {
            Msg::Welcome { monitors, current, audio_rate, mouse, keyboard, gamepad } => {
                audio_rate_seen = audio_rate;
                if host.is_none() {
                    crate::plog!("connect: CONNECTED via {from} (mouse {mouse}, keyboard {keyboard}, controller {gamepad}, audio {audio_rate} Hz)");
                    *sh.host.lock().unwrap() = Some(from);
                    *sh.last_pong.lock().unwrap() = Instant::now();
                    if let Some((max_height, mbps)) = *sh.quality.lock().unwrap() {
                        sh.send(&Msg::SetQuality { max_height, mbps });
                    }
                    if sh.sharp.load(SeqCst) {
                        sh.send(&Msg::SetSharp(true));
                    }
                    if audio_rate > 0 {
                        match crate::audio::start_player(audio_rate) {
                            Ok((s, b)) => {
                                _audio_stream = Some(s);
                                *sh.audio.lock().unwrap() = Some(b);
                            }
                            Err(e) => crate::plog!("audio playback unavailable: {e:#}"),
                        }
                    }
                }
                *sh.state.lock().unwrap() = State::Connected { monitors, current, mouse, keyboard, gamepad };
                (sh.wake)();
            }
            Msg::Reject { reason } => return sh.fail(reason),
            _ if host.is_none() => {}
            Msg::Pong { t } => {
                *sh.last_pong.lock().unwrap() = Instant::now();
                *sh.host.lock().unwrap() = Some(from); // follow NAT rebinding
                sh.stats.lock().unwrap().rtt_ms = now_us().saturating_sub(t) as f64 / 1000.0;
            }
            Msg::Audio { pcm, .. } => {
                if let Some(r) = recorder.as_mut() {
                    let _ = r.audio(&pcm);
                }
                sh.stats.lock().unwrap().audio_packets += 1;
                if let Some(b) = &*sh.audio.lock().unwrap() {
                    let v = sh.volume.load(SeqCst) as f32 / 100.0;
                    let mut b = b.lock().unwrap();
                    b.gain = v * v; // perceptual curve
                    b.push(&pcm);
                }
            }
            Msg::Rumble { pad, large, small } => rumble_pad(&sh, pad, large, small),
            m @ (Msg::FileOffer { .. } | Msg::FileChunk { .. } | Msg::FileAck { .. } | Msg::FileCancel { .. }) => {
                let replies = sh.files.lock().unwrap().on_msg(m);
                for r in replies {
                    sh.send(&r);
                }
                (sh.wake)();
            }
            Msg::Voice { from, pcm } => {
                if let Some(v) = &*sh.voice.lock().unwrap() {
                    v.play(&from, &pcm);
                }
            }
            Msg::Chat { from, text } => {
                sh.chat.lock().unwrap().push((from, text));
                sh.unread.fetch_add(1, SeqCst);
                (sh.wake)();
            }
            Msg::CursorShape { version, w, h, hx, hy, rgba } => {
                if rgba.len() == w as usize * h as usize * 4 {
                    *sh.shape.lock().unwrap() = Some(Arc::new(CursorShape { version, w: w as usize, h: h as usize, hx: hx as f32, hy: hy as f32, rgba }));
                    (sh.wake)();
                }
            }
            Msg::CursorPos { x, y, visible, .. } => {
                *sh.cursor.lock().unwrap() = Some(RemoteCursor { x, y, visible });
                (sh.wake)();
            }
            Msg::Clipboard { seq, text } => {
                let ack = sh.clip.lock().unwrap().on_remote(seq, text);
                sh.send(&ack);
            }
            Msg::ClipboardAck { seq } => sh.clip.lock().unwrap().on_ack(seq),
            Msg::Video { frame, idx, count, key, t, data } => {
                if last_done.is_some_and(|d| frame <= d) || idx >= count {
                    continue;
                }
                bytes += data.len();
                let a = frames.entry(frame).or_insert_with(|| Assembly {
                    parts: vec![None; count as usize],
                    got: 0,
                    key,
                    t,
                    first_seen: Instant::now(),
                });
                if a.parts.len() != count as usize || a.parts[idx as usize].is_some() {
                    continue;
                }
                a.parts[idx as usize] = Some(data);
                a.got += 1;
                if a.got < count {
                    continue;
                }
                // Frame complete. Anything older still pending is lost.
                let a = frames.remove(&frame).unwrap();
                let older: Vec<u32> = frames.range(..frame).map(|(k, _)| *k).collect();
                let gap = last_done.is_some_and(|d| frame != d.wrapping_add(1)) || !older.is_empty();
                for k in older {
                    frames.remove(&k);
                }
                last_done = Some(frame);
                if gap && !a.key {
                    sh.lost.fetch_add(1, SeqCst);
                    need_key = true;
                }
                if a.key {
                    need_key = false;
                }
                if need_key {
                    request_key(&mut last_key_req);
                    continue;
                }
                let data: Vec<u8> = a.parts.into_iter().flatten().flatten().collect();
                let t0 = Instant::now();
                match decoder.decode(&data) {
                    Ok(Some(f)) => {
                        if recorder.as_ref().is_some_and(|r| r.size != (f.w, f.h)) {
                            finish(recorder.take().unwrap(), " (resolution changed)");
                        }
                        if want_rec && a.key && recorder.is_none() {
                            match crate::rec::Recorder::start(&data, f.w, f.h, audio_rate_seen) {
                                Ok(r) => {
                                    recorder = Some(r);
                                    sh.recording.store(true, SeqCst);
                                }
                                Err(e) => sh.notices.lock().unwrap().push(format!("Recording failed: {e:#}")),
                            }
                            want_rec = false;
                        } else if let Some(r) = recorder.as_mut() {
                            let _ = r.video(&data, a.key);
                        }
                        let f = Arc::new(f);
                        *sh.last.lock().unwrap() = Some(f.clone());
                        dec_time += t0.elapsed().as_secs_f64() * 1000.0;
                        e2e += now_us().saturating_sub(a.t) as f64 / 1000.0;
                        n += 1;
                        *sh.frame.lock().unwrap() = Some(f);
                        (sh.wake)();
                    }
                    Ok(None) => {}
                    Err(_) => {
                        need_key = true;
                        request_key(&mut last_key_req);
                    }
                }
            }
            _ => {}
        }
        if tick.elapsed() >= Duration::from_secs(1) {
            let secs = tick.elapsed().as_secs_f64();
            let mut st = sh.stats.lock().unwrap();
            st.fps = n as f64 / secs;
            st.mbps = bytes as f64 * 8.0 / secs / 1e6;
            st.decode_ms = dec_time / n.max(1) as f64;
            st.e2e_ms = e2e / n.max(1) as f64;
            st.lost = sh.lost.load(SeqCst);
            (n, bytes, dec_time, e2e, tick) = (0, 0, 0.0, 0.0, Instant::now());
        }
    }
    if let Some(r) = recorder.take() {
        finish(r, "");
    }
}

#[cfg(not(windows))]
fn rumble_pad(_sh: &Shared, _pad: u8, _large: u8, _small: u8) {}

#[cfg(not(windows))]
fn gamepad_loop(_sh: Arc<Shared>) {} // phone: no controllers in v1

/// Rumble for any local pad: Xbox-style via XInput, PlayStation via its HID report.
#[cfg(windows)]
fn rumble_pad(sh: &Shared, pad: u8, large: u8, small: u8) {
    if pad < 4 {
        return rumble(pad, large, small);
    }
    let ps = sh.ps_pads.lock().unwrap().get(pad as usize - 4).copied().flatten();
    if let Some((product, nth)) = ps {
        let _ = crate::padout::rumble(product, nth, large, small);
    }
}

/// Vibrates a local Xbox-style controller (XInput slots 0-3).
#[cfg(windows)]
fn rumble(pad: u8, large: u8, small: u8) {
    use windows::Win32::UI::Input::XboxController::{XInputSetState, XINPUT_VIBRATION};
    if pad < 4 {
        let v = XINPUT_VIBRATION { wLeftMotorSpeed: large as u16 * 257, wRightMotorSpeed: small as u16 * 257 };
        unsafe {
            XInputSetState(pad as u32, &v);
        }
    }
}

#[cfg(windows)]
/// gilrs button -> XInput bit (so every pad travels as an XInput-style state).
const GILRS_MAP: [(gilrs::Button, u16); 15] = [
    (gilrs::Button::DPadUp, 0x0001),
    (gilrs::Button::DPadDown, 0x0002),
    (gilrs::Button::DPadLeft, 0x0004),
    (gilrs::Button::DPadRight, 0x0008),
    (gilrs::Button::Start, 0x0010),
    (gilrs::Button::Select, 0x0020),
    (gilrs::Button::LeftThumb, 0x0040),
    (gilrs::Button::RightThumb, 0x0080),
    (gilrs::Button::LeftTrigger, 0x0100),
    (gilrs::Button::RightTrigger, 0x0200),
    (gilrs::Button::Mode, 0x0400),
    (gilrs::Button::South, 0x1000),
    (gilrs::Button::East, 0x2000),
    (gilrs::Button::West, 0x4000),
    (gilrs::Button::North, 0x8000),
];

/// Controller button remap (Settings): entry i = the button bit that physical button bit i sends.
pub static PAD_MAP: std::sync::Mutex<[u8; 16]> = std::sync::Mutex::new(IDENTITY_MAP);
pub const IDENTITY_MAP: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// XInput button bits after the user's remap.
pub fn remap(buttons: u16) -> u16 {
    let map = *PAD_MAP.lock().unwrap();
    (0..16).filter(|i| buttons & (1 << i) != 0).fold(0, |out, i| out | 1 << (map[i] & 15))
}

/// Polls local controllers and forwards them (on change, plus a refresh every 100 ms for loss).
/// Xbox-style pads come from XInput (ids 0-3, rumble capable); PlayStation and other pads come
/// from Windows.Gaming.Input via gilrs (ids 4-7) and are flagged so the host can show a DS4.
#[cfg(windows)]
fn gamepad_loop(sh: Arc<Shared>) {
    use windows::Win32::UI::Input::XboxController::{XInputGetState, XINPUT_STATE};
    let mut last: [Option<(PadState, Instant)>; 8] = [None; 8];
    let mut gil = gilrs::Gilrs::new().ok();
    let mut slots: Vec<Option<gilrs::GamepadId>> = vec![None; 4];
    let send = |sh: &Shared, last: &mut Option<(PadState, Instant)>, id: u8, s: Option<PadState>| match s {
        None => {
            if last.take().is_some() {
                sh.send(&Msg::Input(Input::PadRemove { id }));
            }
        }
        Some(mut s) => {
            s.buttons = remap(s.buttons);
            if last.is_none_or(|(p, t)| p != s || t.elapsed() > Duration::from_millis(100)) {
                *last = Some((s, Instant::now()));
                sh.send(&Msg::Input(Input::Pad { id, state: s }));
            }
        }
    };
    while !sh.stop.load(SeqCst) {
        if sh.host.lock().unwrap().is_some() {
            for id in 0..4u32 {
                let mut x = XINPUT_STATE::default();
                let s = (unsafe { XInputGetState(id, &mut x) } == 0).then(|| {
                    let g = x.Gamepad;
                    PadState { buttons: g.wButtons.0, lt: g.bLeftTrigger, rt: g.bRightTrigger, lx: g.sThumbLX, ly: g.sThumbLY, rx: g.sThumbRX, ry: g.sThumbRY, kind: PAD_XBOX }
                });
                send(&sh, &mut last[id as usize], id as u8, s);
            }
            if let Some(g) = gil.as_mut() {
                while g.next_event().is_some() {}
                // ponytail: Xbox pads are skipped by vendor/name (XInput already has them); exotic XInput clones may show twice
                let others: Vec<(gilrs::GamepadId, bool)> = g
                    .gamepads()
                    .filter(|(_, p)| p.is_connected() && p.vendor_id() != Some(0x045E) && !p.name().to_lowercase().contains("xbox"))
                    .map(|(id, p)| (id, p.vendor_id() == Some(0x054C)))
                    .collect();
                for slot in slots.iter_mut() {
                    if slot.is_some_and(|id| !others.iter().any(|(o, _)| *o == id)) {
                        *slot = None;
                    }
                }
                for (id, _) in &others {
                    if !slots.contains(&Some(*id)) {
                        if let Some(free) = slots.iter_mut().find(|s| s.is_none()) {
                            *free = Some(*id);
                        }
                    }
                }
                {
                    // Which physical PlayStation controller each slot is (for rumble).
                    let mut ps = sh.ps_pads.lock().unwrap();
                    let mut seen: Vec<u16> = Vec::new();
                    for (i, slot) in slots.iter().enumerate() {
                        ps[i] = slot.and_then(|id| {
                            let p = g.connected_gamepad(id)?;
                            let prod = p.product_id()?;
                            (p.vendor_id() == Some(0x054C)).then(|| {
                                let nth = seen.iter().filter(|x| **x == prod).count();
                                seen.push(prod);
                                (prod, nth)
                            })
                        });
                    }
                }
                for (i, slot) in slots.iter().enumerate() {
                    let s = slot.and_then(|id| {
                        let p = g.connected_gamepad(id)?;
                        let mut buttons = 0;
                        for (b, bit) in GILRS_MAP {
                            if p.is_pressed(b) {
                                buttons |= bit;
                            }
                        }
                        let trig = |b| (p.button_data(b).map(|d| d.value()).unwrap_or(0.0) * 255.0) as u8;
                        let ax = |a| (p.value(a).clamp(-1.0, 1.0) * 32767.0) as i16;
                        let ps = others.iter().any(|(o, sony)| *o == id && *sony);
                        Some(PadState {
                            buttons,
                            lt: trig(gilrs::Button::LeftTrigger2),
                            rt: trig(gilrs::Button::RightTrigger2),
                            lx: ax(gilrs::Axis::LeftStickX),
                            ly: ax(gilrs::Axis::LeftStickY),
                            rx: ax(gilrs::Axis::RightStickX),
                            ry: ax(gilrs::Axis::RightStickY),
                            kind: if ps { PAD_PLAYSTATION } else { PAD_XBOX },
                        })
                    });
                    send(&sh, &mut last[4 + i], 4 + i as u8, s);
                }
            }
        }
        thread::sleep(Duration::from_millis(4));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn remap_swaps_a_and_b() {
        assert_eq!(super::remap(0x1001), 0x1001); // unchanged by default
        let mut m = super::IDENTITY_MAP;
        (m[12], m[13]) = (13, 12); // A <-> B
        *super::PAD_MAP.lock().unwrap() = m;
        assert_eq!(super::remap(0x1000), 0x2000);
        assert_eq!(super::remap(0x2001), 0x1001);
        *super::PAD_MAP.lock().unwrap() = super::IDENTITY_MAP;
    }
}
