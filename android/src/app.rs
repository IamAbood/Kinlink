//! Phone UI: set up a username, manage friends, connect, then drive the PC with the screen
//! as a trackpad (like a laptop touchpad) plus the phone keyboard.
use crate::client::{self, Client, State};
use crate::crypto::{self, Ephemeral, Identity, Pub};
use crate::proto::Input;
use crate::render::Gfx;
use crate::signal::{Event, Sig, Signal};
use crate::i18n::{tl, tlf, tr, trf};
use egui::RichText;
use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::platform::android::activity::AndroidApp;
use winit::window::{Window, WindowId};

/// Trackpad speed: fraction of the PC screen per phone-screen width swiped.
const SPEED: f32 = 1.4;
const TAP: Duration = Duration::from_millis(320);
const HOLD: Duration = Duration::from_millis(450);
/// How far (in dp, scaled to the screen) a finger may wobble and still count as a tap.
const SLOP_DP: f32 = 14.0;

/// Picture quality presets: (name, max height, Mbps, sharp text). Default: Balanced.
const PHONE_PRESETS: &[(&str, u32, u32, bool)] = &[("Low data", 540, 3, false), ("Balanced", 720, 8, false), ("Best", 1080, 20, false), ("Sharp text", 1080, 20, true)];

/// One notification per friend (a new one replaces the old).
fn notif_id(p: &Pub) -> i32 {
    i32::from_le_bytes([p[0], p[1], p[2], p[3] & 0x3f])
}

struct Outgoing {
    to: Pub,
    name: String,
    req: u64,
    prep: Arc<Mutex<Option<(UdpSocket, Ephemeral)>>>,
    at: Instant,
}

/// One gesture on the trackpad, from first finger down to last finger up.
#[derive(Default)]
struct Gesture {
    /// Two-finger pinch: zooming instead of scrolling; finger distance and zoom when it began.
    pinch: bool,
    dist0: f32,
    zoom0: f32,
    fingers: HashMap<u64, (f32, f32)>,
    max_fingers: usize,
    start: Option<Instant>,
    travel: f32,
    dragging: bool,
    wheel: f32,
}

pub struct App {
    android: AndroidApp,
    proxy: Arc<Mutex<EventLoopProxy<()>>>,
    ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    gfx: Option<Gfx>,
    signal: Option<Signal>,
    client: Option<Client>,
    outgoing: Option<Outgoing>,
    peer: String,
    name_input: String,
    add_input: String,
    message: Option<String>,
    menu: bool,
    keyboard: bool,
    muted: bool,
    /// Controller mode: the screen is a gamepad instead of a trackpad.
    pad_mode: bool,
    pad: crate::gamepad::TouchPad,
    /// A newer release tag found on GitHub.
    update: Arc<Mutex<Option<String>>>,
    /// Update download progress text (set from the download thread; "" = installer opened).
    dl_note: Arc<Mutex<Option<String>>>,
    cursor: (f32, f32),
    /// PHONE_PRESETS index (picture quality).
    preset: usize,
    /// App start (friends that are already online all show up in the first seconds).
    started: Instant,
    /// Pinch zoom (1 = whole screen); the view follows the cursor when zoomed in.
    zoom: f32,
    g: Gesture,
}

impl App {
    pub fn new(android: AndroidApp, proxy: EventLoopProxy<()>) -> Self {
        let mut app = Self {
            android,
            proxy: Arc::new(Mutex::new(proxy)),
            ctx: egui::Context::default(),
            egui_state: None,
            gfx: None,
            signal: None,
            client: None,
            outgoing: None,
            peer: String::new(),
            name_input: String::new(),
            add_input: String::new(),
            message: None,
            menu: false,
            keyboard: false,
            muted: false,
            pad_mode: false,
            pad: Default::default(),
            update: Arc::new(Mutex::new(None)),
            dl_note: Arc::new(Mutex::new(None)),
            cursor: (0.5, 0.5),
            preset: 1,
            started: Instant::now(),
            zoom: 1.0,
            g: Gesture::default(),
        };
        crate::theme::apply(&app.ctx);
        app.ctx.global_style_mut(|s| {
            s.spacing.button_padding = egui::vec2(14.0, 10.0);
            s.spacing.interact_size.y = 40.0; // finger-sized
        });
        if let Some(id) = Identity::load() {
            app.start_signal(id);
        }
        crate::mic::ask_notifications();
        check_for_update(app.update.clone(), app.waker());
        app
    }

    fn waker(&self) -> impl Fn() + Send + Sync + 'static {
        let p = self.proxy.clone();
        move || {
            let _ = p.lock().unwrap().send_event(());
        }
    }

    fn start_signal(&mut self, id: Identity) {
        let store = crate::settings::data_dir().map(|d| d.join("friends.json"));
        self.signal = Some(Signal::start(id, store, self.waker()));
    }

    fn redraw(&self) {
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
    }

    fn set_keyboard(&mut self, on: bool) {
        self.keyboard = on;
        if on {
            self.android.show_soft_input(true);
        } else {
            self.android.hide_soft_input(false);
        }
    }

    // ------------------------------------------------------------ connection

    fn connect_to(&mut self, to: Pub, name: String) {
        let Some(sig) = self.signal.clone() else { return };
        if self.client.is_some() || self.outgoing.is_some() {
            return;
        }
        let req: u64 = rand::random();
        let prep = Arc::new(Mutex::new(None));
        let (p2, wake) = (prep.clone(), self.waker());
        std::thread::spawn(move || match client::prepare_socket() {
            Ok((sock, addrs)) => {
                let eph = Ephemeral::new();
                let public = eph.public;
                *p2.lock().unwrap() = Some((sock, eph));
                sig.send(&to, Sig::Connect { req, eph: public, addrs });
            }
            Err(e) => {
                log::error!("socket: {e}");
                wake();
            }
        });
        self.outgoing = Some(Outgoing { to, name, req, prep, at: Instant::now() });
        self.message = None;
    }

    fn cancel_outgoing(&mut self) {
        if let (Some(o), Some(s)) = (self.outgoing.take(), &self.signal) {
            s.send(&o.to, Sig::Cancel { req: o.req });
        }
    }

    /// In a session: full screen (no status bar to pull down by accident) + keep the screen on.
    fn window_flags(&self, on: bool) {
        // Not android-activity's set_window_flags: it takes a write lock the event loop holds while
        // it runs our handlers, which froze the app on connect / disconnect.
        const FLAGS: i32 = 0x400 | 0x80; // FLAG_FULLSCREEN | FLAG_KEEP_SCREEN_ON
        let (add, clear) = if on { (FLAGS, 0) } else { (0, FLAGS) };
        let activity = self.android.activity_as_ptr() as usize;
        self.android.run_on_java_main_thread(Box::new(move || crate::mic::window_flags(activity, add, clear)));
    }

    fn disconnect(&mut self, why: Option<String>) {
        self.window_flags(false);
        self.zoom = 1.0;
        self.pad_mode = false;
        self.pad.release();
        if let Some(c) = self.client.take() {
            c.input(Input::PadRemove { id: 0 });
            c.input(Input::ReleaseAll);
            c.disconnect();
        }
        if let Some(g) = &mut self.gfx {
            g.clear_video();
        }
        self.set_keyboard(false);
        self.menu = false;
        self.message = why;
    }

    fn on_event(&mut self, e: Event) {
        match e {
            // The phone only connects out; it never hosts.
            Event::IncomingConnect { from, req, .. } => {
                if let Some(s) = &self.signal {
                    s.send(&from, Sig::ConnectNo { req, reason: "is on a phone (can't be controlled)".into() });
                }
            }
            Event::ConnectReply { from, sig } => {
                let Some(o) = self.outgoing.as_ref().filter(|o| o.to == from) else { return };
                match sig {
                    Sig::ConnectOk { req, session, eph, addrs } if req == o.req => {
                        let o = self.outgoing.take().unwrap();
                        let Some((sock, mine)) = o.prep.lock().unwrap().take() else { return };
                        let cipher = mine.cipher(&eph, session, true);
                        self.peer = o.name.clone();
                        let c = Client::start(sock, addrs, cipher, o.name, self.waker());
                        let (_, h, mbps, sharp) = PHONE_PRESETS[self.preset];
                        c.set_quality(h, mbps);
                        c.set_sharp(sharp);
                        self.cursor = (0.5, 0.5);
                        self.zoom = 1.0;
                        // Full screen (no status bar to pull down by accident) and keep the screen on.
                        self.window_flags(true);
                        self.client = Some(c);
                    }
                    Sig::ConnectNo { req, reason } if req == o.req => {
                        let name = self.outgoing.take().unwrap().name;
                        self.message = Some(format!("{name} {reason}"));
                    }
                    _ => {}
                }
            }
            Event::Toast(m) => self.message = Some(m),
            // Notifications (after the first seconds, when everyone online shows up at once).
            Event::FriendOnline(p) => {
                if let (true, Some(f)) = (self.started.elapsed() > Duration::from_secs(20), self.signal.as_ref().and_then(|s| s.friend(&p))) {
                    crate::mic::notify(notif_id(&p), &f.display(), &tl("is online"));
                }
            }
            Event::FriendRequest(name) => crate::mic::notify(1, &tl("Friend request"), &tlf("{} wants to be your friend", &[&name])),
            Event::Message { from, text } => {
                let who = self.signal.as_ref().and_then(|s| s.friend(&from)).map(|f| f.display()).unwrap_or_default();
                crate::mic::notify(notif_id(&from) ^ 0x5a5a, &who, &text);
            }
            Event::Cancelled { .. } => {}
            // The phone can't host: turn invite connections down.
            Event::GuestConnect { from, name, req, .. } => {
                if let Some(s) = &self.signal {
                    s.send_to(&name, &from, Sig::ConnectNo { req, reason: "is on a phone (can't be controlled)".into() });
                }
            }
            Event::JoinReady { .. } | Event::HelpRequest(_) | Event::Game { .. } => {}
        }
    }

    // ------------------------------------------------------------ trackpad

    fn send(&self, i: Input) {
        if let Some(c) = &self.client {
            c.input(i);
        }
    }

    fn click(&self, b: u8) {
        self.send(Input::Button { b, down: true });
        self.send(Input::Button { b, down: false });
    }

    fn touch(&mut self, id: u64, phase: TouchPhase, x: f32, y: f32) {
        let width = self.gfx.as_ref().and_then(|g| g.video_rect()).map(|r| r.2).unwrap_or(1000.0).max(100.0);
        let slop = SLOP_DP * self.ctx.pixels_per_point();
        let mut out: Vec<Input> = Vec::new();
        let g = &mut self.g;
        match phase {
            TouchPhase::Started => {
                if g.fingers.is_empty() {
                    *g = Gesture { start: Some(Instant::now()), ..Default::default() };
                }
                g.fingers.insert(id, (x, y));
                g.max_fingers = g.max_fingers.max(g.fingers.len());
                if g.fingers.len() == 2 {
                    let p: Vec<(f32, f32)> = g.fingers.values().copied().collect();
                    g.dist0 = ((p[0].0 - p[1].0).powi(2) + (p[0].1 - p[1].1).powi(2)).sqrt().max(1.0);
                    g.zoom0 = self.zoom;
                }
            }
            TouchPhase::Moved => {
                let Some(&(px, py)) = g.fingers.get(&id) else { return };
                let (dx, dy) = (x - px, y - py);
                g.fingers.insert(id, (x, y));
                g.travel += dx.abs() + dy.abs();
                let held = g.start.is_some_and(|s| s.elapsed() > HOLD);
                match g.fingers.len() {
                    1 if !g.pinch => {
                        // Hold still, then move = drag (select text, move windows).
                        if held && !g.dragging && g.travel < slop * 2.0 && g.max_fingers == 1 {
                            g.dragging = true;
                            out.push(Input::Button { b: 0, down: true });
                        }
                        // Zoomed in: finer movement.
                        let k = SPEED / width / self.zoom;
                        self.cursor.0 = (self.cursor.0 + dx * k).clamp(0.0, 1.0);
                        self.cursor.1 = (self.cursor.1 + dy * k * 16.0 / 9.0).clamp(0.0, 1.0);
                        out.push(Input::MouseAbs { x: self.cursor.0, y: self.cursor.1 });
                    }
                    2 => {
                        let p: Vec<(f32, f32)> = g.fingers.values().copied().collect();
                        let d = ((p[0].0 - p[1].0).powi(2) + (p[0].1 - p[1].1).powi(2)).sqrt();
                        // Fingers moving apart/together = zoom; moving together = scroll.
                        if g.pinch || (d / g.dist0 - 1.0).abs() > 0.15 {
                            g.pinch = true;
                            self.zoom = (g.zoom0 * d / g.dist0).clamp(1.0, 4.0);
                        } else {
                            g.wheel += dy * 0.5 * 3.0 / self.ctx.pixels_per_point();
                            let notches = g.wheel as i32;
                            if notches != 0 {
                                g.wheel -= notches as f32;
                                out.push(Input::Wheel { dx: 0, dy: notches });
                            }
                        }
                    }
                    _ => {}
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                g.fingers.remove(&id);
                if g.fingers.is_empty() {
                    let quick = g.start.is_some_and(|s| s.elapsed() < TAP);
                    if g.dragging {
                        out.push(Input::Button { b: 0, down: false });
                    } else if quick && !g.pinch && g.travel < slop * g.max_fingers as f32 && phase == TouchPhase::Ended {
                        // Click where our cursor is (the PC's mouse may be somewhere else).
                        out.push(Input::MouseAbs { x: self.cursor.0, y: self.cursor.1 });
                        let b = match g.max_fingers {
                            1 => 0,
                            2 => 1,
                            _ => 2,
                        };
                        out.push(Input::Button { b, down: true });
                        out.push(Input::Button { b, down: false });
                    }
                    self.g = Gesture::default();
                }
            }
        }
        for i in out {
            self.send(i);
        }
    }

    // ------------------------------------------------------------ keyboard

    fn key(&self, scan: u16, ext: bool) {
        self.send(Input::Key { scan, ext, down: true });
        self.send(Input::Key { scan, ext, down: false });
    }

    fn combo(&self, keys: &[(u16, bool)]) {
        for &(scan, ext) in keys {
            self.send(Input::Key { scan, ext, down: true });
        }
        for &(scan, ext) in keys.iter().rev() {
            self.send(Input::Key { scan, ext, down: false });
        }
    }

    /// Phone keyboard -> PC keys (US layout scan codes).
    fn phone_key(&self, key: &Key) {
        match key {
            Key::Named(n) => {
                let k = match n {
                    NamedKey::Enter => (0x1C, false),
                    NamedKey::Backspace => (0x0E, false),
                    NamedKey::Tab => (0x0F, false),
                    NamedKey::Escape => (0x01, false),
                    NamedKey::Space => (0x39, false),
                    NamedKey::Delete => (0x53, true),
                    NamedKey::ArrowUp => (0x48, true),
                    NamedKey::ArrowDown => (0x50, true),
                    NamedKey::ArrowLeft => (0x4B, true),
                    NamedKey::ArrowRight => (0x4D, true),
                    _ => return,
                };
                self.key(k.0, k.1);
            }
            Key::Character(s) => {
                for c in s.chars() {
                    if let Some((scan, shift)) = us_scan(c) {
                        if shift {
                            self.combo(&[(0x2A, false), (scan, false)]);
                        } else {
                            self.key(scan, false);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ screens

    fn draw(&mut self) {
        let Some(window) = self.gfx.as_ref().map(|g| g.window.clone()) else { return };
        let streaming = self.client.is_some();
        if let (Some(c), Some(g)) = (&self.client, &mut self.gfx) {
            if let Some(f) = c.take_frame() {
                g.upload(&f);
            }
        }
        if let Some(c) = &self.client {
            if let State::Failed(e) = c.state() {
                self.disconnect(Some(e));
            }
        }
        let Some(state) = &mut self.egui_state else { return };
        let raw = state.take_egui_input(&window);
        let ctx = self.ctx.clone();
        let out = ctx.run_ui(raw, |ui| {
            if self.signal.is_none() {
                self.setup_ui(ui)
            } else if self.client.is_some() {
                self.stream_ui(ui.ctx())
            } else {
                self.home_ui(ui)
            }
        });
        // Text boxes need the phone keyboard (winit doesn't open it on Android by itself).
        if !streaming {
            let want = ctx.egui_wants_keyboard_input();
            if want != self.keyboard {
                self.set_keyboard(want);
            }
        }
        if let Some(state) = &mut self.egui_state {
            state.handle_platform_output(&window, out.platform_output.clone());
        }
        if let Some(v) = out.viewport_output.get(&egui::ViewportId::ROOT) {
            if v.repaint_delay < Duration::from_millis(500) {
                window.request_redraw();
            }
        }
        let streaming = self.client.is_some();
        if let Some(g) = &mut self.gfx {
            g.render(streaming, out, &ctx);
        }
    }

    fn setup_ui(&mut self, root: &mut egui::Ui) {
        use crate::theme as t;
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t::BG()).inner_margin(egui::Margin::same(20))).show(root, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(20.0);
                ui.horizontal(|ui| language_switch(ui));
                ui.add_space(40.0);
                let (r, _) = ui.allocate_exact_size(egui::vec2(84.0, 84.0), egui::Sense::hover());
                t::logo(ui.painter(), r, true);
                ui.add_space(8.0);
                ui.label(RichText::new(tr("Welcome to Kinlink")).font(egui::FontId::new(26.0, t::bold())));
                ui.label(RichText::new(tr("Pick a username. Friends add you with it.")).color(t::SOFT()));
                ui.add_space(14.0);
                ui.add(egui::TextEdit::singleline(&mut self.name_input).hint_text(tr("username")).desired_width(280.0).margin(egui::vec2(14.0, 12.0)).font(egui::FontId::new(17.0, t::bold())));
                let name = self.name_input.trim().to_string();
                let ok = crypto::valid_new_name(&name);
                ui.add_space(6.0);
                if t::primary(ui, &tr("Continue"), None, ok).clicked() {
                    let id = Identity::new(&name);
                    if id.save() {
                        self.start_signal(id);
                    } else {
                        self.message = Some(tl("Couldn't save your identity"));
                    }
                }
                ui.label(RichText::new(tr("2–20 letters or numbers")).size(12.0).color(t::MUTED()));
                if let Some(m) = &self.message {
                    ui.colored_label(t::DANGER(), m);
                }
                ui.add_space(16.0);
                t::para(ui, &tl("By continuing you agree to the Terms of Use and Privacy Policy
(github.com/IamAbood/Kinlink). Only connect to PCs you own
or have permission to use."), 11.5, t::MUTED());
                ui.add_space(8.0);
                ui.label(RichText::new(tr("Made by Abdulkareem Alsaou")).size(12.0).color(t::MUTED()));
            });
        });
    }

    fn home_ui(&mut self, root: &mut egui::Ui) {
        use crate::theme::{self as t, Icon};
        let Some(sig) = self.signal.clone() else { return };
        let mut connect: Option<(Pub, String)> = None;
        let mut cancel = false;
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t::BG()).inner_margin(egui::Margin::symmetric(16, 10))).show(root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 12.0;
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(36.0, 36.0), egui::Sense::hover());
                    t::logo(ui.painter(), r, true);
                    ui.label(RichText::new(tr("Kinlink")).font(egui::FontId::new(20.0, t::bold())));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| language_switch(ui));
                });
                t::card().fill(t::FIELD()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    t::section(ui, &tr("Your ID"));
                    ui.horizontal(|ui| {
                        let id = sig.id();
                        let (name, tag) = id.split_once('#').unwrap_or((&id, ""));
                        ui.spacing_mut().item_spacing.x = 0.0;
                        ui.label(RichText::new(name).font(egui::FontId::new(17.0, t::bold())));
                        ui.label(RichText::new(format!("#{tag}")).font(egui::FontId::new(17.0, t::bold())).color(t::ACCENT_SOFT()));
                    });
                    ui.label(RichText::new(if sig.online() { "Online" } else { "Connecting…" }).size(12.0).color(if sig.online() { t::MINT_SOFT() } else { t::MUTED() }));
                    ui.label(RichText::new(concat!("v", env!("CARGO_PKG_VERSION"))).size(11.0).color(t::MUTED()));
                });
                let newer = self.update.lock().unwrap().clone();
                if let Some(tag) = newer {
                    egui::Frame::NONE.fill(egui::Color32::from_rgb(0x10, 0x26, 0x1E)).stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(0x1F, 0x4D, 0x3B))).corner_radius(12.0).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(trf("Kinlink {} is available", &[&tag])).font(egui::FontId::new(14.0, t::bold())).color(t::MINT_SOFT()));
                        const APK: &str = "https://github.com/IamAbood/Kinlink/releases/latest/download/Kinlink.apk";
                        let note = self.dl_note.lock().unwrap().clone();
                        if t::primary(ui, &tr("Download"), Some(Icon::Update), note.is_none()).clicked() {
                            let (slot, wake) = (self.dl_note.clone(), self.waker());
                            *slot.lock().unwrap() = Some(tl("Downloading… the installer opens when it's done."));
                            let report = move |m: String| {
                                *slot.lock().unwrap() = if m.is_empty() { None } else { Some(m) };
                                wake();
                            };
                            if let Err(e) = crate::mic::download_and_install(APK, &format!("Kinlink-{tag}.apk"), report) {
                                *self.dl_note.lock().unwrap() = None;
                                if let Err(e2) = crate::mic::open_url(APK) {
                                    self.message = Some(tlf("Couldn't open the browser ({}). Open this link yourself:", &[&format!("{e}; {e2}")]));
                                }
                            }
                        }
                        if let Some(n) = note {
                            ui.label(RichText::new(crate::rtl::show(&n)).size(12.0).color(t::SOFT()));
                        }
                        ui.label(RichText::new("github.com/IamAbood/Kinlink/releases").size(11.5).color(t::SOFT()));
                    });
                }
                if let Some(m) = &self.message {
                    ui.colored_label(egui::Color32::from_rgb(0xFF, 0xC9, 0x8A), crate::rtl::show(m));
                }
                if let Some(o) = &self.outgoing {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(trf("Waiting for {} to accept…", &[&o.name]));
                        if ui.button(tr("Cancel")).clicked() {
                            cancel = true;
                        }
                    });
                }
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.add_input).hint_text(tr("Add a friend: name#1234-ABCDEF")).desired_width(ui.available_width() - 90.0).margin(egui::vec2(12.0, 10.0)));
                    if t::primary(ui, &tr("Add"), Some(Icon::Plus), true).clicked() {
                        self.message = Some(match sig.add_friend(self.add_input.trim()) {
                            Ok(()) => tlf("Request sent to {}", &[self.add_input.trim()]),
                            Err(e) => e,
                        });
                        self.add_input.clear();
                    }
                });
                for (name, public) in sig.incoming() {
                    egui::Frame::NONE.fill(t::ACCENT_BG()).stroke(egui::Stroke::new(1.0, t::ACCENT_LINE())).corner_radius(12.0).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(trf("{} wants to be friends", &[&name])).font(egui::FontId::new(14.0, t::bold())));
                        t::para(ui, &tlf("Safety code {}: must match on their screen", &[&sig.safety_code(&public)]), 12.0, t::SOFT());
                        ui.horizontal(|ui| {
                            if t::primary(ui, &tr("Accept"), None, true).clicked() {
                                sig.accept(public);
                            }
                            if ui.button(tr("Decline")).clicked() {
                                sig.decline(public);
                            }
                        });
                    });
                }
                for o in sig.outgoing_info() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(trf("Request sent to {}", &[&o.id])).color(t::MUTED()));
                        if ui.small_button(tr("Cancel")).clicked() {
                            sig.cancel_request(&o.id);
                        }
                    });
                    if let Some(code) = &o.code {
                        ui.label(RichText::new(trf("Safety code {}", &[code])).size(12.0).color(t::SOFT()));
                    }
                    if o.conflict {
                        t::para(ui, &tl("Two different PCs answered for this ID: check the safety code."), 13.0, t::DANGER());
                    }
                }
                let mut friends = sig.friends();
                friends.sort_by_key(|(f, on)| (!on, f.display().to_lowercase()));
                t::section(ui, &trf("Friends · {} online", &[&friends.iter().filter(|(_, o)| *o).count().to_string()]));
                if friends.is_empty() {
                    t::para(ui, &tl("No friends yet. Add your PC's name#tag above, then accept the request on the PC."), 14.0, t::SOFT());
                }
                for (f, online) in friends {
                    t::card().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            t::avatar(ui, &f.display(), Some(online), 42.0);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 1.0;
                                ui.label(RichText::new(crate::rtl::show(&f.display())).font(egui::FontId::new(15.0, t::bold())));
                                ui.label(RichText::new(if online { "Online" } else { "Offline" }).size(12.0).color(if online { t::MINT_SOFT() } else { t::MUTED() }));
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let busy = self.outgoing.is_some();
                                if online && t::primary(ui, &tr("Connect"), Some(Icon::Play), !busy).clicked() {
                                    connect = Some((f.public, f.id()));
                                }
                            });
                        });
                    });
                }
                t::para(ui, &tl("In a session: drag one finger to move the mouse, tap to click, two-finger tap to right-click, two fingers to scroll, hold then drag to drag."), 12.0, t::MUTED());
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(tr("Made by Abdulkareem Alsaou")).size(11.0).color(t::MUTED()));
                });
            });
        });
        if cancel {
            self.cancel_outgoing();
        }
        if let Some((p, n)) = connect {
            self.connect_to(p, n);
        }
        // A host that never answers.
        if self.outgoing.as_ref().is_some_and(|o| o.at.elapsed() > Duration::from_secs(60)) {
            self.cancel_outgoing();
            self.message = Some(tl("No answer. Is the PC on and running Kinlink?"));
        }
    }

    fn stream_ui(&mut self, ctx: &egui::Context) {
        use crate::theme::{self as t, Icon};
        let Some(c) = &self.client else { return };
        let state = c.state();
        let stats = c.stats();
        if self.pad_mode {
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("pad")));
            self.pad.draw(&painter, ctx.content_rect());
        }
        // Zoomed in: keep the cursor in view.
        if let Some(g) = &mut self.gfx {
            g.zoom = self.zoom;
            g.focus = self.cursor;
        }
        // Remote cursor where the trackpad puts it (through the zoom window).
        if let (false, State::Connected { .. }, Some((x, y, w, h))) = (self.pad_mode, &state, self.gfx.as_ref().and_then(|g| g.video_rect())) {
            let ppp = ctx.pixels_per_point();
            let (ox, oy, vs) = self.gfx.as_ref().map(|g| g.view_window()).unwrap_or((0.0, 0.0, 1.0));
            let (cx, cy) = ((self.cursor.0 - ox) / vs, (self.cursor.1 - oy) / vs);
            let p = egui::pos2((x + cx * w) / ppp, (y + cy * h) / ppp);
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("cursor")));
            let tip = [p, p + egui::vec2(0.0, 18.0), p + egui::vec2(5.0, 14.0), p + egui::vec2(12.0, 13.0)];
            painter.add(egui::Shape::convex_polygon(tip.to_vec(), egui::Color32::WHITE, egui::Stroke::new(1.5, egui::Color32::BLACK)));
        }
        if let State::Connected { mouse, keyboard, .. } = &state {
            if !*mouse || !*keyboard {
                let what = match (*mouse, *keyboard) {
                    (false, false) => tl("Mouse and keyboard are turned off for you on that PC. Ask its owner to allow them in Friends."),
                    (false, true) => tl("Mouse is turned off for you on that PC. Ask its owner to allow it in Friends."),
                    _ => tl("Keyboard is turned off for you on that PC. Ask its owner to allow it in Friends."),
                };
                egui::Area::new(egui::Id::new("perm_note")).anchor(egui::Align2::CENTER_BOTTOM, [0.0, -16.0]).show(ctx, |ui| {
                    egui::Frame::NONE.fill(egui::Color32::from_rgba_unmultiplied(58, 20, 38, 235)).corner_radius(12.0).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                        ui.set_max_width(320.0);
                        t::para(ui, &what, 12.5, t::DANGER());
                    });
                });
            }
        }
        if self.zoom > 1.01 && !self.pad_mode {
            egui::Area::new(egui::Id::new("zoom_note")).anchor(egui::Align2::RIGHT_TOP, [-14.0, 30.0]).show(ctx, |ui| {
                egui::Frame::NONE.fill(egui::Color32::from_black_alpha(150)).corner_radius(10.0).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(format!("{:.1}×", self.zoom)).size(12.0).color(egui::Color32::WHITE)).extend());
                });
            });
        }
        if matches!(state, State::Connecting) {
            egui::Area::new(egui::Id::new("connecting")).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                t::card().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(trf("Connecting to {}…", &[&self.peer]));
                    });
                });
            });
        }
        let mut act: Option<&str> = None;
        egui::Area::new(egui::Id::new("menu")).anchor(egui::Align2::CENTER_TOP, [0.0, 30.0]).show(ctx, |ui| {
            if !self.menu {
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(52.0, 36.0), egui::Sense::click());
                let p = ui.painter();
                p.rect(rect, 12.0, egui::Color32::from_rgba_unmultiplied(11, 16, 32, 210), egui::Stroke::new(1.0, t::ACCENT_LINE()), egui::StrokeKind::Inside);
                t::icon(p, egui::Rect::from_center_size(rect.center(), egui::vec2(20.0, 20.0)), Icon::Menu, t::ACCENT_SOFT());
                if resp.clicked() {
                    act = Some("menu");
                }
                return;
            }
            egui::Frame::NONE.fill(egui::Color32::from_rgba_unmultiplied(15, 22, 41, 246)).stroke(egui::Stroke::new(1.0, t::LINE2())).corner_radius(16.0).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
                ui.set_width(320.0);
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 10.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(crate::rtl::show(&self.peer)).font(egui::FontId::new(14.5, t::bold())));
                    ui.label(RichText::new(format!("· {:.0} ms", stats.rtt_ms)).color(t::MUTED()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if t::icon_button(ui, Icon::Close, &tr("Close"), false).clicked() {
                            act = Some("menu");
                        }
                    });
                });
                let w = (320.0 - 6.0) / 2.0;
                let mic = c.mic_on();
                ui.horizontal(|ui| {
                    if t::tile(ui, Icon::Keyboard, &if self.keyboard { tr("Hide keyboard") } else { tr("Keyboard") }, self.keyboard.then_some(t::ACCENT_SOFT()), w).clicked() {
                        act = Some("kb");
                    }
                    if t::tile(ui, Icon::Mic, &if mic { tr("Mic on") } else { tr("Mic off") }, mic.then_some(t::MINT()), w).clicked() {
                        act = Some("mic");
                    }
                });
                ui.horizontal(|ui| {
                    if t::tile(ui, Icon::Hide, &if self.muted { tr("Sound off") } else { tr("Sound on") }, (!self.muted).then_some(t::ACCENT_SOFT()), w).clicked() {
                        act = Some("sound");
                    }
                    if t::tile(ui, Icon::Close, &tr("Disconnect"), Some(t::DANGER()), w).clicked() {
                        act = Some("bye");
                    }
                });
                // Picture quality presets.
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (i, (name, ..)) in PHONE_PRESETS.iter().enumerate() {
                        if ui.add_sized([(320.0 - 12.0) / 4.0, 30.0], egui::Button::selectable(self.preset == i, RichText::new(tr(name)).size(12.0))).clicked() && self.preset != i {
                            self.preset = i;
                            let (_, h, mbps, sharp) = PHONE_PRESETS[i];
                            c.set_quality(h, mbps);
                            c.set_sharp(sharp);
                        }
                    }
                });
                ui.horizontal(|ui| {
                    if t::tile(ui, Icon::Gamepad, &if self.pad_mode { tr("Trackpad") } else { tr("Controller") }, self.pad_mode.then_some(t::MINT()), 320.0).clicked() {
                        act = Some("pad");
                    }
                });
                let (talking, mic_err) = c.voice_status();
                if !talking.is_empty() {
                    ui.label(RichText::new(trf("Talking: {}", &[&talking.join(", ")])).color(t::MINT_SOFT()));
                }
                if let Some(e) = mic_err {
                    ui.colored_label(t::DANGER(), e);
                }
                let kw = (320.0 - 3.0 * 6.0) / 4.0;
                for row in [["Esc", "Tab", "Enter", "Back"], ["Win", "Alt+Tab", "Copy", "Paste"], ["Left", "Up", "Down", "Right"]] {
                    ui.horizontal(|ui| {
                        for k in row {
                            if ui.add(egui::Button::new(RichText::new(k).font(egui::FontId::new(13.0, t::bold()))).min_size(egui::vec2(kw, 40.0))).clicked() {
                                act = Some(k);
                            }
                        }
                    });
                }
                egui::Frame::NONE.fill(t::BG()).corner_radius(9.0).inner_margin(egui::Margin::symmetric(10, 7)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(format!("{:.0} fps · {:.0} ms · {:.1} Mbps", stats.fps, stats.rtt_ms, stats.mbps)).monospace().size(11.0).color(t::MUTED()));
                });
            });
        });
        match act {
            Some("menu") => self.menu = !self.menu,
            Some("kb") => self.set_keyboard(!self.keyboard),
            Some("pad") => {
                self.pad_mode = !self.pad_mode;
                self.menu = false;
                self.pad.release();
                if !self.pad_mode {
                    self.send(Input::PadRemove { id: 0 });
                }
            }
            Some("mic") => {
                if let Some(c) = &self.client {
                    if c.mic_on() {
                        c.set_mic(false);
                    } else if crate::mic::ensure_mic() {
                        c.set_mic(true); // use headphones, or the PC hears itself
                    } else {
                        self.message = Some(tl("Allow the microphone, then tap Mic again"));
                    }
                }
            }
            Some("sound") => {
                self.muted = !self.muted;
                if let Some(c) = &self.client {
                    c.set_volume(if self.muted { 0 } else { 100 });
                }
            }
            Some("bye") => self.disconnect(None),
            Some("Esc") => self.key(0x01, false),
            Some("Tab") => self.key(0x0F, false),
            Some("Enter") => self.key(0x1C, false),
            Some("Back") => self.key(0x0E, false),
            Some("Win") => self.key(0x5B, true),
            Some("Alt+Tab") => self.combo(&[(0x38, false), (0x0F, false)]),
            Some("Copy") => self.combo(&[(0x1D, false), (0x2E, false)]),
            Some("Paste") => self.combo(&[(0x1D, false), (0x2F, false)]),
            Some("Left") => self.key(0x4B, true),
            Some("Right") => self.key(0x4D, true),
            Some("Up") => self.key(0x48, true),
            Some("Down") => self.key(0x50, true),
            _ => {}
        }
    }
}

/// Character -> (US scan code, needs shift).
fn us_scan(c: char) -> Option<(u16, bool)> {
    const ROWS: [(&str, &str, u16); 4] = [
        ("1234567890-=", "!@#$%^&*()_+", 0x02),
        ("qwertyuiop[]", "QWERTYUIOP{}", 0x10),
        ("asdfghjkl;'`", "ASDFGHJKL:\"~", 0x1E),
        ("\\zxcvbnm,./", "|ZXCVBNM<>?", 0x2B),
    ];
    if c == ' ' {
        return Some((0x39, false));
    }
    if c == '\n' {
        return Some((0x1C, false));
    }
    for (lo, hi, base) in ROWS {
        if let Some(i) = lo.chars().position(|x| x == c) {
            return Some((base + i as u16, false));
        }
        if let Some(i) = hi.chars().position(|x| x == c) {
            return Some((base + i as u16, true));
        }
    }
    None
}

impl ApplicationHandler<()> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        // Android destroys the surface when the app goes to the background; rebuild it.
        let window = Arc::new(el.create_window(Window::default_attributes().with_title("Kinlink")).expect("window"));
        match Gfx::new(window.clone()) {
            Ok(g) => self.gfx = Some(g),
            Err(e) => {
                log::error!("graphics init failed: {e:#}");
                el.exit();
                return;
            }
        }
        self.egui_state = Some(egui_winit::State::new(self.ctx.clone(), egui::ViewportId::ROOT, &window, Some(window.scale_factor() as f32), None, None));
        window.request_redraw();
    }

    fn suspended(&mut self, _el: &ActiveEventLoop) {
        self.gfx = None;
        self.egui_state = None;
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        let streaming = self.client.is_some();
        match &ev {
            WindowEvent::Resized(s) => {
                if let Some(g) = &mut self.gfx {
                    g.resize(s.width, s.height);
                }
            }
            WindowEvent::RedrawRequested => {
                self.draw();
                return;
            }
            WindowEvent::Touch(t) if streaming => {
                // A finger that lands on the menu belongs to egui; anywhere else is the trackpad.
                let ppp = self.ctx.pixels_per_point();
                let pos = egui::pos2(t.location.x as f32 / ppp, t.location.y as f32 / ppp);
                let free = t.phase == TouchPhase::Started && self.ctx.layer_id_at(pos).is_none_or(|l| l.order == egui::Order::Background);
                if self.pad_mode && (free || self.pad.owns(t.id)) {
                    let screen = self.ctx.content_rect();
                    if let Some(st) = self.pad.touch(t.id, t.phase, pos, screen) {
                        self.send(Input::Pad { id: 0, state: st });
                    }
                    self.redraw();
                    return;
                }
                let mine = self.g.fingers.contains_key(&t.id) || free;
                if mine {
                    self.touch(t.id, t.phase, t.location.x as f32, t.location.y as f32);
                    self.redraw();
                    return;
                }
            }
            WindowEvent::KeyboardInput { event, .. } if streaming => {
                if event.state == ElementState::Pressed {
                    // Android Back = leave the stream.
                    if event.logical_key == Key::Named(NamedKey::GoBack) || event.logical_key == Key::Named(NamedKey::BrowserBack) {
                        self.disconnect(None);
                    } else {
                        self.phone_key(&event.logical_key);
                    }
                }
                self.redraw();
                return;
            }
            _ => {}
        }
        if let (Some(st), Some(w)) = (&mut self.egui_state, self.gfx.as_ref().map(|g| g.window.clone())) {
            if st.on_window_event(&w, &ev).repaint {
                w.request_redraw();
            }
        }
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _ev: ()) {
        self.redraw();
    }

    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        let events = self.signal.as_ref().map(|s| s.take_events()).unwrap_or_default();
        let any = !events.is_empty();
        for e in events {
            self.on_event(e);
        }
        if any || self.client.is_some() || self.outgoing.is_some() {
            self.redraw();
        }
    }
}

fn lang_file() -> Option<std::path::PathBuf> {
    Some(crate::settings::data_dir()?.join("lang.txt"))
}

/// Applies the saved language (call once at start).
pub fn load_language() {
    let saved = lang_file().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    crate::i18n::set_arabic(saved.trim() == "ar");
}

/// English / العربية toggle.
fn language_switch(ui: &mut egui::Ui) {
    let ar = crate::i18n::arabic();
    let mut pick = None;
    if ui.selectable_label(ar, crate::rtl::show("العربية")).clicked() && !ar {
        pick = Some(true);
    }
    if ui.selectable_label(!ar, "English").clicked() && ar {
        pick = Some(false);
    }
    if let Some(on) = pick {
        crate::i18n::set_arabic(on);
        if let Some(p) = lang_file() {
            let _ = std::fs::write(p, if on { "ar" } else { "en" });
        }
    }
}

/// Asks GitHub for the newest release; fills `slot` if it's newer than this app.
fn check_for_update(slot: Arc<Mutex<Option<String>>>, wake: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let Ok(mut resp) = ureq::get("https://api.github.com/repos/IamAbood/Kinlink/releases/latest").header("User-Agent", "Kinlink-Android").call() else { return };
        let Ok(body) = resp.body_mut().read_to_string() else { return };
        let Ok(j) = serde_json::from_str::<serde_json::Value>(&body) else { return };
        let Some(tag) = j["tag_name"].as_str() else { return };
        // The APK is attached a few minutes after the release appears; don't offer a 404.
        let has_apk = j["assets"].as_array().is_some_and(|a| a.iter().any(|x| x["name"] == "Kinlink.apk"));
        if !has_apk {
            return;
        }
        if version_key(tag) > version_key(env!("CARGO_PKG_VERSION")) {
            *slot.lock().unwrap() = Some(tag.trim_start_matches('v').to_string());
            wake();
        }
    });
}

/// "v0.17.0-beta" -> [0, 17, 0]
fn version_key(v: &str) -> Vec<u32> {
    v.trim_start_matches('v').split(['-', '+']).next().unwrap_or("").split('.').map(|n| n.parse().unwrap_or(0)).collect()
}
