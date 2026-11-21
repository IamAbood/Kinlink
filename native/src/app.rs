//! Window, friends UI, connection flow, streaming view with floating control box, tray.
use crate::client::{self, Client, State};
use crate::crypto::{Ephemeral, Identity, Pub};
use crate::host::{Host, HostConfig, Perms};
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
    outgoing: Option<Outgoing>,
    client: Option<Client>,
    peer: String,
    message: Option<(String, Instant)>,
    add_text: String,
    add_err: Option<String>,
    rename_text: String,
    page: Page,
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
            outgoing: None,
            client: None,
            peer: String::new(),
            message: None,
            add_text: String::new(),
            add_err: None,
            rename_text: String::new(),
            page: Page::Friends,
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
        self.signal = Some(Signal::start(id, store, self.waker()));
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
        let cfg = HostConfig { name, port: self.s.port, max_mbps: self.s.max_mbps, fps: self.s.fps, hardware: self.s.hardware, upnp: self.s.upnp };
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
        let Some(sig) = self.signal.clone() else { return };
        if self.client.is_some() || self.outgoing.is_some() {
            return;
        }
        let req: u64 = rand::random();
        crate::plog!("connect: asking {} (friend online: {})", f.id(), sig.friends().iter().any(|(x, on)| x.public == f.public && *on));
        let prep = Arc::new(Mutex::new(None));
        let (p2, to, wake) = (prep.clone(), f.public, self.waker());
        std::thread::spawn(move || match client::prepare_socket() {
            Ok((sock, addrs)) => {
                let eph = Ephemeral::new();
                let public = eph.public;
                *p2.lock().unwrap() = Some((sock, eph));
                sig.send(&to, Sig::Connect { req, eph: public, addrs });
            }
            Err(e) => {
                crate::plog!("connect: couldn't open a UDP socket: {e}");
                wake();
            }
        });
        self.outgoing = Some(Outgoing { to, name: f.id(), req, prep, at: Instant::now() });
        self.message = None;
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
                return Err(format!("You can change it again in {} days", wait.div_ceil(86_400)));
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
                self.signal = Some(Signal::start(new, store, self.waker()));
                self.rename_err = None;
                self.rename_text.clear();
                self.toast(format!("You're now {id}. Friends keep you; give new people this ID."));
            }
        }
    }

    fn cancel_outgoing(&mut self) {
        crate::plog!("connect: request cancelled");
        if let (Some(o), Some(s)) = (self.outgoing.take(), &self.signal) {
            s.send(&o.to, Sig::Cancel { req: o.req });
        }
    }

    fn accept_incoming(&mut self, r: IncomingReq) {
        let (Some(sig), Some(host)) = (self.signal.clone(), self.host.as_ref().map(|h| h.handle())) else { return };
        let Some(f) = sig.friend(&r.from) else { return };
        let perms = Perms { mouse: f.allow_mouse, keyboard: f.allow_keyboard, gamepad: f.allow_gamepad };
        std::thread::spawn(move || {
            let session: u64 = rand::random();
            let eph = Ephemeral::new();
            let cipher = eph.cipher(&r.eph, session, false);
            let addrs = host.prepare(cipher, perms, r.name);
            sig.send(&r.from, Sig::ConnectOk { req: r.req, session, eph: eph.public, addrs });
            host.punch(r.addrs, session);
        });
    }

    fn decline_incoming(&self, r: &IncomingReq, reason: &str) {
        if let Some(s) = &self.signal {
            s.send(&r.from, Sig::ConnectNo { req: r.req, reason: reason.into() });
        }
    }

    fn on_signal_event(&mut self, e: Event) {
        let Some(sig) = self.signal.clone() else { return };
        match e {
            Event::IncomingConnect { from, req, eph, addrs } => {
                let Some(f) = sig.friend(&from) else { return };
                crate::plog!("host: {} wants to connect (hosting: {}, auto-accept: {})", f.id(), self.host.is_some(), f.auto_connect);
                let r = IncomingReq { from, name: f.id(), req, eph, addrs, at: Instant::now() };
                if self.host.is_none() {
                    self.decline_incoming(&r, "isn't accepting connections right now");
                } else if f.auto_connect {
                    self.toast(format!("{} connected to this PC", f.id()));
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
                        crate::notify::toast(&format!("{} is online", f.display()), "Open Kinlink to connect.");
                    }
                }
            }
            Event::FriendRequest(id) => crate::notify::toast(&format!("{id} wants to be friends"), "Open Kinlink to accept."),
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
                Ok(p) => format!("Screenshot saved: {}", p.display()),
                Err(e) => format!("Screenshot failed: {e}"),
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
        if k.down && self.mods == [true; 3] && matches!(k.scan, SC_Q | SC_O | SC_M | SC_R | SC_F | SC_C | SC_P | SC_V) {
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
                    self.bubble = Some((if on { "Microphone on (Ctrl+Alt+Shift+V to mute)".into() } else { "Microphone off".into() }, Instant::now()));
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
                ui.label(RichText::new("Welcome to Kinlink").font(egui::FontId::new(28.0, t::bold())));
                ui.label(RichText::new("Pick a username. Friends add you with it, plus a 4-digit tag\nonly your PC can own. No account, no password.").color(t::SOFT));
                ui.add_space(14.0);
                let r = ui.add(egui::TextEdit::singleline(&mut self.setup_name).hint_text("username").desired_width(300.0).margin(egui::vec2(14.0, 11.0)).font(egui::FontId::new(16.0, t::bold())));
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    go = true;
                }
                ui.add_space(6.0);
                if t::primary(ui, "Continue", None, true).clicked() {
                    go = true;
                }
                ui.label(RichText::new("2–20 letters or numbers").size(12.0).color(t::MUTED));
                if let Some(e) = &self.setup_err {
                    ui.colored_label(t::DANGER, e);
                }
                ui.add_space(30.0);
                ui.label(RichText::new("Made by Abdulkareem Alsaou").size(12.0).color(t::MUTED));
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
                            ui.label(RichText::new("wants to connect to this PC").size(12.0).color(t::MUTED));
                        });
                    });
                    if let Some(f) = &f {
                        let allowed: Vec<&str> = [(f.allow_mouse, "mouse"), (f.allow_keyboard, "keyboard"), (f.allow_gamepad, "controller")].iter().filter(|x| x.0).map(|x| x.1).collect();
                        let what = if allowed.is_empty() { "They can only watch (no mouse, keyboard or controller).".to_string() } else { format!("Allowed: {}. Change it in Friends.", allowed.join(", ")) };
                        ui.label(RichText::new(what).size(12.0).color(t::SOFT));
                    }
                    ui.label(RichText::new(format!("Declines by itself in {left}s")).size(11.0).color(t::MUTED));
                    ui.horizontal(|ui| {
                        if t::primary(ui, "Accept", None, true).clicked() {
                            decided = Some((i, true));
                        }
                        if ui.add(egui::Button::new("Decline").min_size(egui::vec2(100.0, 36.0))).clicked() {
                            decided = Some((i, false));
                        }
                    });
                });
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
        for p in c.take_finished_files() {
            let text = match (&p.failed, p.sending) {
                (Some(e), _) => format!("{}: {e}", p.name),
                (None, true) => format!("Sent {}", p.name),
                (None, false) => format!("Received {} (Downloads\\Kinlink)", p.name),
            };
            self.bubble = Some((text, Instant::now()));
        }
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
                        ui.label(RichText::new("REC").font(egui::FontId::new(12.0, crate::theme::bold())).color(crate::theme::DANGER));
                    });
                });
            });
        }
        let unread = if self.chat_open { 0 } else { c.take_unread() };
        if unread > 0 {
            if let Some((from, text)) = c.chat_log().last() {
                self.bubble = Some((format!("{from}: {text}   (Ctrl+Alt+Shift+C to reply)"), Instant::now()));
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
                            if ui.small_button("Cancel").clicked() {
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
                        ui.label(RichText::new("Say hi. Messages go to everyone in this session.").color(t::MUTED));
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
                let r = ui.add(egui::TextEdit::singleline(&mut self.chat_text).hint_text("Type and press Enter").desired_width(320.0).margin(egui::vec2(10.0, 8.0)));
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
        let who = if had { "everyone was disconnected" } else { "nobody was connected" };
        self.toast(format!("Panic button: {who}. Sharing is paused; tick \"Let friends connect\" to turn it back on."));
        self.show_window();
    }

    fn overlay_ui(&mut self, ctx: &egui::Context) {
        use crate::theme::{self as t, Icon};
        self.draw_remote_cursor(ctx);
        let Some(c) = &self.client else { return };
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
                if resp.on_hover_text("Menu").clicked() {
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
                        if t::icon_button(ui, Icon::Close, "Hide menu", false).clicked() {
                            self.overlay_open = false;
                        }
                    });
                });
                // actions
                let w = (300.0 - 3.0 * 6.0) / 4.0;
                let mic = c.mic_on();
                let rec = c.is_recording();
                ui.horizontal(|ui| {
                    if t::tile(ui, Icon::Mic, if mic { "Mic on" } else { "Mic off" }, mic.then_some(t::MINT), w).on_hover_text("Voice chat · Ctrl+Alt+Shift+V · use headphones").clicked() {
                        c.set_mic(!mic);
                    }
                    if t::tile(ui, Icon::Chat, "Chat", self.chat_open.then_some(t::ACCENT_SOFT), w).on_hover_text("Ctrl+Alt+Shift+C").clicked() {
                        toggle_chat = true;
                    }
                    if t::tile(ui, Icon::Upload, "Send file", None, w).on_hover_text("Or drag files onto this window").clicked() {
                        pick = true;
                    }
                    if t::tile(ui, Icon::Camera, "Screenshot", None, w).on_hover_text("Ctrl+Alt+Shift+P · Pictures\\Kinlink").clicked() {
                        shot = true;
                    }
                });
                ui.horizontal(|ui| {
                    if t::tile(ui, Icon::Record, if rec { "Stop rec" } else { "Record" }, rec.then_some(t::DANGER), w).on_hover_text("MP4 in Videos\\Kinlink").clicked() {
                        if rec { c.stop_recording() } else { c.start_recording() }
                    }
                    if t::tile(ui, if is_full { Icon::Shrink } else { Icon::Expand }, if is_full { "Windowed" } else { "Fullscreen" }, None, w).on_hover_text("Ctrl+Alt+Shift+F").clicked() {
                        toggle_full = true;
                    }
                    if t::tile(ui, Icon::Lock, "Lock mouse", self.relative.then_some(t::ACCENT_SOFT), w).on_hover_text("For games · Ctrl+Alt+Shift+R to unlock").clicked() {
                        toggle_rel = true;
                    }
                    if t::tile(ui, Icon::Hide, "Hide all", None, w).on_hover_text("Hides the menu button too · Ctrl+Alt+Shift+O brings it back").clicked() {
                        hide_menu = true;
                    }
                });
                // stream settings
                egui::Grid::new("ov").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label(RichText::new("Resolution").color(t::SOFT));
                    egui::ComboBox::from_id_salt("res").width(150.0).selected_text(RESOLUTIONS[self.res_idx].0).show_ui(ui, |ui| {
                        for (i, (name, _)) in RESOLUTIONS.iter().enumerate() {
                            quality_changed |= ui.selectable_value(&mut self.res_idx, i, *name).changed();
                        }
                    });
                    ui.end_row();
                    ui.label(RichText::new("Quality").color(t::SOFT));
                    egui::ComboBox::from_id_salt("q").width(150.0).selected_text(QUALITIES[self.qual_idx].0).show_ui(ui, |ui| {
                        for (i, (name, _)) in QUALITIES.iter().enumerate() {
                            quality_changed |= ui.selectable_value(&mut self.qual_idx, i, *name).changed();
                        }
                    });
                    ui.end_row();
                    if let State::Connected { monitors, current, .. } = &state {
                        if monitors.len() > 1 {
                            ui.label(RichText::new("Monitor").color(t::SOFT));
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
                    ui.label(RichText::new("Volume").color(t::SOFT));
                    ui.horizontal(|ui| {
                        let a = ui.add(egui::Slider::new(&mut self.volume, 0..=100).show_value(false)).changed();
                        let b = ui.checkbox(&mut self.muted, "Mute").changed();
                        if a || b {
                            c.set_volume(if self.muted { 0 } else { self.volume });
                        }
                    });
                    ui.end_row();
                });
                if let State::Connected { mouse, keyboard, gamepad, .. } = state {
                    if !(mouse && keyboard && gamepad) {
                        let allowed: Vec<&str> = [(mouse, "mouse"), (keyboard, "keyboard"), (gamepad, "controller")].iter().filter(|x| x.0).map(|x| x.1).collect();
                        let what = if allowed.is_empty() { "view only (spectator)".to_string() } else { format!("{} only", allowed.join(" + ")) };
                        ui.label(RichText::new(format!("Host allows {what}")).color(Color32::from_rgb(0xFF, 0xC9, 0x8A)));
                    }
                }
                egui::Frame::NONE.fill(t::BG).corner_radius(9.0).inner_margin(egui::Margin::symmetric(10, 7)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(format!("{:.0} fps · {:.0} ms · {:.1} Mbps · decode {:.1} ms · lost {}", st.fps, st.rtt_ms, st.mbps, st.decode_ms, st.lost)).monospace().size(11.0).color(t::MUTED));
                });
                ui.label(RichText::new("Ctrl+Alt+Shift: Q disconnect · O hide · R lock mouse · F fullscreen").size(11.0).color(t::MUTED));
                let fresh = self.overlay_opened.elapsed() < Duration::from_millis(500);
                if t::danger_wide(ui, "Disconnect", !fresh).clicked() && !fresh {
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
                    ui.colored_label(Color32::WHITE, "Menu: small button top-left · Ctrl+Alt+Shift+Q to disconnect");
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
                        ui.label(RichText::new("Kinlink").font(egui::FontId::new(17.0, t::bold())));
                        ui.label(RichText::new(format!("v{}", crate::updater::current())).size(11.0).color(t::MUTED));
                    });
                });
                ui.add_space(14.0);
                ui.spacing_mut().item_spacing.y = 4.0;
                if t::nav(ui, Icon::Friends, "Friends", self.page == Page::Friends, Some(sig.incoming().len())).clicked() {
                    go = Some(Page::Friends);
                }
                if t::nav(ui, Icon::Monitor, "This PC", self.page == Page::Pc, Some(guests)).clicked() {
                    go = Some(Page::Pc);
                }
                if t::nav(ui, Icon::Files, "Files", self.page == Page::Files, None).clicked() {
                    go = Some(Page::Files);
                }
                if t::nav(ui, Icon::Settings, "Settings", self.page == Page::Settings, None).clicked() {
                    go = Some(Page::Settings);
                }
                egui::Panel::bottom("sidebar_bottom").resizable(false).show_separator_line(false).frame(egui::Frame::NONE).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 10.0;
                    let u = self.update.lock().unwrap().clone();
                    match u {
                        Update::Available(r) => {
                            if t::primary(ui, &format!("Update to {}", r.version), Some(Icon::Update), true).on_hover_text("Downloads, checks and restarts the app").clicked() {
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
                            ui.colored_label(t::DANGER, "Update failed").on_hover_text(e);
                        }
                        _ => {}
                    }
                    t::card().fill(t::FIELD).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 6.0;
                        ui.horizontal(|ui| {
                            t::section(ui, "Your ID");
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if t::icon_button(ui, Icon::Copy, "Copy your ID", false).clicked() {
                                    ui.ctx().copy_text(sig.id());
                                    self.toast("Your ID was copied. Send it to a friend so they can add you.");
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
                        ui.horizontal(|ui| {
                            let (c, text) = if sig.online() {
                                (t::MINT, if self.host.is_some() { "Online · hosting on" } else { "Online · hosting off" })
                            } else {
                                (t::MUTED, "Connecting…")
                            };
                            dot(ui, c);
                            ui.label(RichText::new(text).size(12.0).color(if sig.online() { t::MINT_SOFT } else { t::MUTED }));
                        });
                    });
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("Made by Abdulkareem Alsaou").size(11.0).color(t::MUTED));
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
                    ui.label(format!("Waiting for {name} to accept…"));
                    cancel = ui.button("Cancel").clicked();
                });
                log_tail(ui);
                if cancel {
                    self.cancel_outgoing();
                }
            } else if self.client.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("Connecting to {}…", self.peer));
                });
                log_tail(ui);
            } else if let Some((m, _)) = &self.message {
                let m = m.clone();
                ui.horizontal(|ui| {
                    ui.label(crate::rtl::show(&m));
                    if m.contains("Downloads") && ui.small_button("Open folder").clicked() {
                        open_folder(crate::files::default_save_dir());
                    }
                });
            } else {
                ui.label(RichText::new("In a session: Ctrl+Alt+Shift + Q disconnect · O hide menu · R lock mouse · F fullscreen").size(11.0).color(t::MUTED));
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
                        t::title(ui, "Friends", Some(&format!("{online} of {} online", friends.len())));
                        ui.horizontal(|ui| {
                            let enter_now = ui.input(|i| i.key_pressed(egui::Key::Enter));
                            let r = ui.add(egui::TextEdit::singleline(&mut self.add_text).hint_text("Add a friend: name#1234-ABCDEF").desired_width(230.0).margin(egui::vec2(10.0, 8.0)));
                            let enter = (r.has_focus() || r.lost_focus()) && enter_now;
                            if t::primary(ui, "Add", Some(Icon::Plus), true).clicked() || enter {
                                match sig.add_friend(&self.add_text) {
                                    Ok(()) => {
                                        self.add_err = None;
                                        self.add_text.clear();
                                    }
                                    Err(e) => self.add_err = Some(e),
                                }
                            }
                        });
                        if let Some(e) = &self.add_err {
                            ui.colored_label(t::DANGER, e);
                        }
                        for (id, p) in sig.incoming() {
                            egui::Frame::NONE.fill(t::ACCENT_BG).stroke(egui::Stroke::new(1.0, t::ACCENT_LINE)).corner_radius(12.0).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.label(RichText::new(format!("Safety code {}: it must match on their screen. If it doesn't, decline.", sig.safety_code(&p))).size(12.0).color(t::SOFT));
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&id).font(egui::FontId::new(14.0, t::bold())));
                                    ui.label("wants to be friends");
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.button("Decline").clicked() {
                                            sig.decline(p);
                                        }
                                        if t::primary(ui, "Accept", None, true).clicked() {
                                            sig.accept(p);
                                        }
                                    });
                                });
                            });
                        }
                        for o in sig.outgoing_info() {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("Waiting for {} to accept", o.id)).italics().color(t::MUTED));
                                if let Some(code) = &o.code {
                                    ui.label(RichText::new(format!("· safety code {code}")).color(t::SOFT));
                                }
                                if ui.small_button("Cancel").clicked() {
                                    sig.cancel_request(&o.id);
                                }
                            });
                            if o.conflict {
                                ui.colored_label(t::DANGER, "Two different PCs answered for this ID. Only accept if the safety code matches on your friend's screen.");
                            }
                        }
                        friends.sort_by_key(|(f, on)| (!on, f.display().to_lowercase()));
                        if friends.is_empty() {
                            t::card().show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.label(RichText::new("No friends yet").font(egui::FontId::new(16.0, t::bold())));
                                ui.label(RichText::new("Send someone your ID (bottom left) and add theirs above. They accept, and you can connect.").color(t::SOFT));
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
                                let title = if group.is_empty() { "Other".to_string() } else { crate::rtl::show(&group) };
                                t::section(ui, &format!("{title} · {on}/{} online", members.len()));
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
                            t::title(ui, "This PC", Some(sub));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let mut hosting = self.s.hosting;
                                if t::switch(ui, &mut hosting, "Let friends connect") {
                                    host_toggle = Some(hosting);
                                }
                            });
                        });
                        if let (Some(h), Some(st)) = (&self.host, &hstat) {
                            ui.columns(3, |c| {
                                let stats = [
                                    (st.clients.len().to_string(), if st.clients.len() == 1 { "guest connected".to_string() } else { "guests connected".to_string() }),
                                    (if st.encoder.is_empty() { "Idle".to_string() } else { "Streaming".to_string() }, if st.encoder.is_empty() { "starts when a friend joins".to_string() } else { st.encoder.clone() }),
                                    (format!("{} Mbps", self.s.max_mbps), format!("max · {} fps", self.s.fps)),
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
                                t::section(ui, "Guests");
                                t::card().show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    for c in &st.clients {
                                        ui.horizontal(|ui| {
                                            dot(ui, t::MINT);
                                            ui.label(RichText::new(c).font(egui::FontId::new(14.0, t::bold())));
                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                if ui.button(RichText::new("Kick").color(t::DANGER)).clicked() {
                                                    h.kick(c);
                                                }
                                            });
                                        });
                                    }
                                });
                                ui.horizontal(|ui| {
                                    let mut mic = h.mic_on();
                                    if t::switch(ui, &mut mic, "Talk to guests (mic · use headphones)") {
                                        h.set_mic(mic);
                                    }
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.button(RichText::new("Panic: kick everyone").color(t::DANGER)).on_hover_text("Ctrl+Alt+Shift+F12 anywhere").clicked() {
                                            panic_clicked = true;
                                        }
                                        if ui.button("Send file…").clicked() {
                                            pick = true;
                                        }
                                    });
                                });
                                let (talking, mic_err) = h.voice_status();
                                if !talking.is_empty() {
                                    ui.label(RichText::new(format!("Talking: {}", talking.join(", "))).color(t::MINT_SOFT));
                                }
                                if let Some(e) = mic_err {
                                    ui.colored_label(t::DANGER, e);
                                }
                                t::section(ui, "Chat");
                                t::card().show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    egui::ScrollArea::vertical().id_salt("hostchat").max_height(140.0).stick_to_bottom(true).show(ui, |ui| {
                                        if st.chat.is_empty() {
                                            ui.label(RichText::new("No messages yet").color(t::MUTED));
                                        }
                                        for (from, text) in st.chat.iter().rev().take(30).rev() {
                                            ui.label(crate::rtl::show(&format!("{from}: {text}")));
                                        }
                                    });
                                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                                    let r = ui.add(egui::TextEdit::singleline(&mut self.host_chat_text).hint_text("Message your guests").desired_width(f32::INFINITY).margin(egui::vec2(10.0, 8.0)));
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
                                    t::section(ui, "Controllers");
                                    t::card().show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        let n = pads.len();
                                        for (i, p) in pads.iter().enumerate() {
                                            ui.horizontal(|ui| {
                                                let slot = p.player.map(|n| format!("Player {n}")).unwrap_or_else(|| format!("#{}", i + 1));
                                                let kind = if p.playstation { "PlayStation" } else { "Xbox" };
                                                ui.label(RichText::new(slot).font(egui::FontId::new(14.0, t::bold())));
                                                ui.label(RichText::new(format!("{} · pad {} · {kind}", p.guest, p.pad + 1)).color(t::SOFT));
                                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                    if i + 1 < n && ui.small_button("Down").clicked() {
                                                        move_pad = Some((i, false));
                                                    }
                                                    if i > 0 && ui.small_button("Up").clicked() {
                                                        move_pad = Some((i, true));
                                                    }
                                                });
                                            });
                                        }
                                    });
                                }
                            }
                            if let Some(e) = &st.error {
                                ui.colored_label(t::DANGER, e);
                            }
                            for n in &st.notes {
                                ui.label(RichText::new(n).size(11.5).color(t::MUTED));
                            }
                        } else {
                            egui::Frame::NONE.stroke(egui::Stroke::new(1.0, t::LINE2)).corner_radius(14.0).inner_margin(egui::Margin::same(28)).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.vertical_centered(|ui| {
                                    ui.label(RichText::new("Hosting is off. Friends can't connect to this PC until you turn it on.").color(t::SOFT));
                                });
                            });
                        }
                        if let Some(e) = &self.host_err {
                            ui.colored_label(t::DANGER, e);
                        }
                    }

                    // ---------------------------------------- Files
                    Page::Files => {
                        ui.horizontal(|ui| {
                            t::title(ui, "Files", Some("Received files go to Downloads\\Kinlink"));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Open Downloads").clicked() {
                                    open_folder(crate::files::default_save_dir());
                                }
                            });
                        });
                        egui::Frame::NONE.fill(Color32::from_rgb(0x11, 0x0F, 0x2C)).stroke(egui::Stroke::new(1.5, t::ACCENT_LINE)).corner_radius(16.0).inner_margin(egui::Margin::same(22)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.vertical_centered(|ui| {
                                let (r, _) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
                                t::icon(ui.painter(), r, Icon::Upload, t::ACCENT_SOFT);
                                ui.label(RichText::new("Drop files on this window to send them").font(egui::FontId::new(15.0, t::bold())));
                                if guests > 0 {
                                    if t::primary(ui, "Choose a file…", None, true).clicked() {
                                        pick = true;
                                    }
                                    ui.label(RichText::new(format!("Goes to your {guests} guest(s)")).size(12.0).color(t::MUTED));
                                } else {
                                    ui.label(RichText::new("Sends to whoever you're connected with: guests on this PC, or the PC you're controlling (use Send file in its menu).").size(12.0).color(t::MUTED));
                                }
                            });
                        });
                        let xfers = hstat.as_ref().map(|s| s.transfers.clone()).unwrap_or_default();
                        t::section(ui, "Transfers");
                        t::card().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            if xfers.is_empty() {
                                ui.label(RichText::new("Nothing in progress").color(t::MUTED));
                            }
                            for (guest, p) in &xfers {
                                ui.horizontal(|ui| {
                                    ui.label(crate::rtl::show(&format!("{} {} {} {}", if p.sending { "Sending" } else { "Receiving" }, p.name, if p.sending { "to" } else { "from" }, guest)));
                                    ui.add(egui::ProgressBar::new(p.done as f32 / p.size.max(1) as f32).desired_width(140.0).show_percentage());
                                    if ui.small_button("Cancel").clicked() {
                                        if let Some(h) = &self.host {
                                            h.cancel_file(guest, p.id);
                                        }
                                    }
                                });
                            }
                        });
                        t::section(ui, "Captures");
                        ui.horizontal(|ui| {
                            if ui.button("Screenshots folder").on_hover_text("Pictures\\Kinlink").clicked() {
                                open_folder(crate::rec::user_dir("Pictures"));
                            }
                            if ui.button("Recordings folder").on_hover_text("Videos\\Kinlink").clicked() {
                                open_folder(crate::rec::user_dir("Videos"));
                            }
                        });
                    }

                    // ---------------------------------------- Settings
                    Page::Settings => {
                        t::title(ui, "Settings", None);
                        ui.columns(2, |c| {
                            let (left, right) = c.split_at_mut(1);
                            let (l, r) = (&mut left[0], &mut right[0]);
                            t::card().show(l, |ui| {
                                ui.set_width(ui.available_width());
                                t::section(ui, "Account");
                                ui.horizontal(|ui| {
                                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                                    let rr = ui.add(egui::TextEdit::singleline(&mut self.rename_text).hint_text("new username").desired_width(150.0).margin(egui::vec2(10.0, 7.0)));
                                    if ui.button("Change").clicked() || ((rr.has_focus() || rr.lost_focus()) && enter) {
                                        rename_now = true;
                                    }
                                });
                                if let Some(e) = &self.rename_err {
                                    ui.colored_label(t::DANGER, e);
                                }
                                ui.label(RichText::new("Letters and numbers only, once every 3 months. Your #tag changes too; friends keep you.").size(11.5).color(t::MUTED));
                            });
                            t::card().show(r, |ui| {
                                ui.set_width(ui.available_width());
                                t::section(ui, "General");
                                if t::switch(ui, &mut self.autostart, "Start with Windows (in tray)") {
                                    settings::set_autostart(self.autostart);
                                }
                                if t::switch(ui, &mut self.s.notify_online, "Notify when friends come online") {
                                    self.s.save();
                                }
                                if t::switch(ui, &mut self.s.fullscreen_connect, "Fullscreen when connecting") {
                                    self.s.save();
                                }
                            });
                            t::card().show(l, |ui| {
                                ui.set_width(ui.available_width());
                                t::section(ui, "Hosting quality");
                                restart_host |= t::switch(ui, &mut self.s.hardware, "Hardware encoding (GPU)");
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Max fps").color(t::SOFT));
                                    egui::ComboBox::from_id_salt("fps").selected_text(format!("{}", self.s.fps)).show_ui(ui, |ui| {
                                        for f in [30, 60, 120, 144] {
                                            restart_host |= ui.selectable_value(&mut self.s.fps, f, format!("{f}")).changed();
                                        }
                                    });
                                });
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Max bitrate").color(t::SOFT));
                                    restart_host |= ui.add(egui::Slider::new(&mut self.s.max_mbps, 3..=100).suffix(" Mbps")).drag_stopped();
                                });
                            });
                            t::card().show(r, |ui| {
                                ui.set_width(ui.available_width());
                                t::section(ui, "Network");
                                restart_host |= t::switch(ui, &mut self.s.upnp, "Open router port (UPnP)");
                                ui.horizontal_wrapped(|ui| {
                                    if t::primary(ui, "Run network test", None, self.nettest.is_none()).on_hover_text("Checks your router, firewall and speed (uses Cloudflare's speed test)").clicked() {
                                        let slot = Arc::new(Mutex::new(None));
                                        let (s2, online, wake) = (slot.clone(), sig.online(), self.waker());
                                        std::thread::spawn(move || {
                                            *s2.lock().unwrap() = Some(crate::nettest::run(online));
                                            wake();
                                        });
                                        self.nettest = Some(slot);
                                    }
                                    if ui.button("Allow through firewall").on_hover_text("One-time; asks for admin permission").clicked() {
                                        allow_firewall();
                                    }
                                });
                            });
                            t::card().show(l, |ui| {
                                ui.set_width(ui.available_width());
                                t::section(ui, "Lock screen & admin prompts");
                                let (installed, running) = crate::syshelper::service_state();
                                if installed {
                                    ui.colored_label(if running { t::MINT_SOFT } else { t::DANGER }, if running { "Guests can control them" } else { "Helper installed but not running" });
                                    ui.horizontal(|ui| {
                                        if ui.button("Turn off").on_hover_text("Removes the helper service (asks for admin)").clicked() {
                                            crate::syshelper::run_elevated("--uninstall-service");
                                        }
                                    });
                                } else {
                                    ui.label(RichText::new("Lets guests see and use the Windows lock screen, UAC prompts and admin apps.").size(12.0).color(t::SOFT));
                                    ui.horizontal(|ui| {
                                        if ui.button("Enable (asks for admin)").clicked() {
                                            crate::syshelper::run_elevated("--install-service");
                                        }
                                    });
                                }
                            });
                            t::card().show(r, |ui| {
                                ui.set_width(ui.available_width());
                                t::section(ui, "Help");
                                ui.horizontal(|ui| {
                                    if ui.button("Open log").on_hover_text("Everything the app did this run (connections, errors)").clicked() {
                                        if let Some(p) = crate::applog::path() {
                                            let _ = std::process::Command::new("notepad").arg(p).spawn();
                                        }
                                    }
                                });
                                ui.label(RichText::new(format!("Kinlink {} · made by Abdulkareem Alsaou", crate::updater::current())).size(11.5).color(t::MUTED));
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
                    ui.label(RichText::new(if online { "Online" } else { "Offline" }).size(12.0).color(if online { t::MINT_SOFT } else { t::MUTED }));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let open = self.expanded == Some(f.public);
                    if t::icon_button(ui, Icon::More, "Permissions, nickname, remove", open).clicked() {
                        self.expanded = if open { None } else { Some(f.public) };
                    }
                    let busy = self.client.is_some() || self.outgoing.is_some();
                    if online {
                        if t::primary(ui, "Connect", Some(Icon::Play), !busy).clicked() {
                            *connect = Some(f.clone());
                        }
                    }
                });
            });
            if self.expanded == Some(f.public) {
                let mut g = f.clone();
                egui::Frame::NONE.fill(t::BG).stroke(egui::Stroke::new(1.0, t::LINE)).corner_radius(12.0).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.columns(2, |c| {
                        let ui = &mut c[0];
                        ui.label(RichText::new("Nickname").size(12.0).color(t::SOFT));
                        ui.add(egui::TextEdit::singleline(&mut g.nickname).hint_text("e.g. Ahmad's gaming PC").margin(egui::vec2(10.0, 7.0)));
                        ui.label(RichText::new("Group").size(12.0).color(t::SOFT));
                        ui.add(egui::TextEdit::singleline(&mut g.group).hint_text("e.g. Home").margin(egui::vec2(10.0, 7.0)));
                        ui.horizontal(|ui| {
                            if ui.button("Desktop shortcut").on_hover_text("One click on the desktop connects to this friend").clicked() {
                                shortcut_for = Some(f.clone());
                            }
                            if ui.button(RichText::new("Remove").color(t::DANGER)).clicked() {
                                sig.remove(f.public);
                                self.expanded = None;
                            }
                        });
                        let ui = &mut c[1];
                        ui.label(RichText::new("When they connect to this PC").size(12.0).color(t::SOFT));
                        t::switch(ui, &mut g.allow_mouse, "Mouse");
                        t::switch(ui, &mut g.allow_keyboard, "Keyboard");
                        t::switch(ui, &mut g.allow_gamepad, "Controller");
                        t::switch(ui, &mut g.auto_connect, "Connect without asking");
                        ui.label(RichText::new("All three off = spectator (view only)").size(11.0).color(t::MUTED));
                        ui.label(RichText::new(format!("Safety code: {}", sig.safety_code(&f.public))).size(11.5).color(t::SOFT))
                            .on_hover_text("Same code on both screens = you're really talking to each other");
                    });
                });
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
                Ok(()) => format!("Shortcut \"Connect to {}\" added to your desktop", f.display()),
                Err(e) => format!("Couldn't create the shortcut: {e}"),
            });
        }
    }

    fn nettest_ui(&mut self, ctx: &egui::Context) {
        let Some(slot) = self.nettest.clone() else { return };
        let mut open = true;
        let mut apply: Option<u32> = None;
        egui::Window::new(RichText::new("Network test").strong()).collapsible(false).resizable(false).open(&mut open).show(ctx, |ui| {
            let r = slot.lock().unwrap();
            let Some(r) = r.as_ref() else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Testing... (about 15 seconds)");
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
                ui.label(format!("Suggested max bitrate when friends connect to you: {m} Mbps (current {} Mbps)", self.s.max_mbps));
                ui.small("If several friends join at once, they share your upload: lower it further.");
                if ui.button(format!("Use {m} Mbps")).clicked() {
                    apply = Some(m);
                }
            }
        });
        if let Some(m) = apply {
            self.s.max_mbps = m;
            self.restart_host_if_idle();
            self.toast(format!("Max bitrate set to {m} Mbps"));
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
            if let (None, false) = (&p.failed, p.sending) {
                self.toast(format!("Received {} from {guest} (Downloads\\Kinlink)", p.name));
                crate::notify::toast(&format!("{guest} sent you {}", p.name), "Saved in Downloads\\Kinlink");
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
        if let (Some((id, since)), Some(sig)) = (self.connect_on_start.clone(), self.signal.clone()) {
            match sig.friends().into_iter().find(|(f, _)| f.id().eq_ignore_ascii_case(&id)) {
                Some((f, true)) => {
                    self.connect_on_start = None;
                    self.connect_to(&f);
                }
                None if since.elapsed() > Duration::from_secs(5) => {
                    self.connect_on_start = None;
                    self.toast(format!("{id} isn't in your friends list"));
                }
                Some((f, false)) if since.elapsed() > Duration::from_secs(60) => {
                    self.connect_on_start = None;
                    self.toast(format!("{} isn't online right now", f.display()));
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
            self.toast(format!("No answer from {name}"));
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
                Ok(()) => format!("Sending {name} to {}", self.peer),
                Err(e) => format!("Can't send {name}: {e}"),
            }, Instant::now()));
        } else if let Some(h) = &self.host {
            match h.send_file(path) {
                Ok(0) => self.toast("Nobody is connected to receive files"),
                Ok(n) => self.toast(format!("Sending {name} to {n} guest(s)")),
                Err(e) => self.toast(format!("Can't send {name}: {e}")),
            }
        } else {
            self.toast("Connect to a friend (or have one connect to you) to send files");
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
