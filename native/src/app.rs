//! Window, friends UI, connection flow, streaming view with floating control box, tray.
use crate::client::{self, Client, State};
use crate::crypto::{Ephemeral, Identity, Pub};
use crate::host::{Host, HostConfig, Perms};
use crate::i18n::{tl, tlf, tr, trf};
use crate::theme::para;
use crate::kbhook::{self, KeyEv};
use crate::proto::Input;
use crate::render::Gfx;
use crate::settings::{self, Settings};
use crate::signal::{Event, Friend, Sig, Signal};
use egui::{Color32, RichText};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowId};

// Scan codes for hotkeys (Ctrl+Alt+Shift + key).
const SC_CTRL: u16 = 0x1D;
const SC_ALT: u16 = 0x38;
const SC_LSHIFT: u16 = 0x2A;
const SC_RSHIFT: u16 = 0x36;
const SC_Q: u16 = 0x10;
const SC_O: u16 = 0x18;
const SC_M: u16 = 0x32;
const SC_R: u16 = 0x13;
const SC_F: u16 = 0x21;
const SC_C: u16 = 0x2E;
const SC_P: u16 = 0x19;
const SC_V: u16 = 0x2F;

const GREEN: Color32 = crate::theme::MINT;
const RED: Color32 = Color32::from_rgb(0xFF, 0x7A, 0x96);
const RESOLUTIONS: &[(&str, u32)] = &[("Native", 0), ("1440p", 1440), ("1080p", 1080), ("900p", 900), ("720p", 720), ("540p", 540)];
const QUALITIES: &[(&str, u32)] = &[("Low (5 Mbps)", 5), ("Medium (10 Mbps)", 10), ("High (20 Mbps)", 20), ("Very high (35 Mbps)", 35), ("Max (60 Mbps)", 60)];
/// One-tap presets: (name, RESOLUTIONS index, QUALITIES index, sharp text).
const PRESETS: &[(&str, usize, usize, bool)] = &[("Low data", 4, 0, false), ("Balanced", 2, 2, false), ("Best", 0, 3, false), ("Sharp text", 0, 3, true)];

/// Choices for the in-session shortcut modifiers (bit 0 Ctrl, 1 Alt, 2 Shift).
const HOTKEY_MODS: &[(u8, &str)] = &[(7, "Ctrl+Alt+Shift"), (5, "Ctrl+Shift"), (3, "Ctrl+Alt"), (6, "Alt+Shift")];

/// A hint text with the user's shortcut modifiers instead of the default Ctrl+Alt+Shift.
fn hk(s: &Settings, text: &str) -> String {
    let m = HOTKEY_MODS.iter().find(|m| m.0 == s.hotkey_mods).map(|m| m.1).unwrap_or("Ctrl+Alt+Shift");
    text.replace("Ctrl+Alt+Shift", m)
}

/// Controller buttons as (XInput bit index, label), in remap-editor order.
const PAD_BUTTONS: &[(u8, &str)] = &[(12, "A"), (13, "B"), (14, "X"), (15, "Y"), (8, "LB"), (9, "RB"), (6, "L3"), (7, "R3"), (4, "Start"), (5, "Back"), (0, "Up"), (1, "Down"), (2, "Left"), (3, "Right"), (10, "Guide")];

/// Applies a saved controller remap (empty/invalid = unchanged).
fn set_pad_map(m: &[u8]) {
    *crate::client::PAD_MAP.lock().unwrap() = m.try_into().unwrap_or(crate::client::IDENTITY_MAP);
}

/// What each preset is for (tooltip).
fn preset_hint(name: &str) -> &'static str {
    match name {
        "Low data" => "720p, 5 Mbps: slow or mobile internet",
        "Balanced" => "1080p, 20 Mbps",
        "Best" => "Full resolution, 35 Mbps",
        _ => "Full resolution at 30 fps: crisp text for reading and office work",
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Friends,
    Pc,
    Files,
    Settings,
}

struct IncomingReq {
    from: Pub,
    name: String,
    req: u64,
    eph: Pub,
    addrs: Vec<SocketAddr>,
    at: Instant,
    /// Joined with an invite code: the permissions chosen when the invite was made.
    guest: Option<Perms>,
}

struct Outgoing {
    to: Pub,
    name: String,
    req: u64,
    prep: Arc<Mutex<Option<(UdpSocket, Ephemeral)>>>,
    at: Instant,
}

/// Auto-update state shared with the checker/installer threads.
#[derive(Clone)]
enum Update {
    None,
    Available(crate::updater::Release),
    Installing(f32),
    Failed(String),
    Done,
}

enum TrayMsg {
    Open,
    Panic,
    Quit,
}

static SHOW_REQUEST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Waits for a second launch of the exe (see main::single_instance) and asks the UI to show itself.
fn listen_for_show(wake: impl Fn() + Send + 'static) {
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
    let Ok(h) = (unsafe { CreateEventW(None, false, false, &settings::show_event_name()) }) else { return };
    let raw = h.0 as isize;
    std::thread::spawn(move || loop {
        let h = windows::Win32::Foundation::HANDLE(raw as *mut _);
        unsafe { WaitForSingleObject(h, INFINITE) };
        SHOW_REQUEST.store(true, std::sync::atomic::Ordering::SeqCst);
        wake();
    });
}

pub struct App {
    s: Settings,
    proxy: Arc<Mutex<EventLoopProxy<()>>>,
    gfx: Option<Gfx>,
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    kb: Option<Receiver<KeyEv>>,
    signal: Option<Signal>,
    setup_name: String,
    setup_err: Option<String>,
    host: Option<Host>,
    host_err: Option<String>,
    popups: Vec<IncomingReq>,
    /// Friends we asked for help (they get full control without a popup for 15 min).
    help_asked: Vec<(Pub, Instant)>,
    /// Friends asking us for help.
    help_from: Vec<Pub>,
    outgoing: Option<Outgoing>,
    client: Option<Client>,
    peer: String,
    message: Option<(String, Instant)>,
    add_text: String,
    add_err: Option<String>,
    rename_text: String,
    page: Page,
    /// Custom status being typed (None = not editing).
    status_edit: Option<String>,
    /// Friend whose chat window is open, and what's being typed.
    chat_with: Option<Pub>,
    chat_draft: String,
    /// Copied pictures/files arriving from the other PC: batch -> (expected, is_image, paths).
    clip_batches: std::collections::HashMap<String, (usize, bool, Vec<std::path::PathBuf>)>,
    /// Joining by invite: (host id, code, since).
    pending_join: Option<(String, String, Instant)>,
    join_text: String,
    /// Permissions for the next invite, and the last link made.
    invite_perms: Perms,
    last_invite: Option<String>,
    /// Session history: finished sessions (newest last) and the ones running now.
    history: Vec<HistEntry>,
    live_sessions: std::collections::HashMap<String, (String, Instant)>,
    rename_err: Option<String>,
    expanded: Option<Pub>,
    streaming: bool,
    stream_started: Instant,
    overlay_open: bool,
    /// When the session menu was last opened (clicks right after opening are ignored for Disconnect).
    overlay_opened: Instant,
    overlay_shown: bool,
    over_overlay: bool,
    cursor_visible: bool,
    relative: bool,
    mods: [bool; 3],
    res_idx: usize,
    qual_idx: usize,
    /// Sharp-text mode: 30 fps with more detail per frame (for reading/office work).
    sharp: bool,
    volume: u32,
    muted: bool,
    autostart: bool,
    start_minimized: bool,
    tray: Option<tray_icon::TrayIcon>,
    tray_rx: Option<Receiver<TrayMsg>>,
    last_refresh: Instant,
    /// When egui asked to be redrawn next (animations, a second layout pass, timers).
    repaint_at: Option<Instant>,
    update: Arc<Mutex<Update>>,
    connect_on_start: Option<(String, Instant)>,
    /// Local mouse position over the window (physical px), for the snappy cursor and menu reveal.
    pointer: Option<(f32, f32)>,
    show_hint: bool,
    signal_started: Instant,
    chat_open: bool,
    chat_text: String,
    host_chat_text: String,
    /// Short-lived bubble in a session (new chat message, file received...).
    bubble: Option<(String, Instant)>,
    nettest: Option<Arc<Mutex<Option<crate::nettest::Report>>>>,
    cursor_tex: Option<(u32, egui::TextureHandle)>,
    /// While we drive the mouse, our real Windows cursor is shown, wearing the host cursor's shape
    /// (one cursor, no lag). These track that shape.
    os_cursor: bool,
    os_shape: Option<Arc<client::CursorShape>>,
    os_applied: Option<(u32, winit::window::CustomCursor)>,
    os_dirty: bool,
    /// File chosen with the "Send file..." button (picked on another thread).
    picked: Arc<Mutex<Option<std::path::PathBuf>>>,
}

impl App {
    pub fn new(proxy: EventLoopProxy<()>, start_minimized: bool, connect_on_start: Option<String>) -> Self {
        Self {
            s: Settings::load(),
            proxy: Arc::new(Mutex::new(proxy)),
            gfx: None,
            egui_ctx: egui::Context::default(),
            egui_state: None,
            kb: None,
            signal: None,
            setup_name: String::new(),
            setup_err: None,
            host: None,
            host_err: None,
            popups: Vec::new(),
            help_asked: Vec::new(),
            help_from: Vec::new(),
            outgoing: None,
            client: None,
            peer: String::new(),
            message: None,
            add_text: String::new(),
            add_err: None,
            rename_text: String::new(),
            page: Page::Friends,
            status_edit: None,
            chat_with: None,
            chat_draft: String::new(),
            pending_join: None,
            clip_batches: Default::default(),
            join_text: String::new(),
            invite_perms: Perms { mouse: false, keyboard: false, gamepad: true },
            last_invite: None,
            history: load_history(),
            live_sessions: Default::default(),
            rename_err: None,
            expanded: None,
            streaming: false,
            stream_started: Instant::now(),
            overlay_open: false,
            overlay_opened: Instant::now(),
            overlay_shown: true,
            over_overlay: false,
            cursor_visible: true,
            relative: false,
            mods: [false; 3],
            res_idx: 0,
            qual_idx: 3,
            sharp: false,
            volume: 100,
            muted: false,
            autostart: settings::autostart_enabled(),
            start_minimized,
            tray: None,
            tray_rx: None,
            last_refresh: Instant::now(),
            repaint_at: None,
            update: Arc::new(Mutex::new(Update::None)),
            connect_on_start: connect_on_start.map(|id| (id, Instant::now())),
            pointer: None,
            show_hint: false,
            signal_started: Instant::now(),
            chat_open: false,
            chat_text: String::new(),
            host_chat_text: String::new(),
            bubble: None,
            nettest: None,
            cursor_tex: None,
            os_cursor: false,
            os_shape: None,
            os_applied: None,
            os_dirty: true,
            picked: Arc::new(Mutex::new(None)),
        }
    }

    fn waker(&self) -> impl Fn() + Send + Sync + 'static {
        let p = self.proxy.clone();
        move || {
            let _ = p.lock().unwrap().send_event(());
        }
    }

    fn window(&self) -> Option<&Arc<Window>> {
        self.gfx.as_ref().map(|g| &g.window)
    }

    fn show_window(&self) {
        if let Some(w) = self.window() {
            w.set_visible(true);
            w.set_minimized(false);
            w.focus_window();
            w.request_redraw();
        }
    }

    fn toast(&mut self, m: impl Into<String>) {
        self.message = Some((m.into(), Instant::now()));
    }

    fn start_services(&mut self, id: Identity) {
        let store = settings::data_dir().map(|d| d.join("friends.json"));
        let sig = Signal::start(id, store, self.waker());
        sig.set_status(&self.s.status);
        self.signal = Some(sig);
        crate::files::ASK.store(self.s.ask_files, std::sync::atomic::Ordering::Relaxed);
        set_pad_map(&self.s.pad_map);
        self.signal_started = Instant::now();
        if !self.s.autostart_init && settings::profile().is_none() {
            // New install: start with Windows by default (can be turned off in settings).
            self.s.autostart_init = true;
            self.s.save();
            settings::set_autostart(true);
            self.autostart = true;
        }
        if self.s.hosting {
            self.start_host();
        }
        // Check GitHub Releases now and every 6 hours.
        let (upd, wake) = (self.update.clone(), self.waker());
        std::thread::spawn(move || loop {
            if let Ok(Some(r)) = crate::updater::check() {
                let mut u = upd.lock().unwrap();
                if matches!(*u, Update::None | Update::Failed(_)) {
                    *u = Update::Available(r);
                    drop(u);
                    wake();
                }
            }
            std::thread::sleep(Duration::from_secs(6 * 3600));
        });
    }

    fn start_host(&mut self) {
        let name = self.signal.as_ref().map(|s| s.id()).unwrap_or_else(|| "Host".into());
        let cfg = HostConfig { name, port: self.s.port, max_mbps: self.s.max_mbps, fps: self.s.fps, hardware: self.s.hardware, upnp: self.s.upnp, record: self.s.record_sessions };
        match Host::start(cfg) {
            Ok(h) => {
                self.host = Some(h);
                self.host_err = None;
            }
            Err(e) => self.host_err = Some(format!("{e:#}")),
        }
    }

    fn restart_host_if_idle(&mut self) {
        self.s.save();
        let busy = self.host.as_ref().is_some_and(|h| !h.status().clients.is_empty());
        if self.host.is_some() && !busy {
            self.host = None;
            self.start_host();
        }
    }

    // ------------------------------------------------------------ connection flow

    fn connect_to(&mut self, f: &Friend) {
        crate::plog!("connect: asking {}", f.id());
        self.connect_with(f.public, f.id(), None);
    }

    /// Connects to a host: a friend (invite = None) or anyone with their invite code.
    fn connect_with(&mut self, to: Pub, host_id: String, invite: Option<String>) {
        let Some(sig) = self.signal.clone() else { return };
        if self.client.is_some() || self.outgoing.is_some() {
            return;
        }
        let req: u64 = rand::random();
        let prep = Arc::new(Mutex::new(None));
        let (p2, wake, id2, me) = (prep.clone(), self.waker(), host_id.clone(), sig.id());
        std::thread::spawn(move || match client::prepare_socket() {
            Ok((sock, addrs)) => {
                let eph = Ephemeral::new();
                let public = eph.public;
                *p2.lock().unwrap() = Some((sock, eph));
                let msg = match invite {
                    Some(code) => Sig::GuestConnect { code, name: me, req, eph: public, addrs },
                    None => Sig::Connect { req, eph: public, addrs },
                };
                sig.send_to(&id2, &to, msg);
            }
            Err(e) => {
                crate::plog!("connect: couldn't open a UDP socket: {e}");
                wake();
            }
        });
        self.outgoing = Some(Outgoing { to, name: host_id, req, prep, at: Instant::now() });
        self.message = None;
    }

    /// Joins a host with an invite link or code.
    fn start_join(&mut self, text: &str) {
        let Some(sig) = self.signal.clone() else { return };
        match crate::signal::parse_invite(text) {
            Some((host_id, code)) => {
                crate::plog!("join: looking up {host_id}");
                sig.join(&host_id);
                self.toast(tlf("Finding {}…", &[&host_id]));
                self.pending_join = Some((host_id, code, Instant::now()));
            }
            None => self.toast(tl("That isn't a valid invite link")),
        }
    }

    /// New username, same key: tell friends, then reconnect to the brokers under the new id.
    fn rename(&mut self) {
        let name = self.rename_text.split('#').next().unwrap_or("").trim().to_string();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        let res = (|| {
            if !crate::crypto::valid_new_name(&name) {
                return Err("2–20 letters or numbers, no symbols".to_string());
            }
            let cur = Identity::load().ok_or("Couldn't read your identity")?;
            if cur.name == name {
                return Err("That's already your name".into());
            }
            let wait = cur.rename_wait(now);
            if wait > 0 {
                return Err(tlf("You can change it again in {} days", &[&wait.div_ceil(86_400).to_string()]));
            }
            let new = cur.renamed(&name, now);
            if !new.save() {
                return Err("Couldn't save your new name".into());
            }
            Ok((cur.id(), new))
        })();
        match res {
            Err(e) => self.rename_err = Some(e),
            Ok((was, new)) => {
                let id = new.id();
                crate::plog!("rename: {was} -> {id}");
                if let Some(s) = self.signal.take() {
                    s.tell_renamed(&name);
                    s.shutdown();
                }
                let store = settings::data_dir().map(|d| d.join("friends.json"));
                let sig = Signal::start(new, store, self.waker());
                sig.set_status(&self.s.status);
                self.signal = Some(sig);
                self.rename_err = None;
                self.rename_text.clear();
                self.toast(tlf("You're now {}. Friends keep you; give new people this ID.", &[&id]));
            }
        }
    }

    fn cancel_outgoing(&mut self) {
        crate::plog!("connect: request cancelled");
        if let (Some(o), Some(s)) = (self.outgoing.take(), &self.signal) {
            s.send_to(&o.name, &o.to, Sig::Cancel { req: o.req });
        }
    }

    fn accept_incoming(&mut self, r: IncomingReq) {
        let (Some(sig), Some(host)) = (self.signal.clone(), self.host.as_ref().map(|h| h.handle())) else { return };
        let perms = match (r.guest, sig.friend(&r.from)) {
            (Some(p), _) => p,
            (None, Some(f)) => Perms { mouse: f.allow_mouse, keyboard: f.allow_keyboard, gamepad: f.allow_gamepad },
            (None, None) => return,
        };
        std::thread::spawn(move || {
            let session: u64 = rand::random();
            let eph = Ephemeral::new();
            let cipher = eph.cipher(&r.eph, session, false);
            let addrs = host.prepare(cipher, perms, r.name.clone());
            sig.send_to(&r.name, &r.from, Sig::ConnectOk { req: r.req, session, eph: eph.public, addrs });
            host.punch(r.addrs, session);
        });
    }

    fn decline_incoming(&self, r: &IncomingReq, reason: &str) {
        if let Some(s) = &self.signal {
            s.send_to(&r.name, &r.from, Sig::ConnectNo { req: r.req, reason: reason.into() });
        }
    }

    fn on_signal_event(&mut self, e: Event) {
        let Some(sig) = self.signal.clone() else { return };
        match e {
            Event::IncomingConnect { from, req, eph, addrs } => {
                let Some(f) = sig.friend(&from) else { return };
                crate::plog!("host: {} wants to connect (hosting: {}, auto-accept: {})", f.id(), self.host.is_some(), f.auto_connect);
                let mut r = IncomingReq { from, name: f.id(), req, eph, addrs, at: Instant::now(), guest: None };
                let helping = self.help_asked.iter().any(|(p, t)| *p == from && t.elapsed() < Duration::from_secs(900));
                if self.host.is_none() {
                    self.decline_incoming(&r, "isn't accepting connections right now");
                } else if helping {
                    // We asked them for help: let them in with full control.
                    self.help_asked.retain(|(p, _)| *p != from);
                    r.guest = Some(Perms { mouse: true, keyboard: true, gamepad: true });
                    self.toast(tlf("{} is connecting to help you", &[&f.id()]));
                    self.accept_incoming(r);
                } else if f.auto_connect {
                    self.toast(tlf("{} connected to this PC", &[&f.id()]));
                    self.accept_incoming(r);
                } else if !self.popups.iter().any(|p| p.from == from && p.req == req) {
                    self.popups.push(r);
                    self.show_window();
                }
            }
            Event::ConnectReply { from, sig: reply } => {
                let Some(o) = self.outgoing.as_ref().filter(|o| o.to == from) else {
                    crate::plog!("connect: ignored a reply (no request waiting for it)");
                    return;
                };
                match reply {
                    Sig::ConnectOk { req, session, eph, addrs } if req == o.req => {
                        let o = self.outgoing.take().unwrap();
                        let Some((sock, mine)) = o.prep.lock().unwrap().take() else {
                            crate::plog!("connect: host answered before our socket was ready; try again");
                            return;
                        };
                        let cipher = mine.cipher(&eph, session, true);
                        self.peer = o.name.clone();
                        self.client = Some(Client::start(sock, addrs, cipher, o.name, self.waker()));
                    }
                    Sig::ConnectNo { req, reason } if req == o.req => {
                        let name = self.outgoing.take().unwrap().name;
                        self.toast(format!("{name} {reason}"));
                    }
                    _ => {}
                }
            }
            Event::Cancelled { from, req } => self.popups.retain(|p| !(p.from == from && p.req == req)),
            Event::Toast(m) => self.toast(m),
            Event::FriendOnline(p) => {
                // Skip the burst of "online" replies right after we start.
                if self.s.notify_online && self.signal_started.elapsed() > Duration::from_secs(20) {
                    if let Some(f) = sig.friend(&p) {
                        crate::notify::toast(&tlf("{} is online", &[&f.display()]), &tl("Open Kinlink to connect."));
                    }
                }
            }
            Event::FriendRequest(id) => crate::notify::toast(&tlf("{} wants to be friends", &[&id]), &tl("Open Kinlink to accept.")),
            Event::GuestConnect { from, name, req, eph, addrs, mouse, keyboard, gamepad } => {
                let r = IncomingReq { from, name: name.clone(), req, eph, addrs, at: Instant::now(), guest: Some(Perms { mouse, keyboard, gamepad }) };
                if self.host.is_none() {
                    self.decline_incoming(&r, "isn't accepting connections right now");
                } else {
                    crate::plog!("host: {name} joined with an invite");
                    self.toast(tlf("{} joined with your invite", &[&name]));
                    crate::notify::toast(&tlf("{} joined with your invite", &[&name]), &tl("Kick them from This PC if you didn't expect this."));
                    self.accept_incoming(r);
                }
            }
            Event::HelpRequest(from) => {
                if let Some(f) = self.signal.as_ref().and_then(|s| s.friend(&from)) {
                    if !self.help_from.contains(&from) {
                        self.help_from.push(from);
                    }
                    crate::notify::toast(&tlf("{} needs help with their PC", &[&f.display()]), &tl("Open Kinlink and press Help to connect."));
                    self.show_window();
                }
            }
            Event::JoinReady { host, host_id } => {
                if let Some((id, code, _)) = self.pending_join.clone() {
                    if id.eq_ignore_ascii_case(&host_id) {
                        self.pending_join = None;
                        crate::plog!("join: found {host_id}, asking to connect");
                        self.connect_with(host, host_id, Some(code));
                    }
                }
            }
            Event::Message { from, text } => {
                let open_here = self.chat_with == Some(from) && self.window().is_some_and(|w| w.has_focus());
                if open_here {
                    sig.mark_read(&from);
                } else if let Some(f) = sig.friend(&from) {
                    crate::notify::toast(&f.display(), &text);
                }
            }
        }
    }

    // ------------------------------------------------------------ streaming

    fn enter_stream(&mut self) {
        self.streaming = true;
        self.stream_started = Instant::now();
        self.overlay_open = false;
        self.overlay_shown = true;
        self.message = None;
        kbhook::set_active(true);
        self.show_hint = !self.s.hint_seen;
        if !self.s.hint_seen {
            self.s.hint_seen = true;
            self.s.save();
        }
        if self.s.fullscreen_connect {
            if let Some(w) = self.window() {
                w.set_fullscreen(Some(Fullscreen::Borderless(None)));
            }
        }
        if let Some(c) = &self.client {
            c.set_volume(if self.muted { 0 } else { self.volume });
        }
        if let Some(g) = &mut self.gfx {
            g.clear_video();
        }
    }

    fn leave_stream(&mut self, msg: Option<String>) {
        self.chat_open = false;
        self.bubble = None;
        if let Some(c) = self.client.take() {
            c.disconnect();
        }
        self.streaming = false;
        self.relative = false;
        self.mods = [false; 3];
        kbhook::set_active(false);
        self.set_cursor_visible(true);
        self.os_cursor = false;
        self.os_shape = None;
        self.os_applied = None;
        if let Some(w) = self.window() {
            w.set_cursor(winit::window::CursorIcon::Default);
            let _ = w.set_cursor_grab(CursorGrabMode::None);
            w.set_fullscreen(None);
            w.request_redraw();
        }
        if let Some(m) = msg {
            self.toast(m);
        }
    }

    fn set_cursor_visible(&mut self, v: bool) {
        if self.cursor_visible != v {
            self.cursor_visible = v;
            if let Some(w) = self.window() {
                w.set_cursor_visible(v);
            }
        }
    }

    fn screenshot(&mut self) {
        if let Some(c) = &self.client {
            let msg = match c.screenshot() {
                Ok(p) => tlf("Screenshot saved: {}", &[&p.display().to_string()]),
                Err(e) => tlf("Screenshot failed: {}", &[&e.to_string()]),
            };
            self.bubble = Some((msg, Instant::now()));
        }
    }

    /// Opening chat hands the keyboard to the chat box instead of the remote PC.
    fn set_chat(&mut self, open: bool) {
        self.chat_open = open;
        kbhook::set_active(self.streaming && !open);
        if open {
            if let Some(c) = &self.client {
                c.input(Input::ReleaseAll);
                c.take_unread();
            }
            self.bubble = None;
        }
        if let Some(w) = self.window() {
            w.request_redraw();
        }
    }

    fn set_relative(&mut self, on: bool) {
        self.relative = on;
        if let Some(w) = self.window() {
            let mode = if on { CursorGrabMode::Locked } else { CursorGrabMode::None };
            if w.set_cursor_grab(mode).is_err() && on {
                let _ = w.set_cursor_grab(CursorGrabMode::Confined);
            }
        }
    }

    fn apply_quality(&self) {
        if let Some(c) = &self.client {
            c.set_quality(RESOLUTIONS[self.res_idx].1, QUALITIES[self.qual_idx].1);
            c.set_sharp(self.sharp);
        }
    }

    fn on_key(&mut self, k: KeyEv) {
        if std::env::var_os("PCLONE_DEBUG").is_some() {
            crate::plog!("ON_KEY {k:?} client={}", self.client.is_some());
        }
        let Some(c) = &self.client else { return };
        match k.scan {
            SC_CTRL => self.mods[0] = k.down,
            SC_ALT => self.mods[1] = k.down,
            SC_LSHIFT | SC_RSHIFT => self.mods[2] = k.down,
            _ => {}
        }
        let want = [self.s.hotkey_mods & 1 != 0, self.s.hotkey_mods & 2 != 0, self.s.hotkey_mods & 4 != 0];
        if k.down && self.mods == want && matches!(k.scan, SC_Q | SC_O | SC_M | SC_R | SC_F | SC_C | SC_P | SC_V) {
            c.input(Input::ReleaseAll); // drop the held modifiers on the host
            match k.scan {
                SC_Q => self.leave_stream(None),
                SC_O => self.overlay_shown = !self.overlay_shown,
                SC_M => {
                    if let State::Connected { monitors, current, .. } = c.state() {
                        c.select_monitor(((current as usize + 1) % monitors.len().max(1)) as u8);
                    }
                }
                SC_R => self.set_relative(!self.relative),
                SC_F => self.toggle_fullscreen(),
                SC_C => self.set_chat(!self.chat_open),
                SC_P => self.screenshot(),
                SC_V => {
                    let on = !c.mic_on();
                    c.set_mic(on);
                    self.bubble = Some((if on { hk(&self.s, "Microphone on (Ctrl+Alt+Shift+V to mute)") } else { "Microphone off".into() }, Instant::now()));
                }
                _ => {}
            }
            if let Some(w) = self.window() {
                w.request_redraw();
            }
            return;
        }
        c.input(Input::Key { scan: k.scan, ext: k.ext, down: k.down });
    }

    fn toggle_fullscreen(&self) {
        if let Some(w) = self.window() {
            w.set_fullscreen(if w.fullscreen().is_some() { None } else { Some(Fullscreen::Borderless(None)) });
        }
    }

    fn stream_input(&mut self, ev: &WindowEvent) {
        // Fallback keyboard path: when the low-level hook works it swallows keys and the window
        // never sees them; when something blocks the hook (e.g. anti-cheat drivers), keys arrive here.
        if self.chat_open && matches!(ev, WindowEvent::KeyboardInput { .. }) {
            return; // typing into chat
        }
        if let WindowEvent::KeyboardInput { event, is_synthetic: false, .. } = ev {
            use winit::platform::scancode::PhysicalKeyExtScancode;
            if let Some(sc) = event.physical_key.to_scancode() {
                if std::env::var_os("PCLONE_DEBUG").is_some() {
                    crate::plog!("WINDOW_KEY sc={sc:#x}");
                }
                if kbhook::is_active() {
                    kbhook::missed_key(); // the hook should have taken it (Alt+Tab would go local)
                }
                self.on_key(KeyEv { scan: (sc & 0xFF) as u16, ext: sc & 0xE000 != 0, down: event.state == ElementState::Pressed });
            }
            return;
        }
        let (Some(c), Some(g)) = (&self.client, &self.gfx) else { return };
        if self.over_overlay && !self.relative {
            return; // the floating box gets the mouse
        }
        match ev {
            WindowEvent::CursorMoved { position, .. } if !self.relative => {
                if let Some((x, y, w, h)) = g.video_rect() {
                    c.input(Input::MouseAbs { x: ((position.x as f32 - x) / w).clamp(0.0, 1.0), y: ((position.y as f32 - y) / h).clamp(0.0, 1.0) });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Back => 3,
                    MouseButton::Forward => 4,
                    MouseButton::Other(_) => return,
                };
                c.input(Input::Button { b, down: *state == ElementState::Pressed });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => ((x * 120.0) as i32, (y * 120.0) as i32),
                    MouseScrollDelta::PixelDelta(p) => (p.x as i32, p.y as i32),
                };
                c.input(Input::Wheel { dx, dy });
            }
            WindowEvent::Focused(false) => {
                c.input(Input::ReleaseAll);
                self.mods = [false; 3];
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ drawing

    fn draw(&mut self) {
        let Some(window) = self.window().cloned() else { return };
        if self.streaming {
            if let (Some(c), Some(g)) = (&self.client, &mut self.gfx) {
                if let Some(f) = c.take_frame() {
                    g.upload(&f);
                }
            }
        }
        let Some(state) = &mut self.egui_state else { return };
        let raw = state.take_egui_input(&window);
        let ctx = self.egui_ctx.clone();
        let out = ctx.run_ui(raw, |ui| {
            if self.signal.is_none() {
                self.setup_ui(ui)
            } else if self.streaming {
                self.overlay_ui(ui.ctx())
            } else {
                self.home_ui(ui)
            }
            if !self.streaming {
                self.popup_ui(ui.ctx());
            }
            {
                self.file_offers_ui(ui.ctx());
            }
        });
        if self.streaming {
            let over = ctx.is_pointer_over_egui();
            if over != self.over_overlay {
                self.os_dirty = true; // egui changes the cursor icon over its widgets
            }
            self.over_overlay = over;
            let show = !self.relative && (self.over_overlay || self.os_cursor);
            self.set_cursor_visible(show);
        }
        if let Some(state) = &mut self.egui_state {
            state.handle_platform_output(&window, out.platform_output.clone());
        }
        // egui often needs another pass right after a change (focus, text, layout) or a timed one
        // (spinners, countdowns); without this the window can lag one change behind.
        if let Some(v) = out.viewport_output.get(&egui::ViewportId::ROOT) {
            if v.repaint_delay.is_zero() {
                window.request_redraw();
            } else if v.repaint_delay < Duration::from_secs(1) {
                self.repaint_at = Some(Instant::now() + v.repaint_delay);
            }
        }
        let show = self.streaming;
        if let Some(g) = &mut self.gfx {
            g.render(show, out, &ctx);
        }
    }

    fn setup_ui(&mut self, root: &mut egui::Ui) {
        use crate::theme as t;
        let mut go = false;
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t::BG)).show(root, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() / 2.0 - 220.0).max(30.0));
                let (r, _) = ui.allocate_exact_size(egui::vec2(84.0, 84.0), egui::Sense::hover());
                t::logo(ui.painter(), r, true);
                ui.add_space(10.0);
                ui.label(RichText::new(tr("Welcome to Kinlink")).font(egui::FontId::new(28.0, t::bold())));
                para(ui, &tl("Pick a username. Friends add you with it, plus a tag\nonly your PC can own. No account, no password."), 14.0, t::SOFT);
                ui.add_space(14.0);
                let r = ui.add(egui::TextEdit::singleline(&mut self.setup_name).hint_text(tr("username")).desired_width(300.0).margin(egui::vec2(14.0, 11.0)).font(egui::FontId::new(16.0, t::bold())));
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    go = true;
                }
                ui.add_space(6.0);
                if t::primary(ui, &tr("Continue"), None, true).clicked() {
                    go = true;
                }
                ui.label(RichText::new(tr("2–20 letters or numbers")).size(12.0).color(t::MUTED));
                if let Some(e) = &self.setup_err {
                    ui.colored_label(t::DANGER, crate::rtl::show(e));
                }
                ui.add_space(14.0);
                let dir = if crate::i18n::arabic() { egui::Layout::right_to_left(egui::Align::Center) } else { egui::Layout::left_to_right(egui::Align::Center) };
                ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 20.0), dir, |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let w = 330.0;
                    ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                    ui.label(RichText::new(tr("By continuing you agree to the")).size(12.0).color(t::MUTED));
                    if ui.link(RichText::new(tr("Terms of Use")).size(12.0)).clicked() {
                        open_url("https://github.com/IamAbood/Kinlink/blob/main/TERMS.md");
                    }
                    ui.label(RichText::new(tr("and")).size(12.0).color(t::MUTED));
                    if ui.link(RichText::new(tr("Privacy Policy")).size(12.0)).clicked() {
                        open_url("https://github.com/IamAbood/Kinlink/blob/main/PRIVACY.md");
                    }
                });
                ui.add_space(16.0);
                ui.label(RichText::new(tr("Made by Abdulkareem Alsaou")).size(12.0).color(t::MUTED));
            });
        });
        if go {
            // People naturally type "name#1234": the number is generated, so keep only the name.
            let name = self.setup_name.split('#').next().unwrap_or("").trim().to_string();
            if !crate::crypto::valid_new_name(&name) {
                self.setup_err = Some("Just the name, no #tag (it's made for you). 2–20 letters or numbers, no symbols.".into());
            } else {
                let id = Identity::new(&name);
                if id.save() {
                    self.start_services(id);
                } else {
                    self.setup_err = Some("Couldn't save your identity to disk".into());
                }
            }
        }
    }

    fn popup_ui(&mut self, ctx: &egui::Context) {
        use crate::theme as t;
        let mut decided: Option<(usize, bool)> = None;
        let sig = self.signal.clone();
        for (i, p) in self.popups.iter().enumerate() {
            let left = 30u64.saturating_sub(p.at.elapsed().as_secs());
            let f = sig.as_ref().and_then(|s| s.friend(&p.from));
            egui::Window::new("incoming")
                .id(egui::Id::new(("popup", p.req)))
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .frame(egui::Frame::NONE.fill(Color32::from_rgb(0x16, 0x1F, 0x3A)).stroke(egui::Stroke::new(1.0, t::ACCENT_LINE)).corner_radius(16.0).inner_margin(egui::Margin::same(18)).shadow(egui::Shadow { offset: [0, 14], blur: 40, spread: 0, color: Color32::from_black_alpha(140) }))
                .anchor(egui::Align2::RIGHT_BOTTOM, [-24.0, -24.0 - i as f32 * 170.0])
                .show(ctx, |ui| {
                    ui.set_width(300.0);
                    ui.horizontal(|ui| {
                        t::avatar(ui, &f.as_ref().map(|f| f.display()).unwrap_or_else(|| p.name.clone()), None, 38.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.label(RichText::new(&p.name).font(egui::FontId::new(14.0, t::bold())));
                            ui.label(RichText::new(tr("wants to connect to this PC")).size(12.0).color(t::MUTED));
                        });
                    });
                    if let Some(f) = &f {
                        let allowed: Vec<&str> = [(f.allow_mouse, "mouse"), (f.allow_keyboard, "keyboard"), (f.allow_gamepad, "controller")].iter().filter(|x| x.0).map(|x| x.1).collect();
                        let what = if allowed.is_empty() { tl("They can only watch (no mouse, keyboard or controller).") } else { tlf("Allowed: {}. Change it in Friends.", &[&allowed.iter().map(|a| tl(a)).collect::<Vec<_>>().join(", ")]) };
                        para(ui, &what, 12.0, t::SOFT);
                    }
                    ui.label(RichText::new(trf("Declines by itself in {}s", &[&left.to_string()])).size(11.0).color(t::MUTED));
                    ui.horizontal(|ui| {
                        if t::primary(ui, &tr("Accept"), None, true).clicked() {
                            decided = Some((i, true));
                        }
                        if ui.add(egui::Button::new(tr("Decline")).min_size(egui::vec2(100.0, 36.0))).clicked() {
                            decided = Some((i, false));
                        }
                    });
                });
        }
        // Friends asking for help.
        let mut help: Option<(Pub, bool)> = None;
        for (i, from) in self.help_from.iter().enumerate() {
            let Some(f) = sig.as_ref().and_then(|s| s.friend(from)) else { continue };
            egui::Window::new("help")
                .id(egui::Id::new(("help", i)))
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .frame(egui::Frame::NONE.fill(Color32::from_rgb(0x16, 0x1F, 0x3A)).stroke(egui::Stroke::new(1.0, t::ACCENT_LINE)).corner_radius(16.0).inner_margin(egui::Margin::same(18)))
                .anchor(egui::Align2::LEFT_BOTTOM, [24.0, -24.0 - i as f32 * 150.0])
                .show(ctx, |ui| {
                    ui.set_width(280.0);
                    ui.horizontal(|ui| {
                        t::avatar(ui, &f.display(), None, 38.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(f.display()).font(egui::FontId::new(14.0, t::bold())));
                            ui.label(RichText::new(tr("is asking for your help")).size(12.0).color(t::MUTED));
                        });
                    });
                    ui.horizontal(|ui| {
                        if t::primary(ui, &tr("Help"), None, self.client.is_none()).clicked() {
                            help = Some((*from, true));
                        }
                        if ui.add(egui::Button::new(tr("Not now")).min_size(egui::vec2(100.0, 36.0))).clicked() {
                            help = Some((*from, false));
                        }
                    });
                });
        }
        if let Some((from, go)) = help {
            self.help_from.retain(|p| *p != from);
            if let (true, Some(f)) = (go, sig.as_ref().and_then(|s| s.friend(&from))) {
                self.connect_to(&f);
            }
        }
        if let Some((i, yes)) = decided {
            let r = self.popups.remove(i);
            if yes {
                self.accept_incoming(r);
            } else {
                self.decline_incoming(&r, "declined the connection");
            }
        }
    }

    /// Draws the host's cursor on top of the video. While we drive the mouse, it sits at our own
    /// pointer (no network lag); otherwise (spectating, locked mouse) at the host's reported spot.
    fn draw_remote_cursor(&mut self, ctx: &egui::Context) {
        self.os_cursor = false;
        let Some((shape, px, py)) = (|| {
            let (c, g) = (self.client.as_ref()?, self.gfx.as_ref()?);
            let (rc, shape) = c.cursor();
            let (rc, shape) = (rc?, shape?);
            let (vx, vy, vw, vh) = g.video_rect()?;
            if !rc.visible || (self.over_overlay && !self.relative) {
                return None;
            }
            let drives_mouse = matches!(c.state(), State::Connected { mouse: true, .. }) && !self.relative;
            let local = self.pointer.filter(|(x, y)| drives_mouse && *x >= vx && *x <= vx + vw && *y >= vy && *y <= vy + vh);
            if local.is_some() {
                // We move the mouse: the real cursor (with the host's shape) is the only one shown.
                self.os_cursor = true;
                if self.os_shape.as_ref().is_none_or(|s| s.version != shape.version) {
                    self.os_shape = Some(shape);
                }
                return None;
            }
            Some((shape, vx + rc.x * vw, vy + rc.y * vh))
        })() else {
            return;
        };
        if self.cursor_tex.as_ref().is_none_or(|(v, _)| *v != shape.version) {
            let img = egui::ColorImage::from_rgba_unmultiplied([shape.w, shape.h], &shape.rgba);
            self.cursor_tex = Some((shape.version, ctx.load_texture("remote_cursor", img, egui::TextureOptions::NEAREST)));
        }
        let tex = &self.cursor_tex.as_ref().unwrap().1;
        let ppp = ctx.pixels_per_point();
        let min = egui::pos2((px - shape.hx) / ppp, (py - shape.hy) / ppp);
        let rect = egui::Rect::from_min_size(min, egui::vec2(shape.w as f32 / ppp, shape.h as f32 / ppp));
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("rcursor"))).image(tex.id(), rect, uv, Color32::WHITE);
    }

    /// Chat window, transfer progress and the notification bubble during a session.
    fn session_extras(&mut self, ctx: &egui::Context, frame: &egui::Frame) {
        let Some(c) = &self.client else { return };
        let finished = c.take_finished_files();
        let peer = self.peer.clone();
        for p in finished {
            if let Some(msg) = self.on_clip_transfer(&p, &peer) {
                if !msg.is_empty() {
                    self.bubble = Some((msg, Instant::now()));
                }
                continue;
            }
            let text = match (&p.failed, p.sending) {
                (Some(e), _) => format!("{}: {e}", p.name),
                (None, true) => tlf("Sent {}", &[&p.name]),
                (None, false) => tlf("Received {} (Downloads\\Kinlink)", &[&p.name]),
            };
            self.bubble = Some((text, Instant::now()));
        }
        let Some(c) = &self.client else { return };
        for n in c.take_notices() {
            self.bubble = Some((n, Instant::now()));
        }
        let (talking, mic_err) = c.voice_status();
        if let Some(e) = mic_err {
            self.bubble = Some((e, Instant::now()));
        }
        if !talking.is_empty() {
            egui::Area::new(egui::Id::new("talk")).anchor(egui::Align2::RIGHT_TOP, [-14.0, 52.0]).show(ctx, |ui| {
                frame.show(ui, |ui| {
                    for name in &talking {
                        ui.horizontal(|ui| {
                            dot(ui, GREEN);
                            ui.colored_label(Color32::WHITE, name);
                        });
                    }
                });
            });
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if c.is_recording() {
            egui::Area::new(egui::Id::new("rec")).anchor(egui::Align2::RIGHT_TOP, [-14.0, 12.0]).show(ctx, |ui| {
                frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        dot(ui, RED);
                        ui.label(RichText::new(tr("REC")).font(egui::FontId::new(12.0, crate::theme::bold())).color(crate::theme::DANGER));
                    });
                });
            });
        }
        let unread = if self.chat_open { 0 } else { c.take_unread() };
        if unread > 0 {
            if let Some((from, text)) = c.chat_log().last() {
                self.bubble = Some((format!("{from}: {text}   ({})", hk(&self.s, &tl("Ctrl+Alt+Shift+C to reply"))), Instant::now()));
            }
        }
        let transfers = c.transfers();
        let mut cancel = None;
        if !transfers.is_empty() {
            egui::Area::new(egui::Id::new("xfers")).anchor(egui::Align2::RIGHT_BOTTOM, [-14.0, -14.0]).show(ctx, |ui| {
                frame.show(ui, |ui| {
                    for p in &transfers {
                        ui.horizontal(|ui| {
                            ui.colored_label(Color32::WHITE, crate::rtl::show(&format!("{} {}", if p.sending { "Sending" } else { "Receiving" }, p.name)));
                            ui.add(egui::ProgressBar::new(p.done as f32 / p.size.max(1) as f32).desired_width(140.0).show_percentage());
                            if ui.small_button(tr("Cancel")).clicked() {
                                cancel = Some(p.id);
                            }
                        });
                    }
                });
            });
        }
        if let Some(id) = cancel {
            c.cancel_file(id);
        }
        if let Some((text, at)) = self.bubble.clone() {
            if at.elapsed() < Duration::from_secs(6) && !self.chat_open {
                egui::Area::new(egui::Id::new("bubble")).anchor(egui::Align2::CENTER_BOTTOM, [0.0, -22.0]).show(ctx, |ui| {
                    egui::Frame::NONE.fill(Color32::from_rgba_unmultiplied(27, 35, 64, 245)).stroke(egui::Stroke::new(1.0, crate::theme::ACCENT_LINE)).corner_radius(20.0).inner_margin(egui::Margin::symmetric(16, 9)).show(ui, |ui| {
                        ui.label(RichText::new(crate::rtl::show(&text)).font(egui::FontId::new(13.0, crate::theme::bold())));
                    });
                });
                ctx.request_repaint_after(Duration::from_millis(500));
            }
        }
        if self.chat_open {
            let log = c.chat_log();
            let mut send = false;
            let mut close = false;
            egui::Window::new("chat").title_bar(false).resizable(false).frame(*frame).anchor(egui::Align2::LEFT_BOTTOM, [14.0, -14.0]).show(ctx, |ui| {
                use crate::theme as t;
                ui.set_width(320.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Chat with {}", crate::rtl::show(&self.peer))).font(egui::FontId::new(14.5, t::bold())));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if t::icon_button(ui, t::Icon::Close, "Close (Esc)", false).clicked() {
                            close = true;
                        }
                    });
                });
                egui::ScrollArea::vertical().max_height(240.0).stick_to_bottom(true).show(ui, |ui| {
                    ui.set_width(320.0);
                    if log.is_empty() {
                        para(ui, &tl("Say hi. Messages go to everyone in this session."), 14.0, t::MUTED);
                    }
                    for (from, text) in log.iter().rev().take(50).rev() {
                        let mine = from == "You";
                        let layout = if mine { egui::Layout::right_to_left(egui::Align::Min) } else { egui::Layout::left_to_right(egui::Align::Min) };
                        ui.with_layout(layout, |ui| {
                            egui::Frame::NONE
                                .fill(if mine { t::ACCENT } else { t::HOVER })
                                .corner_radius(if mine { egui::CornerRadius { nw: 12, ne: 12, sw: 12, se: 4 } } else { egui::CornerRadius { nw: 12, ne: 12, sw: 4, se: 12 } })
                                .inner_margin(egui::Margin::symmetric(10, 7))
                                .show(ui, |ui| {
                                    ui.set_max_width(240.0);
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        if !mine {
                                            ui.label(RichText::new(crate::rtl::show(from)).size(11.0).color(t::ACCENT_SOFT));
                                        }
                                        ui.label(RichText::new(crate::rtl::show(text)).color(Color32::WHITE));
                                    });
                                });
                        });
                    }
                });
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter)); // before the text box consumes it
                let r = ui.add(egui::TextEdit::singleline(&mut self.chat_text).hint_text(tr("Type and press Enter")).desired_width(320.0).margin(egui::vec2(10.0, 8.0)));
                r.request_focus();
                if crate::rtl::has_rtl(&self.chat_text) {
                    ui.colored_label(Color32::WHITE, crate::rtl::show(&self.chat_text)); // how it will look
                }
                if enter {
                    send = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
            if send && !self.chat_text.trim().is_empty() {
                c.say(self.chat_text.trim());
                self.chat_text.clear();
            }
            if close {
                self.set_chat(false);
            }
        }
    }

    /// Panic button (hotkey, tray, or host panel): disconnect every guest right now.
    fn panic(&mut self) {
        let Some(h) = &self.host else { return };
        let had = !h.status().clients.is_empty();
        h.kick_all();
        for p in std::mem::take(&mut self.popups) {
            self.decline_incoming(&p, "isn't accepting connections right now");
        }
        // Pause sharing too, or friends allowed to "connect without asking" would just come back.
        self.host = None;
        self.s.hosting = false;
        self.s.save();
        let who = if had { tl("everyone was disconnected") } else { tl("nobody was connected") };
        self.toast(tlf("Panic button: {}. Sharing is paused; turn on \"Let friends connect\" to share again.", &[&who]));
        self.show_window();
    }

    fn overlay_ui(&mut self, ctx: &egui::Context) {
        use crate::theme::{self as t, Icon};
        self.draw_remote_cursor(ctx);
        let Some(c) = &self.client else { return };
        if self.s.show_stats {
            let st = c.stats();
            egui::Area::new(egui::Id::new("stats_hud")).anchor(egui::Align2::RIGHT_TOP, [-10.0, 10.0]).interactable(false).show(ctx, |ui| {
                egui::Frame::NONE.fill(Color32::from_black_alpha(150)).corner_radius(8.0).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
                    let col = if st.rtt_ms > 80.0 || st.fps < 20.0 { Color32::from_rgb(0xFF, 0xC9, 0x8A) } else { Color32::from_rgb(0x9B, 0xF0, 0xC8) };
                    ui.add(egui::Label::new(RichText::new(format!("{:.0} fps · {:.0} ms · {:.1} Mbps · lost {}", st.fps, st.rtt_ms, st.mbps, st.lost)).monospace().size(11.0).color(col)).extend());
                });
            });
        }
        if !self.overlay_shown {
            // Menu hidden (Ctrl+Alt+Shift+O): chat, transfers and bubbles still work.
            let frame = egui::Frame::NONE.fill(Color32::from_rgba_unmultiplied(15, 22, 41, 246)).inner_margin(14.0).corner_radius(16.0);
            self.session_extras(ctx, &frame);
            return;
        }
        let st = c.stats();
        let state = c.state();
        let mut disconnect = false;
        let mut quality_changed = false;
        let mut toggle_chat = false;
        let mut shot = false;
        let mut pick = false;
        let mut toggle_full = false;
        let is_full = self.window().is_some_and(|w| w.fullscreen().is_some());
        let frame = egui::Frame::NONE.fill(Color32::from_rgba_unmultiplied(15, 22, 41, 246)).inner_margin(14.0).corner_radius(16.0).stroke(egui::Stroke::new(1.0, crate::theme::LINE2));
        let near_corner = self.pointer.is_some_and(|(x, y)| x < 180.0 && y < 140.0) && !self.relative;
        let reveal = near_corner || self.stream_started.elapsed() < Duration::from_secs(4);
        let mut toggle_rel = false;
        let mut hide_menu = false;
        if !self.overlay_open && reveal {
            // Closed: a small menu button, only shown when the mouse is near the corner.
            egui::Area::new(egui::Id::new("menu_btn")).fixed_pos([12.0, 12.0]).show(ctx, |ui| {
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(40.0, 32.0), egui::Sense::click());
                let strong = resp.hovered();
                let p = ui.painter();
                p.rect(rect, 10.0, Color32::from_rgba_unmultiplied(11, 16, 32, if strong { 240 } else { 150 }), egui::Stroke::new(1.0, if strong { t::ACCENT_LINE } else { Color32::from_rgba_unmultiplied(52, 48, 122, 120) }), egui::StrokeKind::Inside);
                t::icon(p, egui::Rect::from_center_size(rect.center(), egui::vec2(18.0, 18.0)), Icon::Menu, if strong { t::ACCENT_SOFT } else { t::SOFT });
                if resp.on_hover_text(tr("Menu")).clicked() {
                    self.overlay_open = true;
                    self.overlay_opened = Instant::now();
                }
            });
        }
        egui::Window::new("overlay")
            .open(&mut self.overlay_open.clone())
            .title_bar(false)
            .resizable(false)
            .frame(frame)
            .default_pos([12.0, 12.0])
            .show(ctx, |ui| {
                ui.set_width(300.0);
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 10.0);
                // header
                ui.horizontal(|ui| {
                    dot_fn(ui, if st.lost == 0 || st.rtt_ms < 80.0 { t::MINT } else { RED });
                    ui.label(RichText::new(crate::rtl::show(&self.peer)).font(egui::FontId::new(14.5, t::bold())));
                    ui.label(RichText::new(format!("· {:.0} ms", st.rtt_ms)).color(t::MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if t::icon_button(ui, Icon::Close, &tr("Hide menu"), false).clicked() {
                            self.overlay_open = false;
                        }
                    });
                });
                // actions
                let w = (300.0 - 3.0 * 6.0) / 4.0;
                let mic = c.mic_on();
                let rec = c.is_recording();
                ui.horizontal(|ui| {
                    if t::tile(ui, Icon::Mic, &if mic { tr("Mic on") } else { tr("Mic off") }, mic.then_some(t::MINT), w).on_hover_text(hk(&self.s, &tr("Voice chat · Ctrl+Alt+Shift+V · use headphones"))).clicked() {
                        c.set_mic(!mic);
                    }
                    if t::tile(ui, Icon::Chat, &tr("Chat"), self.chat_open.then_some(t::ACCENT_SOFT), w).on_hover_text(hk(&self.s, &tr("Ctrl+Alt+Shift+C"))).clicked() {
                        toggle_chat = true;
                    }
                    if t::tile(ui, Icon::Upload, &tr("Send file"), None, w).on_hover_text(tr("Or drag files onto this window")).clicked() {
                        pick = true;
                    }
                    if t::tile(ui, Icon::Camera, &tr("Screenshot"), None, w).on_hover_text(hk(&self.s, &tr("Ctrl+Alt+Shift+P · Pictures\\Kinlink"))).clicked() {
                        shot = true;
                    }
                });
                ui.horizontal(|ui| {
                    if t::tile(ui, Icon::Record, &if rec { tr("Stop rec") } else { tr("Record") }, rec.then_some(t::DANGER), w).on_hover_text(tr("MP4 in Videos\\Kinlink")).clicked() {
                        if rec { c.stop_recording() } else { c.start_recording() }
                    }
                    if t::tile(ui, if is_full { Icon::Shrink } else { Icon::Expand }, &if is_full { tr("Windowed") } else { tr("Fullscreen") }, None, w).on_hover_text(hk(&self.s, &tr("Ctrl+Alt+Shift+F"))).clicked() {
                        toggle_full = true;
                    }
                    if t::tile(ui, Icon::Lock, &tr("Lock mouse"), self.relative.then_some(t::ACCENT_SOFT), w).on_hover_text(hk(&self.s, &tr("For games · Ctrl+Alt+Shift+R to unlock"))).clicked() {
                        toggle_rel = true;
                    }
                    if t::tile(ui, Icon::Hide, &tr("Hide all"), None, w).on_hover_text(hk(&self.s, &tr("Hides the menu button too · Ctrl+Alt+Shift+O brings it back"))).clicked() {
                        hide_menu = true;
                    }
                });
                // stream settings
                egui::Grid::new("ov").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label(RichText::new(tr("Preset")).color(t::SOFT));
                    ui.horizontal(|ui| {
                        for (name, r, q, sharp) in PRESETS {
                            let on = (self.res_idx, self.qual_idx, self.sharp) == (*r, *q, *sharp);
                            if ui.selectable_label(on, tr(name)).on_hover_text(tr(preset_hint(name))).clicked() && !on {
                                (self.res_idx, self.qual_idx, self.sharp) = (*r, *q, *sharp);
                                quality_changed = true;
                            }
                        }
                    });
                    ui.end_row();
                    ui.label(RichText::new(tr("Resolution")).color(t::SOFT));
                    egui::ComboBox::from_id_salt("res").width(150.0).selected_text(tr(RESOLUTIONS[self.res_idx].0)).show_ui(ui, |ui| {
                        for (i, (name, _)) in RESOLUTIONS.iter().enumerate() {
                            quality_changed |= ui.selectable_value(&mut self.res_idx, i, tr(name)).changed();
                        }
                    });
                    ui.end_row();
                    ui.label(RichText::new(tr("Quality")).color(t::SOFT));
                    egui::ComboBox::from_id_salt("q").width(150.0).selected_text(tr(QUALITIES[self.qual_idx].0)).show_ui(ui, |ui| {
                        for (i, (name, _)) in QUALITIES.iter().enumerate() {
                            quality_changed |= ui.selectable_value(&mut self.qual_idx, i, tr(name)).changed();
                        }
                    });
                    ui.end_row();
                    if let State::Connected { monitors, current, .. } = &state {
                        if monitors.len() > 1 {
                            ui.label(RichText::new(tr("Monitor")).color(t::SOFT));
                            ui.horizontal(|ui| {
                                for i in 0..monitors.len() {
                                    if ui.selectable_label(i == *current as usize, format!(" {} ", i + 1)).clicked() {
                                        c.select_monitor(i as u8);
                                    }
                                }
                            });
                            ui.end_row();
                        }
                    }
                    ui.label(RichText::new(tr("Volume")).color(t::SOFT));
                    ui.horizontal(|ui| {
                        let a = ui.add(egui::Slider::new(&mut self.volume, 0..=100).show_value(false)).changed();
                        let b = ui.checkbox(&mut self.muted, tr("Mute")).changed();
                        if a || b {
                            c.set_volume(if self.muted { 0 } else { self.volume });
                        }
                    });
                    ui.end_row();
                });
                if let State::Connected { mouse, keyboard, gamepad, .. } = state {
                    if !(mouse && keyboard && gamepad) {
                        let allowed: Vec<&str> = [(mouse, "mouse"), (keyboard, "keyboard"), (gamepad, "controller")].iter().filter(|x| x.0).map(|x| x.1).collect();
                        let what = if allowed.is_empty() { tl("view only (spectator)") } else { tlf("{} only", &[&allowed.iter().map(|a| tl(a)).collect::<Vec<_>>().join(" + ")]) };
                        ui.label(RichText::new(trf("Host allows {}", &[&what])).color(Color32::from_rgb(0xFF, 0xC9, 0x8A)));
                    }
                }
                egui::Frame::NONE.fill(t::BG).corner_radius(9.0).inner_margin(egui::Margin::symmetric(10, 7)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(format!("{:.0} fps · {:.0} ms · {:.1} Mbps · decode {:.1} ms · lost {}", st.fps, st.rtt_ms, st.mbps, st.decode_ms, st.lost)).monospace().size(11.0).color(t::MUTED));
                });
                if ui.checkbox(&mut self.s.show_stats, tr("Keep these numbers on screen")).changed() {
                    self.s.save();
                }
                para(ui, &hk(&self.s, &tl("Ctrl+Alt+Shift: Q disconnect · O hide · R lock mouse · F fullscreen")), 11.0, t::MUTED);
                let fresh = self.overlay_opened.elapsed() < Duration::from_millis(500);
                if t::danger_wide(ui, &tr("Disconnect"), !fresh).clicked() && !fresh {
                    disconnect = true;
                }
            });
        if toggle_rel {
            self.set_relative(!self.relative);
            self.overlay_open = false;
        }
        if hide_menu {
            self.overlay_shown = false;
            self.overlay_open = false;
        }
        if toggle_full {
            self.toggle_fullscreen();
        }
        if quality_changed {
            self.apply_quality();
        }
        if toggle_chat {
            self.set_chat(!self.chat_open);
        }
        if pick {
            self.pick_and_send();
        }
        if shot {
            self.screenshot();
        }
        if disconnect {
            self.leave_stream(None);
            return;
        }
        self.session_extras(ctx, &frame);
        if self.show_hint && self.stream_started.elapsed() < Duration::from_secs(6) && !self.overlay_open {
            egui::Area::new(egui::Id::new("hint")).anchor(egui::Align2::CENTER_TOP, [0.0, 14.0]).show(ctx, |ui| {
                frame.show(ui, |ui| {
                    ui.colored_label(Color32::WHITE, hk(&self.s, &tr("Menu: small button top-left · Ctrl+Alt+Shift+Q to disconnect")));
                });
            });
        }
    }

    fn home_ui(&mut self, root: &mut egui::Ui) {
        use crate::theme::{self as t, Icon};
        let Some(sig) = self.signal.clone() else { return };
        let mut connect: Option<Friend> = None;
        let mut host_toggle = None;
        let mut restart_host = false;
        let mut panic_clicked = false;
        let mut move_pad: Option<(usize, bool)> = None;
        let mut rename_now = false;
        let mut go: Option<Page> = None;
        let mut pick = false;
        let hstat = self.host.as_ref().map(|h| h.status());
        let guests = hstat.as_ref().map(|s| s.clients.len()).unwrap_or(0);

        // ------------------------------------------------ sidebar
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(228.0)
            .frame(egui::Frame::NONE.fill(t::SIDEBAR).inner_margin(egui::Margin::symmetric(14, 18)))
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(38.0, 38.0), egui::Sense::hover());
                    t::logo(ui.painter(), r, true);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new(tr("Kinlink")).font(egui::FontId::new(17.0, t::bold())));
                        ui.label(RichText::new(format!("v{}", crate::updater::current())).size(11.0).color(t::MUTED));
                    });
                });
                ui.add_space(14.0);
                ui.spacing_mut().item_spacing.y = 4.0;
                if t::nav(ui, Icon::Friends, &tr("Friends"), self.page == Page::Friends, Some(sig.incoming().len() + sig.total_unread() as usize)).clicked() {
                    go = Some(Page::Friends);
                }
                if t::nav(ui, Icon::Monitor, &tr("This PC"), self.page == Page::Pc, Some(guests)).clicked() {
                    go = Some(Page::Pc);
                }
                if t::nav(ui, Icon::Files, &tr("Files"), self.page == Page::Files, None).clicked() {
                    go = Some(Page::Files);
                }
                if t::nav(ui, Icon::Settings, &tr("Settings"), self.page == Page::Settings, None).clicked() {
                    go = Some(Page::Settings);
                }
                egui::Panel::bottom("sidebar_bottom").resizable(false).show_separator_line(false).frame(egui::Frame::NONE).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 10.0;
                    let u = self.update.lock().unwrap().clone();
                    match u {
                        Update::Available(r) => {
                            if t::primary(ui, &trf("Update to {}", &[&r.version]), Some(Icon::Update), true).on_hover_text(tr("Downloads, checks and restarts the app")).clicked() {
                                let (upd, wake) = (self.update.clone(), Arc::new(self.waker()));
                                *upd.lock().unwrap() = Update::Installing(0.0);
                                std::thread::spawn(move || {
                                    let u2 = upd.clone();
                                    let w2 = wake.clone();
                                    let res = crate::updater::install(&r, move |p| {
                                        *u2.lock().unwrap() = Update::Installing(p);
                                        w2();
                                    });
                                    *upd.lock().unwrap() = match res {
                                        Ok(()) => Update::Done,
                                        Err(e) => Update::Failed(format!("{e:#}")),
                                    };
                                    wake();
                                });
                            }
                        }
                        Update::Installing(p) => {
                            ui.add(egui::ProgressBar::new(p).desired_width(ui.available_width()).text("Updating…"));
                        }
                        Update::Failed(e) => {
                            ui.colored_label(t::DANGER, tr("Update failed")).on_hover_text(e);
                        }
                        _ => {}
                    }
                    t::card().fill(t::FIELD).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 6.0;
                        ui.horizontal(|ui| {
                            t::section(ui, &tr("Your ID"));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if t::icon_button(ui, Icon::Copy, &tr("Copy your ID"), false).clicked() {
                                    ui.ctx().copy_text(sig.id());
                                    self.toast(tl("Your ID was copied. Send it to a friend so they can add you."));
                                }
                            });
                        });
                        ui.horizontal(|ui| {
                            let id = sig.id();
                            let (name, tag) = id.split_once('#').unwrap_or((&id, ""));
                            ui.spacing_mut().item_spacing.x = 0.0;
                            ui.label(RichText::new(name).font(egui::FontId::new(14.0, t::bold())));
                            ui.label(RichText::new(format!("#{tag}")).font(egui::FontId::new(14.0, t::bold())).color(t::ACCENT_SOFT));
                        });
                        // My status, shown to friends.
                        let presets = ["", "Busy", "Away", "In a game"];
                        let shown = if self.s.status.is_empty() { tr("Available") } else { crate::rtl::show(&crate::i18n::tl(&self.s.status)) };
                        let mut pick: Option<String> = None;
                        egui::ComboBox::from_id_salt("status").width(ui.available_width() - 4.0).selected_text(shown).show_ui(ui, |ui| {
                            for p in presets {
                                let label = if p.is_empty() { tr("Available") } else { tr(p) };
                                if ui.selectable_label(self.s.status == p, label).clicked() {
                                    pick = Some(p.to_string());
                                }
                            }
                            if ui.selectable_label(false, tr("Custom…")).clicked() {
                                self.status_edit = Some(self.s.status.clone());
                            }
                        });
                        if let Some(text) = &mut self.status_edit {
                            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                            let r = ui.add(egui::TextEdit::singleline(text).hint_text(tr("e.g. Playing FIFA")).desired_width(ui.available_width()).char_limit(40));
                            r.request_focus();
                            if enter || r.lost_focus() {
                                pick = Some(text.clone());
                            }
                        }
                        if let Some(p) = pick {
                            self.status_edit = None;
                            self.s.status = p.trim().to_string();
                            self.s.save();
                            sig.set_status(&self.s.status);
                        }
                        ui.horizontal(|ui| {
                            let (c, text) = if sig.online() {
                                (t::MINT, if self.host.is_some() { tr("Online · hosting on") } else { tr("Online · hosting off") })
                            } else {
                                (t::MUTED, tr("Connecting…"))
                            };
                            dot(ui, c);
                            ui.label(RichText::new(text).size(12.0).color(if sig.online() { t::MINT_SOFT } else { t::MUTED }));
                        });
                    });
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(tr("Made by Abdulkareem Alsaou")).size(11.0).color(t::MUTED));
                    });
                });
            });

        // ------------------------------------------------ status bar (connecting, messages)
        egui::Panel::bottom("bottom").frame(egui::Frame::NONE.fill(t::BG).inner_margin(egui::Margin::symmetric(30, 8))).show(root, |ui| {
            if let Some(o) = &self.outgoing {
                let name = o.name.clone();
                let mut cancel = false;
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(trf("Waiting for {} to accept…", &[&name]));
                    cancel = ui.button(tr("Cancel")).clicked();
                });
                log_tail(ui);
                if cancel {
                    self.cancel_outgoing();
                }
            } else if self.client.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(trf("Connecting to {}…", &[&self.peer]));
                });
                log_tail(ui);
            } else if let Some((m, _)) = &self.message {
                let m = m.clone();
                ui.horizontal(|ui| {
                    ui.label(crate::rtl::show(&m));
                    if m.contains("Downloads") && ui.small_button(tr("Open folder")).clicked() {
                        open_folder(crate::files::default_save_dir());
                    }
                });
            } else {
                para(ui, &hk(&self.s, &tl("In a session: Ctrl+Alt+Shift + Q disconnect · O hide menu · R lock mouse · F fullscreen")), 11.0, t::MUTED);
            }
        });

        // ------------------------------------------------ pages
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t::BG).inner_margin(egui::Margin { left: 30, right: 30, top: 24, bottom: 8 })).show(root, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 12.0;
                match self.page {
                    // ---------------------------------------- Friends
                    Page::Friends => {
                        let mut friends = sig.friends();
                        let online = friends.iter().filter(|(_, on)| *on).count();
                        t::title(ui, &tr("Friends"), Some(&trf("{} of {} online", &[&online.to_string(), &friends.len().to_string()])));
                        ui.horizontal(|ui| {
                            let enter_now = ui.input(|i| i.key_pressed(egui::Key::Enter));
                            let r = ui.add(egui::TextEdit::singleline(&mut self.add_text).hint_text(tr("Add a friend: name#1234-ABCDEF")).desired_width(230.0).margin(egui::vec2(10.0, 8.0)));
                            let enter = (r.has_focus() || r.lost_focus()) && enter_now;
                            if t::primary(ui, &tr("Add"), Some(Icon::Plus), true).clicked() || enter {
                                match sig.add_friend(&self.add_text) {
                                    Ok(()) => {
                                        self.add_err = None;
                                        self.add_text.clear();
                                    }
                                    Err(e) => self.add_err = Some(e),
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            let enter_now = ui.input(|i| i.key_pressed(egui::Key::Enter));
                            let r = ui.add(egui::TextEdit::singleline(&mut self.join_text).hint_text(tr("Got an invite? Paste the link here")).desired_width(230.0).margin(egui::vec2(10.0, 8.0)));
                            let enter = (r.has_focus() || r.lost_focus()) && enter_now;
                            if ui.add(egui::Button::new(tr("Join")).min_size(egui::vec2(70.0, 34.0))).clicked() || enter {
                                let t = std::mem::take(&mut self.join_text);
                                self.start_join(&t);
                            }
                        });
                        if let Some(e) = &self.add_err {
                            ui.colored_label(t::DANGER, crate::rtl::show(e));
                        }
                        for (id, p) in sig.incoming() {
                            egui::Frame::NONE.fill(t::ACCENT_BG).stroke(egui::Stroke::new(1.0, t::ACCENT_LINE)).corner_radius(12.0).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                para(ui, &tlf("Safety code {}: it must match on their screen. If it doesn't, decline.", &[&sig.safety_code(&p)]), 12.0, t::SOFT);
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&id).font(egui::FontId::new(14.0, t::bold())));
                                    ui.label(tr("wants to be friends"));
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.button(tr("Decline")).clicked() {
                                            sig.decline(p);
                                        }
                                        if t::primary(ui, &tr("Accept"), None, true).clicked() {
                                            sig.accept(p);
                                        }
                                    });
                                });
                            });
                        }
                        for o in sig.outgoing_info() {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(trf("Waiting for {} to accept", &[&o.id])).italics().color(t::MUTED));
                                if let Some(code) = &o.code {
                                    ui.label(RichText::new(trf("· safety code {}", &[code])).color(t::SOFT));
                                }
                                if ui.small_button(tr("Cancel")).clicked() {
                                    sig.cancel_request(&o.id);
                                }
                            });
                            if o.conflict {
                                para(ui, &tl("Two different PCs answered for this ID. Only accept if the safety code matches on your friend's screen."), 13.0, t::DANGER);
                            }
                        }
                        friends.sort_by_key(|(f, on)| (!on, f.display().to_lowercase()));
                        if friends.is_empty() {
                            t::card().show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.label(RichText::new(tr("No friends yet")).font(egui::FontId::new(16.0, t::bold())));
                                para(ui, &tl("Send someone your ID (bottom left) and add theirs above. They accept, and you can connect."), 14.0, t::SOFT);
                            });
                        }
                        let mut groups: std::collections::BTreeMap<String, Vec<(Friend, bool)>> = Default::default();
                        for (f, on) in friends {
                            groups.entry(f.group.trim().to_string()).or_default().push((f, on));
                        }
                        let grouped = groups.len() > 1 || groups.keys().any(|g| !g.is_empty());
                        for (group, members) in groups {
                            if grouped {
                                let on = members.iter().filter(|(_, o)| *o).count();
                                let title = if group.is_empty() { tl("Other") } else { group.clone() };
                                t::section(ui, &trf("{} · {}/{} online", &[&title, &on.to_string(), &members.len().to_string()]));
                                if !group.is_empty() {
                                    let mut set: Option<bool> = None;
                                    ui.horizontal(|ui| {
                                        if ui.small_button(tr("Allow control for this group")).on_hover_text(tr("Mouse and keyboard for everyone in the group")).clicked() {
                                            set = Some(true);
                                        }
                                        if ui.small_button(tr("View only for this group")).clicked() {
                                            set = Some(false);
                                        }
                                    });
                                    if let Some(v) = set {
                                        for (f, _) in &members {
                                            let mut g = f.clone();
                                            (g.allow_mouse, g.allow_keyboard) = (v, v);
                                            if let Some(h) = &self.host {
                                                h.set_perms(&g.id(), Perms { mouse: v, keyboard: v, gamepad: g.allow_gamepad });
                                            }
                                            sig.update(g);
                                        }
                                        self.toast(if v { tlf("Everyone in {} can now use mouse and keyboard", &[&group]) } else { tlf("Everyone in {} can now only watch", &[&group]) });
                                    }
                                }
                            }
                            t::card().inner_margin(egui::Margin::symmetric(14, 6)).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                self.friend_rows(ui, &sig, members, &mut connect);
                            });
                        }
                    }

                    // ---------------------------------------- This PC
                    Page::Pc => {
                        let sub = if self.host.is_some() { "Friends can ask to connect to this PC" } else { "Not accepting connections" };
                        ui.horizontal(|ui| {
                            t::title(ui, &tr("This PC"), Some(sub));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let mut hosting = self.s.hosting;
                                if t::switch(ui, &mut hosting, &tr("Let friends connect")) {
                                    host_toggle = Some(hosting);
                                }
                            });
                        });
                        if let (Some(h), Some(st)) = (&self.host, &hstat) {
                            ui.columns(3, |c| {
                                let stats = [
                                    (st.clients.len().to_string(), if st.clients.len() == 1 { "guest connected".to_string() } else { "guests connected".to_string() }),
                                    (if st.encoder.is_empty() { "Idle".to_string() } else { "Streaming".to_string() }, if st.encoder.is_empty() { "starts when a friend joins".to_string() } else { st.encoder.clone() }),
                                    (format!("{} Mbps", self.s.max_mbps), trf("max · {} fps", &[&self.s.fps.to_string()])),
                                ];
                                for (col, (big, small)) in c.iter_mut().zip(stats) {
                                    t::card().show(col, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        ui.label(RichText::new(big).font(egui::FontId::new(20.0, t::bold())));
                                        ui.label(RichText::new(small).size(11.5).color(t::MUTED));
                                    });
                                }
                            });
                            if !st.stats.is_empty() {
                                ui.label(RichText::new(&st.stats).size(11.5).color(t::MUTED));
                            }
                            if !st.clients.is_empty() {
                                t::section(ui, &tr("Guests"));
                                t::card().show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    for c in &st.clients {
                                        ui.horizontal(|ui| {
                                            dot(ui, t::MINT);
                                            ui.label(RichText::new(c).font(egui::FontId::new(14.0, t::bold())));
                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                if ui.button(RichText::new(tr("Kick")).color(t::DANGER)).clicked() {
                                                    h.kick(c);
                                                }
                                            });
                                        });
                                    }
                                });
                                ui.horizontal(|ui| {
                                    let mut mic = h.mic_on();
                                    if t::switch(ui, &mut mic, &tr("Talk to guests (mic · use headphones)")) {
                                        h.set_mic(mic);
                                    }
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.button(RichText::new(tr("Panic: kick everyone")).color(t::DANGER)).on_hover_text(tr("Ctrl+Alt+Shift+F12 anywhere")).clicked() {
                                            panic_clicked = true;
                                        }
                                        if ui.button(tr("Send file…")).clicked() {
                                            pick = true;
                                        }
                                    });
                                });
                                let (talking, mic_err) = h.voice_status();
                                if !talking.is_empty() {
                                    ui.label(RichText::new(trf("Talking: {}", &[&talking.join(", ")])).color(t::MINT_SOFT));
                                }
                                if let Some(e) = mic_err {
                                    ui.colored_label(t::DANGER, crate::rtl::show(&e));
                                }
                                t::section(ui, &tr("Chat"));
                                t::card().show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    egui::ScrollArea::vertical().id_salt("hostchat").max_height(140.0).stick_to_bottom(true).show(ui, |ui| {
                                        if st.chat.is_empty() {
                                            ui.label(RichText::new(tr("No messages yet")).color(t::MUTED));
                                        }
                                        for (from, text) in st.chat.iter().rev().take(30).rev() {
                                            ui.label(crate::rtl::show(&format!("{from}: {text}")));
                                        }
                                    });
                                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                                    let r = ui.add(egui::TextEdit::singleline(&mut self.host_chat_text).hint_text(tr("Message your guests")).desired_width(f32::INFINITY).margin(egui::vec2(10.0, 8.0)));
                                    if crate::rtl::has_rtl(&self.host_chat_text) {
                                        ui.label(crate::rtl::show(&self.host_chat_text));
                                    }
                                    if (r.has_focus() || r.lost_focus()) && enter && !self.host_chat_text.trim().is_empty() {
                                        h.say(self.host_chat_text.trim());
                                        self.host_chat_text.clear();
                                        r.request_focus();
                                    }
                                });
                                let pads = h.pads();
                                if !pads.is_empty() {
                                    t::section(ui, &tr("Controllers"));
                                    t::card().show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        let n = pads.len();
                                        for (i, p) in pads.iter().enumerate() {
                                            ui.horizontal(|ui| {
                                                let slot = p.player.map(|n| trf("Player {}", &[&n.to_string()])).unwrap_or_else(|| format!("#{}", i + 1));
                                                let kind = if p.playstation { "PlayStation" } else { "Xbox" };
                                                ui.label(RichText::new(slot).font(egui::FontId::new(14.0, t::bold())));
                                                ui.label(RichText::new(format!("{} · pad {} · {kind}", p.guest, p.pad + 1)).color(t::SOFT));
                                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                    if i + 1 < n && ui.small_button(tr("Down")).clicked() {
                                                        move_pad = Some((i, false));
                                                    }
                                                    if i > 0 && ui.small_button(tr("Up")).clicked() {
                                                        move_pad = Some((i, true));
                                                    }
                                                });
                                            });
                                        }
                                    });
                                }
                            }
                            if let Some(e) = &st.error {
                                ui.colored_label(t::DANGER, crate::rtl::show(e));
                            }
                            for n in &st.notes {
                                ui.label(RichText::new(n).size(11.5).color(t::MUTED));
                            }
                        } else {
                            egui::Frame::NONE.stroke(egui::Stroke::new(1.0, t::LINE2)).corner_radius(14.0).inner_margin(egui::Margin::same(28)).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.vertical_centered(|ui| {
                                    para(ui, &tl("Hosting is off. Friends can't connect to this PC until you turn it on."), 14.0, t::SOFT);
                                });
                            });
                        }
                        if let Some(e) = &self.host_err {
                            ui.colored_label(t::DANGER, crate::rtl::show(e));
                        }
                        if self.host.is_some() {
                            t::section(ui, &tr("Invite someone (no friend request needed)"));
                            t::card().show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(RichText::new(tr("They may use:")).color(t::SOFT));
                                    t::switch(ui, &mut self.invite_perms.mouse, &tr("Mouse"));
                                    t::switch(ui, &mut self.invite_perms.keyboard, &tr("Keyboard"));
                                    t::switch(ui, &mut self.invite_perms.gamepad, &tr("Controller"));
                                });
                                ui.horizontal(|ui| {
                                    if t::primary(ui, &tr("Create invite"), Some(Icon::Plus), true).clicked() {
                                        let p = self.invite_perms;
                                        let link = sig.create_invite(p.mouse, p.keyboard, p.gamepad);
                                        ui.ctx().copy_text(link.clone());
                                        self.toast(tl("Invite link copied. Send it to them: works once, for 1 hour."));
                                        self.last_invite = Some(link);
                                    }
                                    ui.label(RichText::new(tr("One use, valid for 1 hour")).size(11.5).color(t::MUTED));
                                });
                                ui.horizontal(|ui| {
                                    let host = self.host.as_ref().map(|h| h.handle());
                                    if ui.add_enabled(host.is_some(), egui::Button::new(tr("Browser link"))).on_hover_text(tr("For someone without Kinlink: they open it in Chrome, Edge, Firefox or Safari. Mouse and keyboard only.")).clicked() {
                                        if let Some(h) = host {
                                            let link = h.web_link(self.invite_perms, false);
                                            ui.ctx().copy_text(link);
                                            self.toast(tl("Browser link copied. Anyone who opens it can join once, within 1 hour."));
                                        }
                                    }
                                    ui.label(RichText::new(tr("No install needed")).size(11.5).color(t::MUTED));
                                });
                                ui.horizontal(|ui| {
                                    let host = self.host.as_ref().map(|h| h.handle());
                                    if ui.add_enabled(host.is_some(), egui::Button::new(tr("Watch party link"))).on_hover_text(tr("Up to 8 people watch your screen from their browsers at once (no control).")).clicked() {
                                        if let Some(h) = host {
                                            let link = h.web_link(Perms { mouse: false, keyboard: false, gamepad: false }, true);
                                            ui.ctx().copy_text(link);
                                            self.toast(tl("Watch party link copied: up to 8 people can watch for 3 hours."));
                                        }
                                    }
                                    ui.label(RichText::new(tr("View only, many people")).size(11.5).color(t::MUTED));
                                });
                                for inv in sig.invites() {
                                    let mins = inv.expires.saturating_duration_since(Instant::now()).as_secs() / 60 + 1;
                                    let what: Vec<String> = [(inv.mouse, "mouse"), (inv.keyboard, "keyboard"), (inv.gamepad, "controller")].iter().filter(|x| x.0).map(|x| tl(x.1)).collect();
                                    let what = if what.is_empty() { tl("view only (spectator)") } else { what.join(", ") };
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(&inv.code).monospace().color(t::ACCENT_SOFT));
                                        ui.label(RichText::new(trf("{} · {} min left", &[&what, &mins.to_string()])).size(12.0).color(t::SOFT));
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            if ui.small_button(tr("Revoke")).clicked() {
                                                sig.revoke_invite(&inv.code);
                                            }
                                            if ui.small_button(tr("Copy link")).clicked() {
                                                ui.ctx().copy_text(sig.invite_link(&inv.code));
                                            }
                                        });
                                    });
                                }
                            });
                        }
                        ui.horizontal(|ui| {
                            t::section(ui, &tr("Recent sessions"));
                            if !self.history.is_empty() {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.small_button(tr("Clear")).clicked() {
                                        self.history.clear();
                                        save_history(&self.history);
                                    }
                                });
                            }
                        });
                        t::card().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let live: Vec<(String, String, u64)> = self.live_sessions.iter().map(|(g, (start, at))| (g.clone(), start.clone(), at.elapsed().as_secs())).collect();
                            if live.is_empty() && self.history.is_empty() {
                                ui.label(RichText::new(tr("Nobody has connected yet")).color(t::MUTED));
                            }
                            for (g, start, secs) in &live {
                                ui.horizontal(|ui| {
                                    dot(ui, t::MINT);
                                    ui.label(RichText::new(g).font(egui::FontId::new(13.5, t::bold())));
                                    ui.label(RichText::new(trf("{} · connected now ({})", &[start, &duration_text(*secs)])).size(12.0).color(t::MINT_SOFT));
                                });
                            }
                            for h in self.history.iter().rev().take(15) {
                                ui.horizontal(|ui| {
                                    dot(ui, t::LINE2);
                                    ui.label(RichText::new(&h.guest).font(egui::FontId::new(13.5, t::bold())));
                                    ui.label(RichText::new(format!("{} · {}", h.start, duration_text(h.secs))).size(12.0).color(t::MUTED));
                                });
                            }
                        });
                    }

                    // ---------------------------------------- Files
                    Page::Files => {
                        ui.horizontal(|ui| {
                            t::title(ui, &tr("Files"), Some(&tr("Received files go to Downloads\\Kinlink")));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button(tr("Open Downloads")).clicked() {
                                    open_folder(crate::files::default_save_dir());
                                }
                            });
                        });
                        egui::Frame::NONE.fill(Color32::from_rgb(0x11, 0x0F, 0x2C)).stroke(egui::Stroke::new(1.5, t::ACCENT_LINE)).corner_radius(16.0).inner_margin(egui::Margin::same(22)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.vertical_centered(|ui| {
                                let (r, _) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
                                t::icon(ui.painter(), r, Icon::Upload, t::ACCENT_SOFT);
                                ui.label(RichText::new(tr("Drop files on this window to send them")).font(egui::FontId::new(15.0, t::bold())));
                                if guests > 0 {
                                    if t::primary(ui, &tr("Choose a file…"), None, true).clicked() {
                                        pick = true;
                                    }
                                    ui.label(RichText::new(trf("Goes to your {} guest(s)", &[&guests.to_string()])).size(12.0).color(t::MUTED));
                                } else {
                                    para(ui, &tl("Sends to whoever you're connected with: guests on this PC, or the PC you're controlling (use Send file in its menu)."), 12.0, t::MUTED);
                                }
                            });
                        });
                        let xfers = hstat.as_ref().map(|s| s.transfers.clone()).unwrap_or_default();
                        t::section(ui, &tr("Transfers"));
                        t::card().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            if xfers.is_empty() {
                                ui.label(RichText::new(tr("Nothing in progress")).color(t::MUTED));
                            }
                            for (guest, p) in &xfers {
                                ui.horizontal(|ui| {
                                    ui.label(crate::rtl::show(&format!("{} {} {} {}", if p.sending { "Sending" } else { "Receiving" }, p.name, if p.sending { "to" } else { "from" }, guest)));
                                    ui.add(egui::ProgressBar::new(p.done as f32 / p.size.max(1) as f32).desired_width(140.0).show_percentage());
                                    if ui.small_button(tr("Cancel")).clicked() {
                                        if let Some(h) = &self.host {
                                            h.cancel_file(guest, p.id);
                                        }
                                    }
                                });
                            }
                        });
                        t::section(ui, &tr("Captures"));
                        ui.horizontal(|ui| {
                            if ui.button(tr("Screenshots folder")).on_hover_text(tr("Pictures\\Kinlink")).clicked() {
                                open_folder(crate::rec::user_dir("Pictures"));
                            }
                            if ui.button(tr("Recordings folder")).on_hover_text(tr("Videos\\Kinlink")).clicked() {
                                open_folder(crate::rec::user_dir("Videos"));
                            }
                        });
                    }

                    // ---------------------------------------- Settings
                    Page::Settings => {
                        t::title(ui, &tr("Settings"), None);
                        ui.columns(2, |c| {
                            let (left, right) = c.split_at_mut(1);
                            let (l, r) = (&mut left[0], &mut right[0]);
                            t::card().show(l, |ui| {
                                ui.set_width(ui.available_width());
                                t::section(ui, &tr("Account"));
                                ui.horizontal(|ui| {
                                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                                    let rr = ui.add(egui::TextEdit::singleline(&mut self.rename_text).hint_text(tr("new username")).desired_width(150.0).margin(egui::vec2(10.0, 7.0)));
                                    if ui.button(tr("Change")).clicked() || ((rr.has_focus() || rr.lost_focus()) && enter) {
                                        rename_now = true;
                                    }
                                });
                                if let Some(e) = &self.rename_err {
                                    ui.colored_label(t::DANGER, crate::rtl::show(e));
                                }
                                para(ui, &tl("Letters and numbers only, once every 3 months. Your #tag changes too; friends keep you."), 11.5, t::MUTED);
                            });
                            t::card().show(r, |ui| {
                                ui.set_width(ui.available_width());
                                t::section(ui, &tr("General"));
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(tr("Language")).color(t::SOFT));
                                    let ar = crate::i18n::arabic();
                                    if ui.selectable_label(!ar, "English").clicked() && ar {
                                        crate::i18n::set_arabic(false);
                                        self.s.language = "en".into();
                                        self.s.save();
                                    }
                                    if ui.selectable_label(ar, crate::rtl::show("العربية")).clicked() && !ar {
                                        crate::i18n::set_arabic(true);
                                        self.s.language = "ar".into();
                                        self.s.save();
                                    }
                                });
                                if t::switch(ui, &mut self.autostart, &tr("Start with Windows (in tray)")) {
                                    settings::set_autostart(self.autostart);
                                }
                                if t::switch(ui, &mut self.s.notify_online, &tr("Notify when friends come online")) {
                                    self.s.save();
                                }
                                if t::switch(ui, &mut self.s.fullscreen_connect, &tr("Fullscreen when connecting")) {
                                    self.s.save();
                                }
                                if t::switch(ui, &mut self.s.ask_files, &tr("Ask before receiving files")) {
                                    crate::files::ASK.store(self.s.ask_files, std::sync::atomic::Ordering::Relaxed);
                                    self.s.save();
                                }
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(tr("Session shortcut keys")).color(t::SOFT));
                                    let cur = HOTKEY_MODS.iter().find(|m| m.0 == self.s.hotkey_mods).map(|m| m.1).unwrap_or("Ctrl+Alt+Shift");
                                    egui::ComboBox::from_id_salt("hkmods").selected_text(cur).show_ui(ui, |ui| {
                                        for (bits, name) in HOTKEY_MODS {
                                            if ui.selectable_value(&mut self.s.hotkey_mods, *bits, *name).changed() {
                                                self.s.save();
                                            }
                                        }
                                    });
                                    ui.label(RichText::new(tr("+ Q, O, R, F, C, P, V, M")).size(11.5).color(t::MUTED));
                                });
                            });
                            t::card().show(l, |ui| {
                                ui.set_width(ui.available_width());
                                t::section(ui, &tr("Hosting quality"));
                                restart_host |= t::switch(ui, &mut self.s.hardware, &tr("Hardware encoding (GPU)"));
                                restart_host |= t::switch(ui, &mut self.s.record_sessions, &tr("Record sessions on this PC (Videos\\Kinlink)"));
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(tr("Max fps")).color(t::SOFT));
                                    egui::ComboBox::from_id_salt("fps").selected_text(format!("{}", self.s.fps)).show_ui(ui, |ui| {
                                        for f in [30, 60, 120, 144] {
                                            restart_host |= ui.selectable_value(&mut self.s.fps, f, format!("{f}")).changed();
                                        }
                                    });
                                });
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(tr("Max bitrate")).color(t::SOFT));
                                    restart_host |= ui.add(egui::Slider::new(&mut self.s.max_mbps, 3..=100).suffix(" Mbps")).drag_stopped();
                                });
                            });
                            t::card().show(l, |ui| {
                                ui.set_width(ui.available_width());
                                t::section(ui, &tr("Controller buttons"));
                                ui.label(RichText::new(tr("Make a button on your controller act as another one (for games you play through Kinlink).")).size(12.0).color(t::MUTED));
                                let mut map = if self.s.pad_map.len() == 16 { self.s.pad_map.clone() } else { crate::client::IDENTITY_MAP.to_vec() };
                                let mut changed = false;
                                egui::CollapsingHeader::new(tr("Remap buttons")).show(ui, |ui| {
                                    egui::Grid::new("padmap").num_columns(4).spacing([10.0, 6.0]).show(ui, |ui| {
                                        for (n, (i, name)) in PAD_BUTTONS.iter().enumerate() {
                                            ui.label(RichText::new(*name).color(t::SOFT));
                                            let cur = PAD_BUTTONS.iter().find(|(b, _)| *b == map[*i as usize]).map(|b| b.1).unwrap_or("?");
                                            egui::ComboBox::from_id_salt(("pm", *i)).width(70.0).selected_text(cur).show_ui(ui, |ui| {
                                                for (b, bname) in PAD_BUTTONS {
                                                    changed |= ui.selectable_value(&mut map[*i as usize], *b, *bname).changed();
                                                }
                                            });
                                            if n % 2 == 1 {
                                                ui.end_row();
                                            }
                                        }
                                    });
                                    if ui.button(tr("Reset")).clicked() {
                                        map = crate::client::IDENTITY_MAP.to_vec();
                                        changed = true;
                                    }
                                });
                                if changed {
                                    self.s.pad_map = if map == crate::client::IDENTITY_MAP { Vec::new() } else { map };
                                    set_pad_map(&self.s.pad_map);
                                    self.s.save();
                                }
                            });
                            t::card().show(r, |ui| {
                                ui.set_width(ui.available_width());
                                t::section(ui, &tr("Network"));
                                restart_host |= t::switch(ui, &mut self.s.upnp, &tr("Open router port (UPnP)"));
                                ui.horizontal_wrapped(|ui| {
                                    if t::primary(ui, &tr("Run network test"), None, self.nettest.is_none()).on_hover_text(tr("Checks your router, firewall and speed (uses Cloudflare's speed test)")).clicked() {
                                        let slot = Arc::new(Mutex::new(None));
                                        let (s2, online, wake) = (slot.clone(), sig.online(), self.waker());
                                        std::thread::spawn(move || {
                                            *s2.lock().unwrap() = Some(crate::nettest::run(online));
                                            wake();
                                        });
                                        self.nettest = Some(slot);
                                    }
                                    if ui.button(tr("Allow through firewall")).on_hover_text(tr("One-time; asks for admin permission")).clicked() {
                                        allow_firewall();
                                    }
                                });
                            });
                            t::card().show(l, |ui| {
                                ui.set_width(ui.available_width());
                                t::section(ui, &tr("Lock screen & admin prompts"));
                                let (installed, running) = crate::syshelper::service_state();
                                if installed && crate::syshelper::helper_is_stale() {
                                    para(ui, &tl("The helper belongs to another copy of Kinlink (an old version or a moved file), so it won't work with this one."), 12.0, t::DANGER);
                                    ui.horizontal(|ui| {
                                        if t::primary(ui, &tr("Update helper (asks for admin)"), None, true).clicked() {
                                            crate::syshelper::run_elevated("--install-service");
                                        }
                                    });
                                } else if installed {
                                    ui.colored_label(if running { t::MINT_SOFT } else { t::DANGER }, if running { tr("Guests can control them") } else { tr("Helper installed but not running") });
                                    ui.horizontal(|ui| {
                                        if ui.button(tr("Turn off")).on_hover_text(tr("Removes the helper service (asks for admin)")).clicked() {
                                            crate::syshelper::run_elevated("--uninstall-service");
                                        }
                                    });
                                } else {
                                    para(ui, &tl("Lets guests see and use the Windows lock screen, UAC prompts and admin apps."), 12.0, t::SOFT);
                                    ui.horizontal(|ui| {
                                        if ui.button(tr("Enable (asks for admin)")).clicked() {
                                            crate::syshelper::run_elevated("--install-service");
                                        }
                                    });
                                }
                            });
                            t::card().show(r, |ui| {
                                ui.set_width(ui.available_width());
                                t::section(ui, &tr("Help"));
                                ui.horizontal(|ui| {
                                    if ui.button(tr("Open log")).on_hover_text(tr("Everything the app did this run (connections, errors)")).clicked() {
                                        if let Some(p) = crate::applog::path() {
                                            let _ = std::process::Command::new("notepad").arg(p).spawn();
                                        }
                                    }
                                });
                                ui.horizontal(|ui| {
                                    if ui.button(tr("Terms of Use")).clicked() {
                                        open_url("https://github.com/IamAbood/Kinlink/blob/main/TERMS.md");
                                    }
                                    if ui.button(tr("Privacy Policy")).clicked() {
                                        open_url("https://github.com/IamAbood/Kinlink/blob/main/PRIVACY.md");
                                    }
                                });
                                para(ui, &tl("Only connect to PCs you own or have permission to use."), 11.5, t::SOFT);
                                ui.label(RichText::new(trf("Kinlink {} · made by Abdulkareem Alsaou", &[&crate::updater::current()])).size(11.5).color(t::MUTED));
                            });
                        });
                    }
                }
            });
        });

        if let Some(p) = go {
            self.page = p;
        }
        if pick {
            self.pick_and_send();
        }
        if let Some(f) = connect {
            self.connect_to(&f);
        }
        if let Some(on) = host_toggle {
            self.s.hosting = on;
            self.s.save();
            if on {
                self.start_host();
            } else {
                self.host = None;
            }
        }
        if restart_host {
            self.restart_host_if_idle();
        }
        if panic_clicked {
            self.panic();
        }
        if rename_now {
            self.rename();
        }
        if let (Some((i, up)), Some(h)) = (move_pad, &self.host) {
            h.move_pad(i, up);
        }
        self.nettest_ui(root.ctx());
        self.friend_chat_ui(root.ctx(), &sig);
    }

    fn friend_rows(&mut self, ui: &mut egui::Ui, sig: &Signal, friends: Vec<(Friend, bool)>, connect: &mut Option<Friend>) {
        use crate::theme::{self as t, Icon};
        let mut shortcut_for: Option<Friend> = None;
        let n = friends.len();
        for (k, (f, online)) in friends.into_iter().enumerate() {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                t::avatar(ui, &f.display(), Some(online), 42.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(crate::rtl::show(&f.display())).font(egui::FontId::new(15.0, t::bold())).color(if online { t::TEXT } else { t::SOFT }));
                        if !f.nickname.trim().is_empty() {
                            ui.label(RichText::new(f.id()).size(12.5).color(t::MUTED));
                        }
                    });
                    let status = sig.status_of(&f.public);
                    let line = if !online { tr("Offline") } else if status.is_empty() { tr("Online") } else { trf("Online · {}", &[&crate::i18n::tl(&status)]) };
                    ui.label(RichText::new(line).size(12.0).color(if online { t::MINT_SOFT } else { t::MUTED }));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let open = self.expanded == Some(f.public);
                    if t::icon_button(ui, Icon::More, &tr("Permissions, nickname, remove"), open).clicked() {
                        self.expanded = if open { None } else { Some(f.public) };
                    }
                    let unread = sig.unread(&f.public);
                    let r = t::icon_button(ui, Icon::Chat, &tr("Message"), self.chat_with == Some(f.public));
                    if unread > 0 {
                        let c = egui::pos2(r.rect.max.x - 3.0, r.rect.min.y + 3.0);
                        ui.painter().circle_filled(c, 8.0, t::ACCENT);
                        ui.painter().text(c, egui::Align2::CENTER_CENTER, unread.min(99).to_string(), egui::FontId::new(10.0, t::bold()), Color32::WHITE);
                    }
                    if r.clicked() {
                        self.chat_with = if self.chat_with == Some(f.public) { None } else { Some(f.public) };
                    }
                    let busy = self.client.is_some() || self.outgoing.is_some();
                    if online {
                        if t::primary(ui, &tr("Connect"), Some(Icon::Play), !busy).clicked() {
                            *connect = Some(f.clone());
                        }
                    }
                });
            });
            if self.expanded == Some(f.public) {
                let mut g = f.clone();
                let mut ask = false;
                egui::Frame::NONE.fill(t::BG).stroke(egui::Stroke::new(1.0, t::LINE)).corner_radius(12.0).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        if ui.add_enabled(online, egui::Button::new(tr("Ask them for help"))).on_hover_text(tr("They get a notification; when they connect within 15 minutes they get full control without asking.")).clicked() {
                            ask = true;
                        }
                    });
                    ui.columns(2, |c| {
                        let ui = &mut c[0];
                        ui.label(RichText::new(tr("Nickname")).size(12.0).color(t::SOFT));
                        ui.add(egui::TextEdit::singleline(&mut g.nickname).hint_text(tr("e.g. Ahmad's gaming PC")).margin(egui::vec2(10.0, 7.0)));
                        ui.label(RichText::new(tr("Group")).size(12.0).color(t::SOFT));
                        ui.add(egui::TextEdit::singleline(&mut g.group).hint_text(tr("e.g. Home")).margin(egui::vec2(10.0, 7.0)));
                        ui.horizontal(|ui| {
                            if ui.button(tr("Desktop shortcut")).on_hover_text(tr("One click on the desktop connects to this friend")).clicked() {
                                shortcut_for = Some(f.clone());
                            }
                            if ui.button(RichText::new(tr("Remove")).color(t::DANGER)).clicked() {
                                sig.remove(f.public);
                                self.expanded = None;
                            }
                        });
                        let ui = &mut c[1];
                        ui.label(RichText::new(tr("When they connect to this PC")).size(12.0).color(t::SOFT));
                        t::switch(ui, &mut g.allow_mouse, &tr("Mouse"));
                        t::switch(ui, &mut g.allow_keyboard, &tr("Keyboard"));
                        t::switch(ui, &mut g.allow_gamepad, &tr("Controller"));
                        t::switch(ui, &mut g.auto_connect, &tr("Connect without asking"));
                        ui.label(RichText::new(tr("All three off = spectator (view only)")).size(11.0).color(t::MUTED));
                        ui.label(RichText::new(trf("Safety code: {}", &[&sig.safety_code(&f.public)])).size(11.5).color(t::SOFT))
                            .on_hover_text(tr("Same code on both screens = you're really talking to each other"));
                    });
                });
                if ask {
                    if self.host.is_none() {
                        self.toast(tl("Turn on \"Let friends connect\" on This PC first."));
                    } else {
                        sig.ask_help(&f.public);
                        self.help_asked.retain(|(p, _)| *p != f.public);
                        self.help_asked.push((f.public, Instant::now()));
                        self.toast(tlf("Asked {} for help. When they connect, they get full control.", &[&f.display()]));
                    }
                }
                if g != f {
                    if let Some(h) = &self.host {
                        h.set_perms(&g.id(), Perms { mouse: g.allow_mouse, keyboard: g.allow_keyboard, gamepad: g.allow_gamepad });
                    }
                    sig.update(g);
                }
            }
            ui.add_space(4.0);
            if k + 1 < n {
                let r = ui.available_rect_before_wrap();
                ui.painter().hline(r.x_range(), r.top(), egui::Stroke::new(1.0, t::LINE));
                ui.add_space(1.0);
            }
        }
        if let Some(f) = shortcut_for {
            let made = crate::shortcut::desktop().ok_or_else(|| "no desktop folder".to_string()).and_then(|d| {
                let name = crate::shortcut::safe_name(&f.display());
                crate::shortcut::create(&d.join(format!("Connect to {name}.lnk")), &format!("--connect-friend {}", f.id()), &format!("Connect to {} with Kinlink", f.id()), None)
            });
            self.toast(match made {
                Ok(()) => tlf("Shortcut \"Connect to {}\" added to your desktop", &[&f.display()]),
                Err(e) => tlf("Couldn't create the shortcut: {}", &[&e]),
            });
        }
    }

    fn nettest_ui(&mut self, ctx: &egui::Context) {
        let Some(slot) = self.nettest.clone() else { return };
        let mut open = true;
        let mut apply: Option<u32> = None;
        egui::Window::new(RichText::new(tr("Network test")).strong()).collapsible(false).resizable(false).open(&mut open).show(ctx, |ui| {
            let r = slot.lock().unwrap();
            let Some(r) = r.as_ref() else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(tr("Testing... (about 15 seconds)"));
                });
                return;
            };
            for l in &r.lines {
                ui.horizontal(|ui| {
                    dot(ui, if l.ok { GREEN } else { Color32::from_rgb(230, 180, 60) });
                    ui.label(&l.text);
                });
            }
            if let Some(m) = r.suggest_mbps {
                ui.separator();
                para(ui, &tlf("Suggested max bitrate when friends connect to you: {} Mbps (current {} Mbps)", &[&m.to_string(), &self.s.max_mbps.to_string()]), 14.0, crate::theme::TEXT);
                ui.small(tr("If several friends join at once, they share your upload: lower it further."));
                if ui.button(trf("Use {} Mbps", &[&m.to_string()])).clicked() {
                    apply = Some(m);
                }
            }
        });
        if let Some(m) = apply {
            self.s.max_mbps = m;
            self.restart_host_if_idle();
            self.toast(tlf("Max bitrate set to {} Mbps", &[&m.to_string()]));
        }
        if !open {
            self.nettest = None;
        }
    }
}

/// Status dot (drawn, since the bundled font has no circle glyph).
fn dot(ui: &mut egui::Ui, c: Color32) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(r.center(), 4.5, c);
}
use dot as dot_fn;

/// Adds an inbound-allow rule for this exe (all ports/protocols it listens on). Prompts for admin.
fn allow_firewall() {
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let Ok(exe) = std::env::current_exe() else { return };
    let args = format!("advfirewall firewall add rule name=\"Kinlink\" dir=in action=allow program=\"{}\" enable=yes profile=any", exe.display());
    unsafe {
        ShellExecuteW(None, &HSTRING::from("runas"), &HSTRING::from("netsh"), &HSTRING::from(args), None, SW_HIDE);
    }
}

fn build_tray(wake: impl Fn() + Send + Sync + 'static) -> Option<(tray_icon::TrayIcon, Receiver<TrayMsg>)> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{MouseButton as TMB, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    let menu = Menu::new();
    let open = MenuItem::new("Open Kinlink", true, None);
    let panic = MenuItem::new("Disconnect everyone", true, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append_items(&[&open, &panic, &quit]).ok()?;
    let icon = tray_icon::Icon::from_resource(1, Some((32, 32))).ok()?;
    let tray = TrayIconBuilder::new().with_menu(Box::new(menu)).with_menu_on_left_click(false).with_tooltip("Kinlink").with_icon(icon).build().ok()?;
    let (tx, rx) = channel();
    let wake = Arc::new(wake);
    let (tx2, w2, open_id, quit_id, panic_id) = (tx.clone(), wake.clone(), open.id().clone(), quit.id().clone(), panic.id().clone());
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let m = if e.id == quit_id {
            TrayMsg::Quit
        } else if e.id == open_id {
            TrayMsg::Open
        } else if e.id == panic_id {
            TrayMsg::Panic
        } else {
            return;
        };
        let _ = tx2.send(m);
        w2();
    }));
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: TMB::Left, button_state: MouseButtonState::Up, .. } | TrayIconEvent::DoubleClick { button: TMB::Left, .. } = e {
            let _ = tx.send(TrayMsg::Open);
            wake();
        }
    }));
    Some((tray, rx))
}

impl ApplicationHandler<()> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(format!("Kinlink {}", crate::updater::current()))
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 640.0))
            .with_visible(!self.start_minimized);
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        crate::plog!("startup: window created");
        let gfx = match Gfx::new(window.clone()) {
            Ok(g) => g,
            Err(e) => {
                crate::plog!("graphics init failed: {e:#}");
                let msg = format!("Kinlink couldn't start its graphics:

{e:#}

Updating the graphics driver usually fixes this.");
                unsafe {
                    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
                    MessageBoxW(None, &windows::core::HSTRING::from(msg), windows::core::w!("Kinlink"), MB_OK | MB_ICONERROR);
                }
                el.exit();
                return;
            }
        };
        self.egui_state = Some(egui_winit::State::new(self.egui_ctx.clone(), egui::ViewportId::ROOT, &window, Some(window.scale_factor() as f32), None, None));
        crate::theme::apply(&self.egui_ctx);
        crate::i18n::set_arabic(match self.s.language.as_str() {
            "ar" => true,
            "en" => false,
            _ => crate::i18n::system_is_arabic(),
        });
        if let Ok(h) = window.window_handle() {
            if let RawWindowHandle::Win32(w) = h.as_raw() {
                let wake = self.waker();
                self.kb = Some(kbhook::install(windows::Win32::Foundation::HWND(w.hwnd.get() as *mut _), wake));
            }
        }
        self.gfx = Some(gfx);
        crate::plog!("startup: graphics ok, starting tray, hotkeys and friends");
        listen_for_show(self.waker());
        crate::notify::register();
        settings::register_url_scheme();
        // Start with Windows: keep the entry pointing at this copy (it may have moved or updated).
        if self.autostart && settings::profile().is_none() {
            settings::refresh_autostart();
        }
        crate::hotkey::install(self.waker());
        match build_tray(self.waker()) {
            Some((t, rx)) => {
                self.tray = Some(t);
                self.tray_rx = Some(rx);
            }
            None => crate::plog!("tray icon unavailable"),
        }
        if let Some(id) = Identity::load() {
            crate::plog!("startup: signed in as {}", id.id());
            self.start_services(id);
        } else {
            crate::plog!("startup: first run, showing setup");
            self.show_window(); // first run needs the setup screen
        }
        crate::plog!("startup: done");
        window.request_redraw();
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match &ev {
            WindowEvent::CloseRequested => {
                // Keep running in the tray, like Parsec.
                if self.streaming {
                    self.leave_stream(None);
                }
                if let Some(w) = self.window() {
                    w.set_visible(false);
                }
                return;
            }
            WindowEvent::Resized(s) => {
                if let Some(g) = &mut self.gfx {
                    g.resize(s.width, s.height);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = Some((position.x as f32, position.y as f32));
                if self.streaming {
                    if let Some(w) = self.window() {
                        w.request_redraw(); // keep the drawn cursor glued to the mouse
                    }
                }
            }
            WindowEvent::CursorLeft { .. } => self.pointer = None,
            WindowEvent::DroppedFile(path) => self.send_path(path),
            WindowEvent::RedrawRequested => {
                self.draw();
                return;
            }
            _ => {}
        }
        if let (Some(st), Some(w)) = (&mut self.egui_state, self.gfx.as_ref().map(|g| g.window.clone())) {
            if st.on_window_event(&w, &ev).repaint {
                w.request_redraw();
            }
        }
        if self.streaming {
            self.stream_input(&ev);
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, ev: DeviceEvent) {
        if let (true, true, DeviceEvent::MouseMotion { delta }) = (self.streaming, self.relative, ev) {
            if self.window().is_some_and(|w| w.has_focus()) {
                if let Some(c) = &self.client {
                    c.input(Input::MouseRel { dx: delta.0 as i32, dy: delta.1 as i32 });
                }
            }
        }
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _ev: ()) {
        if let Some(w) = self.window() {
            w.request_redraw();
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.track_sessions();
        let picked = self.picked.lock().unwrap().take();
        if let Some(p) = picked {
            self.send_path(&p);
        }
        if self.streaming && self.os_cursor && !self.over_overlay {
            if let Some(s) = self.os_shape.clone() {
                if self.os_applied.as_ref().is_none_or(|(v, _)| *v != s.version) {
                    let (w, h) = (s.w.max(1) as u16, s.h.max(1) as u16);
                    let src = winit::window::CustomCursor::from_rgba(s.rgba.clone(), w, h, (s.hx as u16).min(w - 1), (s.hy as u16).min(h - 1));
                    self.os_applied = src.ok().map(|src| (s.version, el.create_custom_cursor(src)));
                    self.os_dirty = true;
                }
                if self.os_dirty {
                    if let (Some((_, cur)), Some(w)) = (&self.os_applied, self.window()) {
                        w.set_cursor(cur.clone());
                    }
                    self.os_dirty = false;
                }
            }
        }
        let tray: Vec<TrayMsg> = self.tray_rx.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default();
        for m in tray {
            match m {
                TrayMsg::Open => self.show_window(),
                TrayMsg::Panic => self.panic(),
                TrayMsg::Quit => {
                    self.leave_stream(None);
                    for p in std::mem::take(&mut self.popups) {
                        self.decline_incoming(&p, "closed the app");
                    }
                    if let Some(s) = &self.signal {
                        s.shutdown();
                    }
                    self.host = None;
                    el.exit();
                    return;
                }
            }
        }
        if matches!(*self.update.lock().unwrap(), Update::Done) {
            // The new version is starting and waits for us to exit.
            self.leave_stream(None);
            if let Some(s) = &self.signal {
                s.shutdown();
            }
            self.host = None;
            el.exit();
            return;
        }
        if crate::hotkey::take() {
            self.panic();
        }
        let finished = self.host.as_ref().map(|h| h.take_finished()).unwrap_or_default();
        for (guest, p) in finished {
            if let Some(msg) = self.on_clip_transfer(&p, &guest) {
                if !msg.is_empty() {
                    self.toast(msg);
                }
                continue;
            }
            if let (None, false) = (&p.failed, p.sending) {
                self.toast(tlf("Received {} from {} (Downloads\\Kinlink)", &[&p.name, &guest]));
                crate::notify::toast(&tlf("{} sent you {}", &[&guest, &p.name]), &tl("Saved in Downloads\\Kinlink"));
            }
        }
        if SHOW_REQUEST.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.show_window();
            if let Some(p) = settings::connect_request_path() {
                if let Ok(id) = std::fs::read_to_string(&p) {
                    let _ = std::fs::remove_file(&p);
                    self.connect_on_start = Some((id.trim().to_string(), Instant::now()));
                }
            }
        }
        let keys: Vec<KeyEv> = self.kb.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default();
        for k in keys {
            self.on_key(k);
        }
        // --connect-friend: connect as soon as that friend shows up online.
        if let Some((link, _)) = self.connect_on_start.clone().filter(|(l, _)| l.starts_with("kinlink://")) {
            if self.signal.is_some() {
                self.connect_on_start = None;
                self.start_join(&link);
            }
        }
        if self.pending_join.as_ref().is_some_and(|(_, _, at)| at.elapsed() > Duration::from_secs(40)) {
            let (id, _, _) = self.pending_join.take().unwrap();
            self.toast(tlf("Couldn't find {}. Is that PC on and running Kinlink?", &[&id]));
        }
        if let (Some((id, since)), Some(sig)) = (self.connect_on_start.clone(), self.signal.clone()) {
            match sig.friends().into_iter().find(|(f, _)| f.id().eq_ignore_ascii_case(&id)) {
                Some((f, true)) => {
                    self.connect_on_start = None;
                    self.connect_to(&f);
                }
                None if since.elapsed() > Duration::from_secs(5) => {
                    self.connect_on_start = None;
                    self.toast(tlf("{} isn't in your friends list", &[&id]));
                }
                Some((f, false)) if since.elapsed() > Duration::from_secs(60) => {
                    self.connect_on_start = None;
                    self.toast(tlf("{} isn't online right now", &[&f.display()]));
                }
                _ => {}
            }
        }
        let events = self.signal.as_ref().map(|s| s.take_events()).unwrap_or_default();
        for e in events {
            self.on_signal_event(e);
        }
        if let Some(c) = &self.client {
            match c.state() {
                State::Connected { .. } if !self.streaming => {
                    self.enter_stream();
                    self.apply_quality();
                }
                State::Failed(why) => self.leave_stream(Some(why)),
                _ => {}
            }
        }
        if self.outgoing.as_ref().is_some_and(|o| o.at.elapsed() > Duration::from_secs(45)) {
            let name = self.outgoing.as_ref().unwrap().name.clone();
            self.cancel_outgoing();
            self.toast(tlf("No answer from {}", &[&name]));
        }
        let expired: Vec<IncomingReq> = {
            let (old, keep) = std::mem::take(&mut self.popups).into_iter().partition(|p| p.at.elapsed() > Duration::from_secs(30));
            self.popups = keep;
            old
        };
        for p in expired {
            self.decline_incoming(&p, "didn't answer");
        }
        if self.message.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(8)) {
            self.message = None;
        }
        // Home screen refreshes once a second (online dots, host status, countdowns).
        let visible = self.window().is_some_and(|w| w.is_visible().unwrap_or(true));
        if !self.streaming && visible && self.last_refresh.elapsed() > Duration::from_secs(1) {
            self.last_refresh = Instant::now();
            if let Some(w) = self.window() {
                w.request_redraw();
            }
        }
        let mut wake_at = Instant::now() + Duration::from_secs(1);
        if let Some(t) = self.repaint_at {
            if t <= Instant::now() {
                self.repaint_at = None;
                if let Some(w) = self.window() {
                    w.request_redraw();
                }
            } else {
                wake_at = wake_at.min(t);
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(wake_at));
    }
}

impl App {
    fn pick_and_send(&self) {
        let (slot, wake) = (self.picked.clone(), self.waker());
        crate::shortcut::pick_file(move |p| {
            *slot.lock().unwrap() = p;
            wake();
        });
    }

    /// Sends a dropped or picked file: to the host while streaming, else to our guests.
    fn send_path(&mut self, path: &std::path::Path) {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if let (true, Some(c)) = (self.streaming, &self.client) {
            let r = c.send_file(path);
            self.bubble = Some((match r {
                Ok(()) => tlf("Sending {} to {}", &[&name, &self.peer]),
                Err(e) => tlf("Can't send {}: {}", &[&name, &e.to_string()]),
            }, Instant::now()));
        } else if let Some(h) = &self.host {
            match h.send_file(path) {
                Ok(0) => self.toast(tl("Nobody is connected to receive files")),
                Ok(n) => self.toast(tlf("Sending {} to {} guest(s)", &[&name, &n.to_string()])),
                Err(e) => self.toast(tlf("Can't send {}: {}", &[&name, &e.to_string()])),
            }
        } else {
            self.toast(tl("Connect to a friend (or have one connect to you) to send files"));
        }
        if let Some(w) = self.window() {
            w.request_redraw();
        }
    }
}

/// Last few log lines, so you can see where a connection is stuck.
fn log_tail(ui: &mut egui::Ui) {
    for l in crate::applog::recent(6) {
        ui.label(RichText::new(l).small().monospace().color(Color32::GRAY));
    }
}

/// Opens a folder in Explorer (creating it first).
fn open_folder(dir: Option<std::path::PathBuf>) {
    if let Some(d) = dir {
        let _ = std::fs::create_dir_all(&d);
        let _ = std::process::Command::new("explorer").arg(d).spawn();
    }
}

/// Opens a web page in the default browser.
fn open_url(url: &str) {
    let _ = std::process::Command::new("explorer").arg(url).spawn();
}


// ---------------------------------------------------------------- session history

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct HistEntry {
    guest: String,
    start: String,
    secs: u64,
}

fn history_path() -> Option<std::path::PathBuf> {
    Some(settings::data_dir()?.join("history.json"))
}

fn load_history() -> Vec<HistEntry> {
    history_path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_history(h: &[HistEntry]) {
    if let Some(p) = history_path() {
        let _ = std::fs::write(p, serde_json::to_string(h).unwrap_or_default());
    }
}

/// "21 Nov 16:43" in local time.
fn local_now() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{} {} {:02}:{:02}", t.wDay, M[(t.wMonth as usize).clamp(1, 12) - 1], t.wHour, t.wMinute)
}

fn duration_text(secs: u64) -> String {
    if secs < 60 {
        trf("{} s", &[&secs.to_string()])
    } else if secs < 3600 {
        trf("{} min", &[&(secs / 60).to_string()])
    } else {
        trf("{} h {} min", &[&(secs / 3600).to_string(), &(secs % 3600 / 60).to_string()])
    }
}

fn human_size(b: u64) -> String {
    match b {
        0..1_000_000 => format!("{:.0} KB", b as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1} MB", b as f64 / 1e6),
        _ => format!("{:.2} GB", b as f64 / 1e9),
    }
}

impl App {
    /// Notices guests joining and leaving (for "Recent sessions").
    fn track_sessions(&mut self) {
        let now: Vec<String> = self.host.as_ref().map(|h| h.status().clients).unwrap_or_default();
        for g in &now {
            if !self.live_sessions.contains_key(g) {
                self.live_sessions.insert(g.clone(), (local_now(), Instant::now()));
            }
        }
        let gone: Vec<String> = self.live_sessions.keys().filter(|g| !now.contains(g)).cloned().collect();
        for g in gone {
            if let Some((start, at)) = self.live_sessions.remove(&g) {
                self.history.push(HistEntry { guest: g, start, secs: at.elapsed().as_secs() });
                if self.history.len() > 200 {
                    self.history.remove(0);
                }
                save_history(&self.history);
            }
        }
    }

    /// "X wants to send you a file" cards (as host and during a session).
    fn file_offers_ui(&mut self, ctx: &egui::Context) {
        use crate::theme as t;
        let mut offers: Vec<(Option<String>, u32, String, u64)> = Vec::new();
        if let Some(h) = &self.host {
            offers.extend(h.pending_files().into_iter().map(|(g, id, n, sz)| (Some(g), id, n, sz)));
        }
        if let Some(c) = &self.client {
            offers.extend(c.pending_files().into_iter().map(|(id, n, sz)| (None, id, n, sz)));
        }
        if offers.is_empty() {
            return;
        }
        let mut answer: Option<(Option<String>, u32, bool)> = None;
        let base = self.popups.len() as f32 * 170.0;
        for (i, (from, id, name, size)) in offers.iter().enumerate() {
            let who = from.clone().unwrap_or_else(|| self.peer.clone());
            egui::Window::new("file offer")
                .id(egui::Id::new(("offer", from.clone(), *id)))
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .frame(egui::Frame::NONE.fill(Color32::from_rgb(0x16, 0x1F, 0x3A)).stroke(egui::Stroke::new(1.0, t::ACCENT_LINE)).corner_radius(16.0).inner_margin(egui::Margin::same(16)))
                .anchor(egui::Align2::RIGHT_BOTTOM, [-24.0, -24.0 - base - i as f32 * 150.0])
                .show(ctx, |ui| {
                    ui.set_width(300.0);
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
                        t::icon(ui.painter(), r, t::Icon::Files, t::ACCENT_SOFT);
                        ui.label(RichText::new(trf("{} wants to send you a file", &[&who])).font(egui::FontId::new(14.0, t::bold())));
                    });
                    ui.label(RichText::new(format!("{} · {}", crate::rtl::show(name), human_size(*size))).color(t::SOFT));
                    ui.label(RichText::new(tr("Saved to Downloads\\Kinlink if you accept")).size(11.5).color(t::MUTED));
                    ui.horizontal(|ui| {
                        if t::primary(ui, &tr("Accept"), None, true).clicked() {
                            answer = Some((from.clone(), *id, true));
                        }
                        if ui.add(egui::Button::new(tr("Decline")).min_size(egui::vec2(100.0, 36.0))).clicked() {
                            answer = Some((from.clone(), *id, false));
                        }
                    });
                });
        }
        if let Some((from, id, yes)) = answer {
            match from {
                Some(g) => {
                    if let Some(h) = &self.host {
                        h.answer_file(&g, id, yes);
                    }
                }
                None => {
                    if let Some(c) = &self.client {
                        c.answer_file(id, yes);
                    }
                }
            }
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

impl App {
    /// Chat with one friend, outside any session.
    fn friend_chat_ui(&mut self, ctx: &egui::Context, sig: &Signal) {
        use crate::theme as t;
        let Some(with) = self.chat_with else { return };
        let Some(f) = sig.friend(&with) else {
            self.chat_with = None;
            return;
        };
        sig.mark_read(&with);
        let msgs = sig.messages(&with);
        let online = sig.friends().iter().any(|(x, on)| x.public == with && *on);
        let mut close = false;
        let mut send = false;
        egui::Window::new("friend chat")
            .id(egui::Id::new("friend_chat"))
            .title_bar(false)
            .resizable(false)
            .frame(egui::Frame::NONE.fill(t::SIDEBAR).stroke(egui::Stroke::new(1.0, t::ACCENT_LINE)).corner_radius(16.0).inner_margin(egui::Margin::same(14)).shadow(egui::Shadow { offset: [0, 14], blur: 40, spread: 0, color: Color32::from_black_alpha(140) }))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-20.0, -44.0])
            .show(ctx, |ui| {
                ui.set_width(330.0);
                ui.horizontal(|ui| {
                    t::avatar(ui, &f.display(), Some(online), 34.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new(crate::rtl::show(&f.display())).font(egui::FontId::new(14.5, t::bold())));
                        ui.label(RichText::new(if online { tr("Online") } else { tr("Offline · gets it when back online") }).size(11.5).color(if online { t::MINT_SOFT } else { t::MUTED }));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if t::icon_button(ui, t::Icon::Close, &tr("Close"), false).clicked() {
                            close = true;
                        }
                    });
                });
                ui.separator();
                egui::ScrollArea::vertical().id_salt("fchat").max_height(300.0).min_scrolled_height(300.0).stick_to_bottom(true).show(ui, |ui| {
                    ui.set_width(318.0);
                    if msgs.is_empty() {
                        ui.add_space(20.0);
                        ui.vertical_centered(|ui| ui.label(RichText::new(tr("No messages yet. Say hi!")).color(t::MUTED)));
                    }
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                    for m in msgs.iter().rev().take(200).rev() {
                        let layout = if m.mine { egui::Layout::right_to_left(egui::Align::Min) } else { egui::Layout::left_to_right(egui::Align::Min) };
                        ui.with_layout(layout, |ui| {
                            egui::Frame::NONE
                                .fill(if m.mine { t::ACCENT } else { t::HOVER })
                                .corner_radius(if m.mine { egui::CornerRadius { nw: 12, ne: 12, sw: 12, se: 4 } } else { egui::CornerRadius { nw: 12, ne: 12, sw: 4, se: 12 } })
                                .inner_margin(egui::Margin::symmetric(10, 6))
                                .show(ui, |ui| {
                                    ui.set_max_width(240.0);
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        ui.label(RichText::new(crate::rtl::show(&m.text)).color(Color32::WHITE));
                                        let ago = duration_text(now.saturating_sub(m.ts));
                                        let meta = if !m.mine { ago } else if m.delivered { trf("{} · delivered", &[&ago]) } else { trf("{} · sent", &[&ago]) };
                                        ui.label(RichText::new(meta).size(10.0).color(Color32::from_white_alpha(150)));
                                    });
                                });
                        });
                    }
                });
                ui.horizontal(|ui| {
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let r = ui.add(egui::TextEdit::singleline(&mut self.chat_draft).hint_text(tr("Type a message")).desired_width(250.0).margin(egui::vec2(10.0, 8.0)));
                    if ui.ctx().memory(|m| m.focused().is_none()) {
                        r.request_focus(); // type right away when the chat opens
                    }
                    if (r.has_focus() || r.lost_focus()) && enter {
                        send = true;
                        r.request_focus();
                    }
                    if t::primary(ui, &tr("Send"), None, !self.chat_draft.trim().is_empty()).clicked() {
                        send = true;
                    }
                });
                if crate::rtl::has_rtl(&self.chat_draft) {
                    ui.label(crate::rtl::show(&self.chat_draft));
                }
            });
        if send && !self.chat_draft.trim().is_empty() {
            sig.send_message(&with, &self.chat_draft);
            self.chat_draft.clear();
        }
        if close {
            self.chat_with = None;
        }
    }
}

impl App {
    /// Handles a finished transfer that carried clipboard content. None = an ordinary file.
    /// Some("") = handled quietly; Some(msg) = done, tell the user.
    fn on_clip_transfer(&mut self, p: &crate::files::Progress, from: &str) -> Option<String> {
        let (batch, count, image, _) = crate::clip::parse_marked(&p.name)?;
        if p.sending || p.failed.is_some() {
            return Some(String::new());
        }
        let path = p.path.clone()?;
        let e = self.clip_batches.entry(batch.clone()).or_insert((count, image, Vec::new()));
        e.2.push(path);
        if e.2.len() < e.0 {
            return Some(String::new());
        }
        let (n, image, paths) = self.clip_batches.remove(&batch).unwrap();
        Some(match crate::clip::set_received(&paths, image) {
            Ok(()) if image => tlf("Copied a picture from {}: paste with Ctrl+V", &[from]),
            Ok(()) => tlf("Copied {} file(s) from {}: paste with Ctrl+V", &[&n.to_string(), from]),
            Err(e) => tlf("Couldn't put it on the clipboard: {}", &[&e]),
        })
    }
}
