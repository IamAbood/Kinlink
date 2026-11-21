//! Host: approved friends join over encrypted, hole-punched UDP sessions. Several guests can be
//! connected at once (couch co-op): the screen is encoded once and sent to everyone.
use crate::capture::{monitors, Capturer};
use crate::clip::Clip;
use crate::files::{Progress, Transfers};
use crate::codec::new_encoder;
use crate::crypto::{self, Cipher, PKT_PUNCH};
use crate::input::Injector;
use crate::proto::{self, decode, encode, Input, Msg, CHUNK, VERSION};
use crate::{scale, stun};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering::*};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const MAX_GUESTS: usize = 8;

#[derive(Clone, Debug)]
pub struct HostConfig {
    /// Shown as the sender of the host's chat messages.
    pub name: String,
    pub port: u16,
    pub max_mbps: u32,
    pub fps: u32,
    pub hardware: bool,
    pub upnp: bool,
    /// Record what guests see to Videos\Kinlink.
    pub record: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Perms {
    pub mouse: bool,
    pub keyboard: bool,
    pub gamepad: bool,
}

/// A connected controller as shown in the host UI (player order).
pub struct PadSlot {
    pub guest: String,
    pub pad: u8,
    pub playstation: bool,
    pub player: Option<u32>,
}

#[derive(Clone, Default, Debug)]
pub struct HostStatus {
    /// Names of connected guests.
    pub clients: Vec<String>,
    pub encoder: String,
    pub stats: String,
    pub error: Option<String>,
    pub notes: Vec<String>,
    /// Recent chat (from, text), oldest first.
    pub chat: Vec<(String, String)>,
    /// (guest, transfer) for transfers in progress.
    pub transfers: Vec<(String, Progress)>,
    /// Transfers that finished since the UI last looked (guest, transfer).
    pub finished: Vec<(String, Progress)>,
}

struct Pending {
    cipher: Arc<Cipher>,
    perms: Perms,
    name: String,
    expires: Instant,
}

struct Session {
    id: u64,
    addr: SocketAddr,
    sock: usize,
    last_seen: Instant,
    cipher: Arc<Cipher>,
    perms: Perms,
    name: String,
    clip: Clip,
    files: Transfers,
    max_height: u32,
    mbps: u32,
    sharp: bool,
    last_lost: u32,
}

struct Shared {
    cfg: HostConfig,
    port: u16,
    stop: AtomicBool,
    status: Mutex<HostStatus>,
    socks: Vec<UdpSocket>,
    sessions: Mutex<Vec<Session>>,
    pending: Mutex<HashMap<u64, Pending>>,
    keyframe: AtomicBool,
    monitor: AtomicU8,
    bitrate: AtomicU32,
    audio_rate: AtomicU32,
    injector: Mutex<Injector>,
    stun_wait: Mutex<Option<([u8; 12], Sender<SocketAddr>)>>,
    upnp_ext: Mutex<Option<SocketAddr>>,
    voice: Mutex<Option<crate::voice::Voice>>,
    /// Connection to the lock-screen helper (only while guests are connected).
    helper: Arc<Mutex<Option<crate::syshelper::Link>>>,
    /// Guests watching from a browser (see web.rs).
    web: Mutex<Vec<WebGuest>>,
}

struct WebGuest {
    id: u64,
    /// Shown in the guest list: "name (browser)".
    name: String,
    tx: std::sync::mpsc::SyncSender<Arc<crate::web::Frame>>,
    alive: Arc<AtomicBool>,
}

impl Shared {
    /// Sends to every guest, each with its own encryption.
    fn broadcast(&self, m: &Msg) {
        let plain = encode(m);
        for s in self.sessions.lock().unwrap().iter() {
            let _ = self.socks[s.sock].send_to(&s.cipher.seal(&plain), s.addr);
        }
    }

    fn end_session(&self, id: u64, why: &str) {
        let removed = {
            let mut ss = self.sessions.lock().unwrap();
            let before = ss.len();
            ss.retain(|s| s.id != id);
            before != ss.len()
        };
        if removed {
            self.injector.lock().unwrap().release(id);
            self.refresh_clients(Some(why));
        }
    }

    fn refresh_clients(&self, why: Option<&str>) {
        let mut names: Vec<String> = self.sessions.lock().unwrap().iter().map(|s| s.name.clone()).collect();
        names.extend(self.web.lock().unwrap().iter().map(|g| g.name.clone()));
        let mut st = self.status.lock().unwrap();
        if names.is_empty() {
            if let Some(w) = why {
                st.stats = format!("Session ended ({w})");
            }
        }
        st.clients = names;
    }

    /// Logs a chat line and relays it to every guest except its sender.
    fn chat(&self, from: &str, text: &str, except: Option<u64>) {
        {
            let mut st = self.status.lock().unwrap();
            st.chat.push((from.to_string(), text.to_string()));
            let n = st.chat.len();
            if n > 100 {
                st.chat.drain(..n - 100);
            }
        }
        let plain = encode(&Msg::Chat { from: from.to_string(), text: text.to_string() });
        for s in self.sessions.lock().unwrap().iter().filter(|s| Some(s.id) != except) {
            let _ = self.socks[s.sock].send_to(&s.cipher.seal(&plain), s.addr);
        }
    }

    /// Lowest common quality across guests so everyone can keep up: (max_height, max_bitrate bps).
    fn quality(&self) -> (u32, u32) {
        // Browser guests: 1080p at up to 8 Mbps (no quality picker there, and WebRTC over the
        // internet copes badly with more).
        let web = !self.web.lock().unwrap().is_empty();
        let ss = self.sessions.lock().unwrap();
        let h = ss.iter().map(|s| s.max_height).filter(|h| *h > 0).min().unwrap_or(if web { 1080 } else { 0 });
        let mbps = ss.iter().map(|s| s.mbps).min().unwrap_or(self.cfg.max_mbps);
        let mbps = if web { mbps.min(8) } else { mbps };
        (h, mbps * 1_000_000)
    }

    /// Frame rate to stream at: sharp-text mode (any guest) caps it at 30 so every frame gets
    /// twice the bits, which keeps small text crisp.
    fn fps(&self) -> u32 {
        let sharp = self.sessions.lock().unwrap().iter().any(|s| s.sharp);
        if sharp { self.cfg.fps.clamp(1, 30) } else { self.cfg.fps.max(1) }
    }
}

/// Cheap handle for background threads (connection setup, punching).
#[derive(Clone)]
pub struct HostRef(Arc<Shared>);

pub struct Host {
    r: HostRef,
    threads: Vec<JoinHandle<()>>,
}

pub use crate::net::{local_ip, now_us};

fn bind(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    let s = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        s.set_only_v6(true)?;
    }
    let _ = s.set_recv_buffer_size(4 << 20);
    let _ = s.set_send_buffer_size(8 << 20);
    s.bind(&addr.into())?;
    let s: UdpSocket = s.into();
    s.set_read_timeout(Some(Duration::from_millis(100)))?;
    Ok(s)
}


impl Host {
    pub fn start(cfg: HostConfig) -> anyhow::Result<Host> {
        // Preferred port, else any free one (hole punching doesn't need a fixed port).
        let v4 = (0..10)
            .map(|i| cfg.port + i)
            .chain([0])
            .find_map(|p| bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), p)).ok())
            .ok_or_else(|| anyhow::anyhow!("no UDP port available"))?;
        let port = v4.local_addr()?.port();
        let mut socks = vec![v4];
        if let Ok(v6) = bind(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port)) {
            socks.push(v6);
        }
        let sh = Arc::new(Shared {
            bitrate: AtomicU32::new(cfg.max_mbps * 1_000_000),
            cfg,
            port,
            stop: AtomicBool::new(false),
            status: Mutex::new(HostStatus::default()),
            socks,
            sessions: Mutex::new(Vec::new()),
            pending: Mutex::new(HashMap::new()),
            keyframe: AtomicBool::new(false),
            monitor: AtomicU8::new(0),
            audio_rate: AtomicU32::new(0),
            injector: Mutex::new(Injector::default()),
            stun_wait: Mutex::new(None),
            upnp_ext: Mutex::new(None),
            voice: Mutex::new(None),
            helper: Arc::new(Mutex::new(None)),
            web: Mutex::new(Vec::new()),
        });
        sh.injector.lock().unwrap().helper = Some(sh.helper.clone());
        {
            // Host's microphone goes to every guest.
            let weak = Arc::downgrade(&sh);
            let v = crate::voice::Voice::start(move |pcm| {
                if let Some(s) = weak.upgrade() {
                    let name = s.cfg.name.clone();
                    s.broadcast(&Msg::Voice { from: name, pcm });
                }
            });
            *sh.voice.lock().unwrap() = Some(v);
        }
        let mut threads = Vec::new();
        for i in 0..sh.socks.len() {
            let s = sh.clone();
            threads.push(thread::spawn(move || net_loop(s, i)));
        }
        let s = sh.clone();
        threads.push(thread::spawn(move || video_loop(s)));
        let s = sh.clone();
        threads.push(thread::spawn(move || audio_loop(s)));
        let s = sh.clone();
        threads.push(thread::spawn(move || clip_loop(s)));
        let s = sh.clone();
        threads.push(thread::spawn(move || upnp(s)));
        let s = sh.clone();
        threads.push(thread::spawn(move || file_loop(s)));
        let (rtx, rrx) = channel::<crate::input::Rumble>();
        sh.injector.lock().unwrap().rumble_tx = Some(rtx);
        let s = sh.clone();
        threads.push(thread::spawn(move || {
            // Game vibration on a virtual pad -> that guest's real controller.
            while !s.stop.load(SeqCst) {
                if let Ok((g, pad, large, small)) = rrx.recv_timeout(Duration::from_millis(200)) {
                    let target = s.sessions.lock().unwrap().iter().find(|x| x.id == g).map(|x| (x.sock, x.addr, x.cipher.clone()));
                    if let Some((sock, addr, cipher)) = target {
                        let _ = s.socks[sock].send_to(&cipher.seal(&encode(&Msg::Rumble { pad, large, small })), addr);
                    }
                }
            }
        }));
        Ok(Host { r: HostRef(sh), threads })
    }

    pub fn handle(&self) -> HostRef {
        self.r.clone()
    }

    pub fn status(&self) -> HostStatus {
        self.r.0.status.lock().unwrap().clone()
    }

    /// Disconnects one guest by name (host's "Disconnect" button).
    pub fn kick(&self, name: &str) {
        let sh = &self.r.0;
        for g in sh.web.lock().unwrap().iter().filter(|g| g.name == name) {
            g.alive.store(false, SeqCst);
        }
        let found = sh.sessions.lock().unwrap().iter().find(|s| s.name == name).map(|s| (s.id, s.sock, s.addr, s.cipher.clone()));
        if let Some((id, sock, addr, cipher)) = found {
            let bye = encode(&Msg::Reject { reason: "The host ended the session".into() });
            let _ = sh.socks[sock].send_to(&cipher.seal(&bye), addr);
            sh.end_session(id, "ended by you");
        }
    }

    /// Sends a file to every connected guest.
    pub fn send_file(&self, path: &std::path::Path) -> Result<usize, String> {
        let sh = &self.r.0;
        let mut sent = 0;
        for s in sh.sessions.lock().unwrap().iter_mut() {
            let offer = s.files.send(path)?;
            let _ = sh.socks[s.sock].send_to(&s.cipher.seal(&encode(&offer)), s.addr);
            sent += 1;
        }
        Ok(sent)
    }

    /// Files guests offered that wait for Accept/Decline: (guest, id, name, size).
    pub fn pending_files(&self) -> Vec<(String, u32, String, u64)> {
        let sh = &self.r.0;
        sh.sessions.lock().unwrap().iter().flat_map(|s| s.files.pending().into_iter().map(move |(id, n, sz)| (s.name.clone(), id, n, sz))).collect()
    }
    pub fn answer_file(&self, guest: &str, id: u32, accept: bool) {
        let sh = &self.r.0;
        for s in sh.sessions.lock().unwrap().iter_mut().filter(|s| s.name == guest) {
            let msgs = if accept { s.files.accept(id) } else { vec![s.files.decline(id)] };
            for m in msgs {
                let _ = sh.socks[s.sock].send_to(&s.cipher.seal(&encode(&m)), s.addr);
            }
        }
    }

    pub fn cancel_file(&self, guest: &str, id: u32) {
        let sh = &self.r.0;
        for s in sh.sessions.lock().unwrap().iter_mut().filter(|s| s.name == guest) {
            let m = s.files.cancel(id);
            let _ = sh.socks[s.sock].send_to(&s.cipher.seal(&encode(&m)), s.addr);
        }
    }

    pub fn set_mic(&self, on: bool) {
        if let Some(v) = &*self.r.0.voice.lock().unwrap() {
            v.set_mic(on);
        }
    }
    pub fn mic_on(&self) -> bool {
        self.r.0.voice.lock().unwrap().as_ref().is_some_and(|v| v.mic_on())
    }
    /// (who is talking right now, microphone error if any)
    pub fn voice_status(&self) -> (Vec<String>, Option<String>) {
        self.r.0.voice.lock().unwrap().as_ref().map(|v| (v.speaking(), v.error())).unwrap_or_default()
    }

    /// Host types in chat: goes to every guest.
    pub fn say(&self, text: &str) {
        let name = self.r.0.cfg.name.clone();
        self.r.0.chat(&name, text, None);
    }

    /// Transfers that finished since last asked (for notifications).
    pub fn take_finished(&self) -> Vec<(String, Progress)> {
        std::mem::take(&mut self.r.0.status.lock().unwrap().finished)
    }

    /// Panic button: disconnects every guest at once.
    pub fn kick_all(&self) {
        let sh = &self.r.0;
        for g in sh.web.lock().unwrap().iter() {
            g.alive.store(false, SeqCst);
        }
        sh.broadcast(&Msg::Reject { reason: "The host ended the session".into() });
        let ids: Vec<u64> = sh.sessions.lock().unwrap().iter().map(|s| s.id).collect();
        for id in ids {
            sh.end_session(id, "panic button");
        }
        sh.pending.lock().unwrap().clear();
    }

    /// Applies changed friend permissions to a running session.
    pub fn set_perms(&self, name: &str, perms: Perms) {
        let mut drop = Vec::new();
        for s in self.r.0.sessions.lock().unwrap().iter_mut().filter(|s| s.name == name) {
            s.perms = perms;
            if !perms.gamepad {
                drop.push(s.id);
            }
        }
        let mut inj = self.r.0.injector.lock().unwrap();
        for id in drop {
            inj.drop_pads(id);
        }
    }

    /// Connected controllers in player order.
    pub fn pads(&self) -> Vec<PadSlot> {
        let names: HashMap<u64, String> = self.r.0.sessions.lock().unwrap().iter().map(|s| (s.id, s.name.clone())).collect();
        self.r.0.injector.lock().unwrap().pads().into_iter().map(|p| PadSlot {
            guest: names.get(&p.guest).cloned().unwrap_or_default(),
            pad: p.id,
            playstation: p.playstation,
            player: p.player,
        }).collect()
    }

    /// Moves a controller one player slot up or down.
    pub fn move_pad(&self, index: usize, up: bool) {
        self.r.0.injector.lock().unwrap().move_pad(index, up);
    }

    fn shutdown(&mut self) {
        let sh = &self.r.0;
        sh.broadcast(&Msg::Reject { reason: "The host closed the app".into() });
        sh.sessions.lock().unwrap().clear();
        sh.injector.lock().unwrap().release_everyone();
        sh.stop.store(true, SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        crate::web::set_accept(None);
        self.shutdown();
    }
}

impl HostRef {
    /// A link anyone can open in a browser to join (valid for an hour).
    pub fn web_link(&self, perms: Perms, watch_party: bool) -> String {
        let weak = Arc::downgrade(&self.0);
        crate::web::new_link(
            perms,
            watch_party,
            Arc::new(move |name, perms, offer| {
                let sh = weak.upgrade().ok_or("That PC isn't hosting right now.")?;
                HostRef(sh).add_web_guest(name, perms, offer)
            }),
        )
    }

    fn add_web_guest(&self, name: String, perms: Perms, offer: str0m::change::SdpOffer) -> Result<str0m::change::SdpAnswer, String> {
        if self.0.web.lock().unwrap().len() >= 8 {
            return Err("Sorry, it's full (8 people are already watching).".into());
        }
        let id: u64 = rand::random();
        let (tx, frames) = std::sync::mpsc::sync_channel(8);
        let alive = Arc::new(AtomicBool::new(true));
        let (w1, w2, w3) = (Arc::downgrade(&self.0), Arc::downgrade(&self.0), Arc::downgrade(&self.0));
        let answer = crate::web::serve(
            offer,
            crate::web::Guest {
                frames,
                alive: alive.clone(),
                on_input: Box::new(move |i| {
                    let Some(sh) = w1.upgrade() else { return };
                    let allowed = match i {
                        Input::Key { .. } => perms.keyboard,
                        Input::Pad { .. } | Input::PadRemove { .. } => perms.gamepad,
                        Input::ReleaseAll => true,
                        _ => perms.mouse,
                    };
                    if allowed {
                        sh.injector.lock().unwrap().apply(id, i);
                    }
                }),
                on_keyframe: Box::new(move || {
                    if let Some(sh) = w2.upgrade() {
                        sh.keyframe.store(true, SeqCst);
                    }
                }),
                on_end: Box::new(move || {
                    let Some(sh) = w3.upgrade() else { return };
                    sh.web.lock().unwrap().retain(|g| g.id != id);
                    sh.injector.lock().unwrap().release(id);
                    let (_, cap) = sh.quality();
                    sh.bitrate.store(cap, SeqCst);
                    sh.refresh_clients(Some("browser guest left"));
                    crate::plog!("web: browser guest left");
                }),
            },
        )?;
        let sh = &self.0;
        let label = format!("{name} (browser)");
        crate::plog!("web: {label} joined with a browser link");
        crate::notify::toast(&crate::i18n::tlf("{} joined from a browser", &[&name]), &crate::i18n::tl("Kick them from This PC if you didn't expect this."));
        sh.web.lock().unwrap().push(WebGuest { id, name: label, tx, alive });
        let (_, cap) = sh.quality();
        sh.bitrate.store(cap, SeqCst);
        sh.keyframe.store(true, SeqCst);
        sh.refresh_clients(None);
        Ok(answer)
    }

    /// Registers an approved session and returns the addresses the client should try.
    /// Blocks ~1-2 s for STUN; call off the UI thread.
    pub fn prepare(&self, cipher: Cipher, perms: Perms, name: String) -> Vec<SocketAddr> {
        let sh = &self.0;
        let sid = cipher.session;
        sh.pending.lock().unwrap().insert(sid, Pending { cipher: Arc::new(cipher), perms, name, expires: Instant::now() + Duration::from_secs(60) });
        let mut cands = Vec::new();
        if let Some(a) = self.stun() {
            cands.push(a);
        }
        if let Some(a) = *sh.upnp_ext.lock().unwrap() {
            cands.push(a);
        }
        if let Some(ip) = local_ip(false) {
            cands.push(SocketAddr::new(ip, sh.port));
        }
        if sh.socks.len() > 1 {
            if let Some(ip) = local_ip(true).filter(|ip| !proto::is_private(*ip)) {
                cands.push(SocketAddr::new(ip, sh.port));
            }
        }
        cands.dedup();
        crate::plog!("host: approved {}, my addresses {cands:?}", sh.pending.lock().unwrap().get(&sid).map(|p| p.name.clone()).unwrap_or_default());
        cands
    }

    /// Public address of the host's IPv4 socket, via STUN (response arrives in net_loop).
    fn stun(&self) -> Option<SocketAddr> {
        let txid: [u8; 12] = rand::random();
        let (tx, rx) = channel();
        *self.0.stun_wait.lock().unwrap() = Some((txid, tx));
        let req = stun::request(&txid);
        for s in stun::SERVERS {
            if let Some(a) = s.to_socket_addrs().ok().and_then(|mut it| it.find(|a| a.is_ipv4())) {
                let _ = self.0.socks[0].send_to(&req, a);
            }
        }
        let r = rx.recv_timeout(Duration::from_millis(1500)).ok();
        *self.0.stun_wait.lock().unwrap() = None;
        r
    }

    /// Fires punch packets at the client's candidates so both NATs open a path.
    pub fn punch(&self, addrs: Vec<SocketAddr>, session: u64) {
        crate::plog!("host: punching towards guest {addrs:?}");
        let sh = self.0.clone();
        thread::spawn(move || {
            let p = crypto::punch_packet(session);
            for _ in 0..120 {
                let live = sh.sessions.lock().unwrap().iter().any(|s| s.id == session);
                if live || sh.stop.load(SeqCst) {
                    break;
                }
                for a in &addrs {
                    let idx = if a.is_ipv6() { 1 } else { 0 };
                    if let Some(s) = sh.socks.get(idx) {
                        let _ = s.send_to(&p, a);
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
    }
}

// ---------------------------------------------------------------- network

fn net_loop(sh: Arc<Shared>, idx: usize) {
    let mut buf = vec![0u8; 65536];
    while !sh.stop.load(SeqCst) {
        if let Ok((n, from)) = sh.socks[idx].recv_from(&mut buf) {
            on_packet(&sh, idx, from, &buf[..n]);
        }
        let dead: Vec<u64> = sh.sessions.lock().unwrap().iter().filter(|s| s.last_seen.elapsed() > Duration::from_secs(6)).map(|s| s.id).collect();
        for id in dead {
            sh.end_session(id, "connection lost");
        }
        sh.pending.lock().unwrap().retain(|_, p| p.expires > Instant::now());
    }
}

fn on_packet(sh: &Arc<Shared>, idx: usize, from: SocketAddr, pkt: &[u8]) {
    if stun::is_stun(pkt) {
        if let Some((txid, addr)) = stun::parse(pkt) {
            if let Some((t, tx)) = &*sh.stun_wait.lock().unwrap() {
                if *t == txid {
                    let _ = tx.send(addr);
                }
            }
        }
        return;
    }
    let Some(sid) = crypto::session_of(pkt) else { return };
    if pkt[0] == PKT_PUNCH {
        // Answer punches for sessions we approved: helps open the client's NAT too.
        if sh.pending.lock().unwrap().contains_key(&sid) {
            crate::plog!("host: punch from guest at {from}");
            let _ = sh.socks[idx].send_to(&crypto::punch_packet(sid), from);
        }
        return;
    }
    let active = sh.sessions.lock().unwrap().iter().find(|s| s.id == sid).map(|s| s.cipher.clone());
    let (cipher, is_active) = match active {
        Some(c) => (c, true),
        None => match sh.pending.lock().unwrap().get(&sid) {
            Some(p) => (p.cipher.clone(), false),
            None => return,
        },
    };
    let Some(plain) = cipher.open(pkt) else { return };
    let Some(m) = decode(&plain) else { return };
    let reply = |m: &Msg| {
        let _ = sh.socks[idx].send_to(&cipher.seal(&encode(m)), from);
    };

    if let Msg::Hello { version } = m {
        if !is_active {
            crate::plog!("host: hello from {from} (app protocol {version}, mine {VERSION})");
        }
        if version != VERSION {
            return reply(&Msg::Reject { reason: "Different app versions: update both PCs".into() });
        }
        if !is_active {
            if sh.sessions.lock().unwrap().len() >= MAX_GUESTS {
                return reply(&Msg::Reject { reason: format!("This PC already has {MAX_GUESTS} guests") });
            }
            let Some(p) = sh.pending.lock().unwrap().remove(&sid) else { return };
            if from.ip().is_loopback() || Some(from.ip()) == local_ip(from.is_ipv6()) {
                sh.injector.lock().unwrap().no_pads.insert(sid);
            }
            sh.sessions.lock().unwrap().push(Session {
                id: sid,
                addr: from,
                sock: idx,
                last_seen: Instant::now(),
                cipher: p.cipher,
                perms: p.perms,
                name: p.name,
                clip: Clip::default(),
                files: Transfers::default(),
                max_height: 0,
                mbps: sh.cfg.max_mbps,
                sharp: false,
                last_lost: 0,
            });
            crate::plog!("host: guest connected from {from}");
            sh.keyframe.store(true, SeqCst); // the new guest needs a full picture
            sh.refresh_clients(None);
        }
        let perms = sh.sessions.lock().unwrap().iter().find(|s| s.id == sid).map(|s| s.perms).unwrap_or(Perms { mouse: false, keyboard: false, gamepad: false });
        let names = monitors().into_iter().map(|m| m.name).collect();
        return reply(&Msg::Welcome {
            monitors: names,
            current: sh.monitor.load(SeqCst),
            audio_rate: sh.audio_rate.load(SeqCst),
            mouse: perms.mouse,
            keyboard: perms.keyboard,
            gamepad: perms.gamepad,
        });
    }
    if !is_active {
        return;
    }
    let perms = {
        let mut ss = sh.sessions.lock().unwrap();
        let Some(s) = ss.iter_mut().find(|s| s.id == sid) else { return };
        s.last_seen = Instant::now();
        s.addr = from; // follow NAT rebinding
        s.sock = idx;
        s.perms
    };
    match m {
        Msg::Ping { t, lost } => {
            reply(&Msg::Pong { t });
            adapt_bitrate(sh, sid, lost);
        }
        Msg::Input(i) => {
            let allowed = match i {
                Input::Key { .. } => perms.keyboard,
                Input::Pad { .. } | Input::PadRemove { .. } => perms.gamepad,
                Input::ReleaseAll => true,
                _ => perms.mouse, // moves, buttons, wheel
            };
            if allowed {
                let mut inj = sh.injector.lock().unwrap();
                inj.apply(sid, i);
                if let (Input::Pad { .. }, Some(note)) = (i, inj.gamepad_status()) {
                    drop(inj);
                    let mut st = sh.status.lock().unwrap();
                    if !st.notes.contains(&note) {
                        st.notes.push(note);
                    }
                }
            }
        }
        Msg::KeyframeRequest => sh.keyframe.store(true, SeqCst),
        Msg::SelectMonitor(n) => {
            sh.monitor.store(n, SeqCst);
            sh.keyframe.store(true, SeqCst);
        }
        Msg::SetQuality { max_height, mbps } => {
            if let Some(s) = sh.sessions.lock().unwrap().iter_mut().find(|s| s.id == sid) {
                s.max_height = max_height;
                s.mbps = mbps.clamp(1, 150);
            }
            let (_, cap) = sh.quality();
            sh.bitrate.store(cap, SeqCst);
            sh.keyframe.store(true, SeqCst);
        }
        Msg::ListGames => {
            let names: Vec<String> = if perms.mouse && perms.keyboard { crate::games::installed().into_iter().map(|g| g.0).collect() } else { Vec::new() };
            if names.is_empty() {
                reply(&Msg::Games(Vec::new()));
            }
            // A few names per datagram.
            let mut batch: Vec<String> = Vec::new();
            for n in names {
                if batch.iter().map(|b| b.len() + 8).sum::<usize>() + n.len() > 900 {
                    reply(&Msg::Games(std::mem::take(&mut batch)));
                }
                batch.push(n.chars().take(120).collect());
            }
            if !batch.is_empty() {
                reply(&Msg::Games(batch));
            }
        }
        Msg::LaunchGame(name) => {
            let text = if !(perms.mouse && perms.keyboard) {
                "The host hasn't given you mouse and keyboard control.".to_string()
            } else {
                match crate::games::launch(&name) {
                    Ok(()) => format!("Starting {name}…"),
                    Err(e) => format!("Couldn't start {name}: {e}"),
                }
            };
            reply(&Msg::Info(text));
        }
        Msg::Shell(line) => {
            if !perms.keyboard {
                reply(&Msg::ShellOut("Not allowed: the host hasn't given you keyboard control.\n".into()));
            } else {
                crate::plog!("host: guest ran a command");
                let (sh2, cipher2) = (sh.clone(), cipher.clone());
                thread::spawn(move || {
                    let text = crate::games::run_command(&line);
                    for p in proto::pieces(&format!("{text}\n"), 900) {
                        let _ = sh2.socks[idx].send_to(&cipher2.seal(&encode(&Msg::ShellOut(p))), from);
                        thread::sleep(Duration::from_millis(2));
                    }
                });
            }
        }
        Msg::SetSharp(on) => {
            if let Some(s) = sh.sessions.lock().unwrap().iter_mut().find(|s| s.id == sid) {
                s.sharp = on;
            }
            sh.keyframe.store(true, SeqCst);
        }
        Msg::Clipboard { seq, text } => {
            let ack = sh.sessions.lock().unwrap().iter_mut().find(|s| s.id == sid).map(|s| s.clip.on_remote(seq, text));
            if let Some(ack) = ack {
                reply(&ack);
            }
        }
        Msg::ClipboardAck { seq } => {
            if let Some(s) = sh.sessions.lock().unwrap().iter_mut().find(|s| s.id == sid) {
                s.clip.on_ack(seq);
            }
        }
        Msg::Bye => sh.end_session(sid, "they disconnected"),
        m @ (Msg::FileOffer { .. } | Msg::FileChunk { .. } | Msg::FileAck { .. } | Msg::FileCancel { .. }) => {
            let replies = sh.sessions.lock().unwrap().iter_mut().find(|s| s.id == sid).map(|s| s.files.on_msg(m)).unwrap_or_default();
            for r in replies {
                reply(&r);
            }
        }
        Msg::Voice { pcm, .. } => {
            let name = sh.sessions.lock().unwrap().iter().find(|s| s.id == sid).map(|s| s.name.clone()).unwrap_or_default();
            if let Some(v) = &*sh.voice.lock().unwrap() {
                v.play(&name, &pcm);
            }
            let plain = encode(&Msg::Voice { from: name, pcm });
            for s in sh.sessions.lock().unwrap().iter().filter(|s| s.id != sid) {
                let _ = sh.socks[s.sock].send_to(&s.cipher.seal(&plain), s.addr);
            }
        }
        Msg::Chat { text, .. } => {
            let text: String = text.chars().take(500).collect();
            let name = sh.sessions.lock().unwrap().iter().find(|s| s.id == sid).map(|s| s.name.clone()).unwrap_or_default();
            sh.chat(&name, &text, Some(sid));
        }
        _ => {}
    }
}

/// Lower bitrate 20% when any guest reports new losses; creep back up when clean.
fn adapt_bitrate(sh: &Shared, sid: u64, lost_total: u32) {
    let new_loss = {
        let mut ss = sh.sessions.lock().unwrap();
        let Some(s) = ss.iter_mut().find(|s| s.id == sid) else { return };
        let prev = std::mem::replace(&mut s.last_lost, lost_total);
        lost_total > prev
    };
    let (_, max) = sh.quality();
    let cur = sh.bitrate.load(SeqCst);
    let next = if new_loss { (cur as f64 * 0.8) as u32 } else { (cur as f64 * 1.025) as u32 };
    sh.bitrate.store(next.clamp(1_000_000.min(max), max), SeqCst);
}

// ---------------------------------------------------------------- video

/// The desktop area covered by all these monitors.
fn union_rect(all: &[Capturer]) -> windows::Win32::Foundation::RECT {
    let mut u = all[0].monitor.rect;
    for c in &all[1..] {
        let r = c.monitor.rect;
        (u.left, u.top, u.right, u.bottom) = (u.left.min(r.left), u.top.min(r.top), u.right.max(r.right), u.bottom.max(r.bottom));
    }
    u
}

fn video_loop(sh: Arc<Shared>) {
    let mut cap: Option<Capturer> = None;
    let mut enc: Option<Box<dyn crate::codec::VideoEncoder>> = None;
    let mut enc_size = (0, 0);
    let (mut cur_monitor, mut cur_bitrate) = (u8::MAX, 0);
    let (mut frame, mut scaled) = (Vec::new(), Vec::new());
    let mut frame_id = 0u32;
    let mut dirty = false;
    let mut enc_fps = 0;
    // "All screens": one capturer per monitor, joined like the real desktop layout.
    let mut all: Vec<Capturer> = Vec::new();
    let mut bufs: Vec<Vec<u8>> = Vec::new();
    // Host-side recording (Settings): one file per stretch of guests being connected.
    let mut recorder: Option<crate::rec::Recorder> = None;
    let finish_rec = |r: crate::rec::Recorder, sh: &Shared| match r.finish() {
        Ok(p) => {
            crate::plog!("host: session recording saved to {}", p.display());
            sh.status.lock().unwrap().notes.push(format!("Session recording saved to {}", p.display()));
        }
        Err(e) => crate::plog!("host: recording failed: {e:#}"),
    };
    let mut next_slot = Instant::now();
    let mut sent_shape = u32::MAX;
    let (mut n_frames, mut n_bytes, mut enc_time, mut tick) = (0u32, 0usize, Duration::ZERO, Instant::now());
    // Lock screen / UAC support (if the helper service is installed).
    let (mut secure, mut last_check, mut last_connect) = (false, Instant::now() - Duration::from_secs(10), Instant::now() - Duration::from_secs(10));
    let (mut fw, mut fh) = (0usize, 0usize);

    while !sh.stop.load(SeqCst) {
        let targets: Vec<(SocketAddr, usize, Arc<Cipher>)> = sh.sessions.lock().unwrap().iter().map(|s| (s.addr, s.sock, s.cipher.clone())).collect();
        let web_on = !sh.web.lock().unwrap().is_empty();
        if targets.is_empty() && !web_on {
            if let Some(r) = recorder.take() {
                finish_rec(r, &sh);
            }
            all.clear();
            cap = None;
            enc = None;
            sent_shape = u32::MAX;
            *sh.helper.lock().unwrap() = None; // only talk to the helper during sessions
            secure = false;
            thread::sleep(Duration::from_millis(50));
            continue;
        }
        // Keep a helper link while guests are connected; ask it whether the secure desktop is up.
        if sh.helper.lock().unwrap().is_none() && last_connect.elapsed() > Duration::from_secs(3) {
            last_connect = Instant::now();
            if let Ok(l) = crate::syshelper::Link::connect() {
                *sh.helper.lock().unwrap() = Some(l);
            }
        }
        if last_check.elapsed() > Duration::from_millis(400) {
            last_check = Instant::now();
            let s = sh.helper.lock().unwrap().as_mut().map(|l| l.secure());
            // PCLONE_FORCE_HELPER=1 (tests): stream through the helper even on the normal desktop.
            let now = matches!(s, Some(Ok(true))) || (s.is_some() && std::env::var_os("PCLONE_FORCE_HELPER").is_some());
            if matches!(s, Some(Err(_))) {
                *sh.helper.lock().unwrap() = None;
            }
            if now != secure {
                secure = now;
                cap = None;
                sh.keyframe.store(true, SeqCst);
            }
        }
        if secure {
            // Lock screen / UAC prompt: only the SYSTEM helper can see it.
            let mon = sh.monitor.load(SeqCst);
            let got = sh.helper.lock().unwrap().as_mut().map(|l| l.frame(mon));
            match got {
                Some(Ok(Some((w, h, bgra)))) => {
                    frame = bgra;
                    (fw, fh) = (w, h);
                    dirty = true;
                }
                Some(Ok(None)) => {}
                Some(Err(_)) | None => thread::sleep(Duration::from_millis(50)),
            }
        } else if sh.monitor.load(SeqCst) == proto::ALL_SCREENS {
            cap = None;
            cur_monitor = u8::MAX;
            if all.is_empty() {
                all = monitors().into_iter().filter_map(|m| Capturer::new(m).ok()).collect();
                bufs = vec![Vec::new(); all.len()];
                if all.is_empty() {
                    thread::sleep(Duration::from_millis(300));
                    continue;
                }
                sh.injector.lock().unwrap().set_monitor(union_rect(&all));
                sh.keyframe.store(true, SeqCst);
            }
            let u = union_rect(&all);
            let (uw, uh) = (((u.right - u.left) as usize) & !1, ((u.bottom - u.top) as usize) & !1);
            let wait = next_slot.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
            let (mut changed, mut failed) = (false, false);
            for (i, c) in all.iter_mut().enumerate() {
                let ms = if i == 0 { wait.as_millis() as u32 } else { 0 };
                match c.next_frame(ms, &mut bufs[i]) {
                    Ok(ch) => {
                        changed |= ch.image;
                        let (visible, cx, cy, version) = c.cursor();
                        if visible && (ch.cursor || sh.keyframe.load(SeqCst)) {
                            let x = (c.monitor.rect.left - u.left + cx) as f32 / uw as f32;
                            let y = (c.monitor.rect.top - u.top + cy) as f32 / uh as f32;
                            sh.broadcast(&Msg::CursorPos { x, y, visible, version });
                        }
                    }
                    Err(_) => failed = true,
                }
            }
            if failed {
                all.clear();
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            if changed || frame.len() != uw * uh * 4 {
                frame.resize(uw * uh * 4, 0);
                for (i, c) in all.iter().enumerate() {
                    let b = &bufs[i];
                    if b.len() < c.width * c.height * 4 {
                        continue;
                    }
                    let (ox, oy) = ((c.monitor.rect.left - u.left) as usize, (c.monitor.rect.top - u.top) as usize);
                    let w = c.width.min(uw.saturating_sub(ox));
                    for row in 0..c.height.min(uh.saturating_sub(oy)) {
                        let d = ((oy + row) * uw + ox) * 4;
                        frame[d..d + w * 4].copy_from_slice(&b[row * c.width * 4..row * c.width * 4 + w * 4]);
                    }
                }
                dirty = true;
            }
            (fw, fh) = (uw, uh);
        } else {
        all.clear();
        let mon = sh.monitor.load(SeqCst);
        if mon != cur_monitor {
            cap = None;
            cur_monitor = mon;
        }
        if cap.is_none() {
            let mons = monitors();
            let Some(m) = mons.get(mon as usize).or(mons.first()).cloned() else {
                thread::sleep(Duration::from_millis(500));
                continue;
            };
            match Capturer::new(m) {
                Ok(c) => {
                    sh.injector.lock().unwrap().set_monitor(c.monitor.rect);
                    sh.status.lock().unwrap().error = None;
                    cap = Some(c);
                    frame.clear();
                    sh.keyframe.store(true, SeqCst);
                }
                Err(e) => {
                    // Typical while the lock screen / UAC prompt is up: retry.
                    sh.status.lock().unwrap().error = Some(format!("Screen capture: {e:#}"));
                    thread::sleep(Duration::from_millis(300));
                    continue;
                }
            }
        }
        let c = cap.as_mut().unwrap();
        let wait = next_slot.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        match c.next_frame(wait.as_millis() as u32, &mut frame) {
            Ok(ch) => {
                dirty |= ch.image;
                let (visible, cx, cy, version) = c.cursor();
                let resend = sh.keyframe.load(SeqCst); // a new guest needs the shape too
                if version != sent_shape || resend {
                    if let Some(img) = c.cursor_image() {
                        sh.broadcast(&Msg::CursorShape { version, w: img.w, h: img.h, hx: img.hx, hy: img.hy, rgba: img.rgba });
                        sent_shape = version;
                    }
                }
                if ch.cursor || resend {
                    let (x, y) = (cx as f32 / c.width as f32, cy as f32 / c.height as f32);
                    sh.broadcast(&Msg::CursorPos { x, y, visible, version });
                }
            }
            Err(_) => {
                cap = None;
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        }
        (fw, fh) = (c.width, c.height);
        }
        let force = sh.keyframe.load(SeqCst);
        if frame.is_empty() || (!dirty && !force) || Instant::now() < next_slot {
            continue;
        }
        sh.keyframe.store(false, SeqCst);
        dirty = false;
        let fps = sh.fps();
        next_slot = Instant::now() + Duration::from_secs_f64(1.0 / fps as f64);

        // Resolution option (lowest any guest asked for) + even dimensions for the encoder.
        let (max_h, _) = sh.quality();
        if frame.len() < fw * fh * 4 {
            continue;
        }
        let (w, h) = scale::fit(fw, fh, max_h);
        let src: &[u8] = if (w, h) == (fw, fh) {
            &frame
        } else {
            scale::bilinear(&frame, fw, fh, &mut scaled, w, h);
            &scaled
        };
        if enc.is_none() || enc_size != (w, h) || enc_fps != fps {
            cur_bitrate = sh.bitrate.load(SeqCst);
            match new_encoder(w, h, fps, cur_bitrate, sh.cfg.hardware) {
                Ok(e) => {
                    let what = format!("{} @ {}x{} {} fps", e.name(), w, h, fps);
                    crate::plog!("host: encoder {what}");
                    sh.status.lock().unwrap().encoder = what;
                    enc = Some(e);
                    enc_size = (w, h);
                    enc_fps = fps;
                }
                Err(e) => {
                    sh.status.lock().unwrap().error = Some(format!("Encoder: {e:#}"));
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }
            }
        }
        let e = enc.as_mut().unwrap();
        let br = sh.bitrate.load(SeqCst);
        if br != cur_bitrate {
            e.set_bitrate(br);
            cur_bitrate = br;
        }
        let t = now_us();
        let t0 = Instant::now();
        let (data, key) = match e.encode(src, force) {
            Ok(r) => r,
            Err(err) => {
                sh.status.lock().unwrap().error = Some(format!("Encode: {err:#}"));
                enc = None;
                continue;
            }
        };
        enc_time += t0.elapsed();
        if data.is_empty() {
            if force {
                sh.keyframe.store(true, SeqCst);
            }
            continue;
        }
        if sh.cfg.record {
            if recorder.as_ref().is_some_and(|r| r.size != (w, h)) {
                finish_rec(recorder.take().unwrap(), &sh);
            }
            match recorder.as_mut() {
                Some(r) => {
                    let _ = r.video(&data, key);
                }
                None if key => match crate::rec::Recorder::start(&data, w, h, 0) {
                    Ok(r) => recorder = Some(r),
                    Err(e) => crate::plog!("host: can't record: {e:#}"),
                },
                None => sh.keyframe.store(true, SeqCst), // a recording has to start on a keyframe
            }
        }
        if web_on {
            let f = Arc::new(crate::web::Frame { data: data.as_slice().into(), key });
            sh.web.lock().unwrap().retain(|g| !matches!(g.tx.try_send(f.clone()), Err(std::sync::mpsc::TrySendError::Disconnected(_))));
        }
        // ponytail: whole frame sent as one burst per guest; add pacing if big keyframes overflow slow links
        let count = data.len().div_ceil(CHUNK) as u16;
        for (i, chunk) in data.chunks(CHUNK).enumerate() {
            let plain = encode(&Msg::Video { frame: frame_id, idx: i as u16, count, key, t, data: chunk.to_vec() });
            for (addr, sock, cipher) in &targets {
                let _ = sh.socks[*sock].send_to(&cipher.seal(&plain), addr);
            }
        }
        frame_id = frame_id.wrapping_add(1);
        n_frames += 1;
        n_bytes += data.len();
        if tick.elapsed() >= Duration::from_secs(1) {
            let secs = tick.elapsed().as_secs_f64();
            sh.status.lock().unwrap().stats = format!(
                "{:.0} fps · {:.1} Mbps per guest (target {:.1}) · encode {:.1} ms",
                n_frames as f64 / secs,
                n_bytes as f64 * 8.0 / secs / 1e6,
                cur_bitrate as f64 / 1e6,
                enc_time.as_secs_f64() * 1000.0 / n_frames.max(1) as f64
            );
            (n_frames, n_bytes, enc_time, tick) = (0, 0, Duration::ZERO, Instant::now());
        }
    }
}

// ---------------------------------------------------------------- audio / clipboard / upnp

fn audio_loop(sh: Arc<Shared>) {
    let s2 = sh.clone();
    let mut seq = 0u32;
    let send = move |pcm| {
        seq = seq.wrapping_add(1);
        s2.broadcast(&Msg::Audio { seq, pcm });
    };
    // Preferred: everything except this app, so guests' voices we play aren't sent back.
    let send = match crate::audio::start_loopback_excluding_self(send.clone()) {
        Ok((handle, rate)) => {
            sh.audio_rate.store(rate, SeqCst);
            while !sh.stop.load(SeqCst) {
                thread::sleep(Duration::from_millis(100));
            }
            drop(handle);
            return;
        }
        Err(_) => send, // older Windows: plain loopback (voice chat may echo)
    };
    let res = crate::audio::start_loopback(send);
    match res {
        Ok((_stream, rate)) => {
            sh.audio_rate.store(rate, SeqCst);
            while !sh.stop.load(SeqCst) {
                thread::sleep(Duration::from_millis(100));
            }
        }
        Err(e) => sh.status.lock().unwrap().notes.push(format!("No audio: {e:#}")),
    }
}

fn clip_loop(sh: Arc<Shared>) {
    while !sh.stop.load(SeqCst) {
        thread::sleep(Duration::from_millis(400));
        let mut outgoing: Vec<(usize, SocketAddr, Arc<Cipher>, Msg)> =
            sh.sessions.lock().unwrap().iter_mut().filter_map(|s| s.clip.poll().map(|m| (s.sock, s.addr, s.cipher.clone(), m))).collect();
        // Copied pictures/files go to every guest through the file transfer.
        for s in sh.sessions.lock().unwrap().iter_mut() {
            if let Some(rich) = s.clip.poll_rich() {
                for (path, name) in crate::clip::marked_names(&rich) {
                    if let Ok(offer) = s.files.send_as(&path, &name) {
                        outgoing.push((s.sock, s.addr, s.cipher.clone(), offer));
                    }
                }
            }
        }
        for (sock, addr, cipher, m) in outgoing {
            let _ = sh.socks[sock].send_to(&cipher.seal(&encode(&m)), addr);
        }
    }
}

/// Sends file chunks/acks for every guest and publishes progress for the UI.
fn file_loop(sh: Arc<Shared>) {
    while !sh.stop.load(SeqCst) {
        thread::sleep(Duration::from_millis(5));
        let mut outgoing = Vec::new();
        let (mut active, mut done) = (Vec::new(), Vec::new());
        for s in sh.sessions.lock().unwrap().iter_mut() {
            for m in s.files.pump() {
                outgoing.push((s.sock, s.addr, s.cipher.clone(), m));
            }
            active.extend(s.files.active().into_iter().map(|p| (s.name.clone(), p)));
            done.extend(s.files.take_finished().into_iter().map(|p| (s.name.clone(), p)));
        }
        for (sock, addr, cipher, m) in outgoing {
            let _ = sh.socks[sock].send_to(&cipher.seal(&encode(&m)), addr);
        }
        let mut st = sh.status.lock().unwrap();
        st.transfers = active;
        st.finished.extend(done);
    }
}

/// Optional: a UPnP port map gives friends one more way in (helps with strict NATs).
fn upnp(sh: Arc<Shared>) {
    let (true, Some(IpAddr::V4(lan))) = (sh.cfg.upnp, local_ip(false)) else { return };
    let mut opts = igd_next::SearchOptions::default();
    opts.timeout = Some(Duration::from_secs(4));
    let Ok(gw) = igd_next::search_gateway(opts) else { return };
    let port = sh.port;
    let map = || gw.add_port(igd_next::PortMappingProtocol::UDP, port, SocketAddr::new(lan.into(), port), 3600, "Kinlink");
    if let (Ok(ext), Ok(())) = (gw.get_external_ip(), map()) {
        if !proto::is_private(ext) {
            *sh.upnp_ext.lock().unwrap() = Some(SocketAddr::new(ext, port));
        }
    }
    let mut renew = Instant::now();
    while !sh.stop.load(SeqCst) {
        thread::sleep(Duration::from_millis(200));
        if renew.elapsed() > Duration::from_secs(1800) {
            renew = Instant::now();
            let _ = map();
        }
    }
    let _ = gw.remove_port(igd_next::PortMappingProtocol::UDP, port);
}
