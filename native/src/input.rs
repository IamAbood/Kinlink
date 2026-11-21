//! Host side: turns remote Input messages into real mouse/keyboard/gamepad events.
//! Tracks what each guest is holding so one guest leaving never leaves keys stuck for others,
//! gives every guest's controllers their own virtual pads (Xbox 360 or DualShock 4), forwards
//! rumble back, and lets the host reorder player slots.
use crate::proto::{Input, PadState, PAD_PLAYSTATION};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use vigem_client::{Client as Vigem, DS4Report, DualShock4Wired, TargetId, XButtons, XGamepad, Xbox360Wired};
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN};

/// Guest id = session id.
pub type Guest = u64;
/// (guest, pad id, large motor, small motor)
pub type Rumble = (Guest, u8, u8, u8);

#[derive(Default)]
struct Held {
    keys: HashSet<(u16, bool)>,
    buttons: HashSet<u8>,
}

enum Target {
    Xbox(Xbox360Wired<Arc<Vigem>>),
    Ps(DualShock4Wired<Arc<Vigem>>),
}

struct Pad {
    target: Target,
    kind: u8,
    last: PadState,
    /// Player LED reported by the virtual pad (0-3 = Player 1-4; 255 = unknown).
    led: Arc<AtomicU8>,
}

/// A virtual controller as the host UI shows it.
pub struct PadInfo {
    pub guest: Guest,
    pub id: u8,
    pub playstation: bool,
    /// XInput player number (1-4) for Xbox pads.
    pub player: Option<u32>,
}

pub struct Injector {
    monitor: RECT,
    held: HashMap<Guest, Held>,
    pads: HashMap<(Guest, u8), Pad>,
    /// Plug-in order = player order.
    order: Vec<(Guest, u8)>,
    vigem: Option<Result<Arc<Vigem>, String>>,
    pub rumble_tx: Option<Sender<Rumble>>,
    /// Guests whose pads must be ignored (same-PC tests: a virtual pad would loop back).
    pub no_pads: HashSet<Guest>,
    /// Test switch (PCLONE_DRY_INPUT=1): log instead of touching the real devices.
    dry: bool,
    /// Lock-screen helper (SYSTEM): when connected, mouse/keyboard go through it so they also
    /// reach elevated windows, UAC prompts and the lock screen.
    pub helper: Option<std::sync::Arc<std::sync::Mutex<Option<crate::syshelper::Link>>>>,
}

impl Default for Injector {
    fn default() -> Self {
        Self {
            monitor: RECT::default(),
            held: HashMap::new(),
            pads: HashMap::new(),
            order: Vec::new(),
            vigem: None,
            rumble_tx: None,
            no_pads: HashSet::new(),
            dry: std::env::var_os("PCLONE_DRY_INPUT").is_some(),
            helper: None,
        }
    }
}

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: crate::kbhook::OUR_INPUT } },
    }
}

fn key(scan: u16, ext: bool, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if ext {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: crate::kbhook::OUR_INPUT } },
    }
}

fn button_flags(b: u8, down: bool) -> (MOUSE_EVENT_FLAGS, i32) {
    match (b, down) {
        (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (0, false) => (MOUSEEVENTF_LEFTUP, 0),
        (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (x, true) => (MOUSEEVENTF_XDOWN, if x == 3 { 1 } else { 2 }),
        (x, false) => (MOUSEEVENTF_XUP, if x == 3 { 1 } else { 2 }),
    }
}

/// XInput-style state -> DualShock 4 report (face buttons by position, dpad as hat).
pub fn ds4_report(s: &PadState) -> DS4Report {
    let has = |m: u16| s.buttons & m != 0;
    let (up, down, left, right) = (has(0x1), has(0x2), has(0x4), has(0x8));
    let hat: u16 = match (up, right, down, left) {
        (true, false, false, false) => 0,
        (true, true, false, false) => 1,
        (false, true, false, false) => 2,
        (false, true, true, false) => 3,
        (false, false, true, false) => 4,
        (false, false, true, true) => 5,
        (false, false, false, true) => 6,
        (true, false, false, true) => 7,
        _ => 8,
    };
    let map: [(u16, u16); 10] = [
        (0x1000, 1 << 5),  // A -> Cross
        (0x2000, 1 << 6),  // B -> Circle
        (0x4000, 1 << 4),  // X -> Square
        (0x8000, 1 << 7),  // Y -> Triangle
        (0x0100, 1 << 8),  // LB -> L1
        (0x0200, 1 << 9),  // RB -> R1
        (0x0020, 1 << 12), // Back -> Share
        (0x0010, 1 << 13), // Start -> Options
        (0x0040, 1 << 14), // LS click -> L3
        (0x0080, 1 << 15), // RS click -> R3
    ];
    let mut buttons = hat;
    for (x, d) in map {
        if has(x) {
            buttons |= d;
        }
    }
    if s.lt > 30 {
        buttons |= 1 << 10;
    }
    if s.rt > 30 {
        buttons |= 1 << 11;
    }
    let axis = |v: i16| ((v as i32 + 32768) >> 8) as u8;
    DS4Report {
        thumb_lx: axis(s.lx),
        thumb_ly: 255 - axis(s.ly), // DS4: 0 = up
        thumb_rx: axis(s.rx),
        thumb_ry: 255 - axis(s.ry),
        buttons,
        special: if has(0x0400) { 1 } else { 0 }, // Guide -> PS
        trigger_l: s.lt,
        trigger_r: s.rt,
    }
}

impl Injector {
    pub fn set_monitor(&mut self, r: RECT) {
        self.monitor = r;
    }

    pub fn apply(&mut self, g: Guest, i: Input) {
        if self.dry && !matches!(i, Input::Pad { .. }) {
            crate::plog!("INPUT g{} {i:?}", g % 1000);
        }
        match i {
            Input::MouseAbs { x, y } => unsafe {
                let r = self.monitor;
                let px = r.left as f32 + x.clamp(0.0, 1.0) * (r.right - r.left - 1) as f32;
                let py = r.top as f32 + y.clamp(0.0, 1.0) * (r.bottom - r.top - 1) as f32;
                let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
                let (vw, vh) = (GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2), GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2));
                let ax = ((px - vx as f32) * 65535.0 / (vw - 1) as f32).round() as i32;
                let ay = ((py - vy as f32) * 65535.0 / (vh - 1) as f32).round() as i32;
                self.out(&[mouse(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
            },
            Input::MouseRel { dx, dy } => self.out(&[mouse(dx, dy, 0, MOUSEEVENTF_MOVE)]),
            Input::Button { b, down } => {
                let h = self.held.entry(g).or_default();
                if down { h.buttons.insert(b) } else { h.buttons.remove(&b) };
                let (f, d) = button_flags(b, down);
                self.out(&[mouse(0, 0, d, f)]);
            }
            Input::Wheel { dx, dy } => {
                if dy != 0 {
                    self.out(&[mouse(0, 0, dy, MOUSEEVENTF_WHEEL)]);
                }
                if dx != 0 {
                    self.out(&[mouse(0, 0, dx, MOUSEEVENTF_HWHEEL)]);
                }
            }
            Input::Key { scan, ext, down } => {
                let h = self.held.entry(g).or_default();
                if down { h.keys.insert((scan, ext)) } else { h.keys.remove(&(scan, ext)) };
                self.out(&[key(scan, ext, down)]);
            }
            Input::Pad { id, state } => self.pad(g, id, state),
            Input::PadRemove { id } => {
                self.pads.remove(&(g, id));
                self.order.retain(|k| *k != (g, id));
            }
            Input::ReleaseAll => self.release(g),
        }
    }

    fn out(&self, ins: &[INPUT]) {
        if self.dry || ins.is_empty() {
            return;
        }
        if let Some(h) = &self.helper {
            let mut link = h.lock().unwrap();
            if let Some(l) = link.as_mut() {
                use crate::syshelper::RawIn;
                let raw: Vec<RawIn> = ins
                    .iter()
                    .map(|i| unsafe {
                        if i.r#type == INPUT_MOUSE {
                            let m = i.Anonymous.mi;
                            RawIn::Mouse { dx: m.dx, dy: m.dy, data: m.mouseData as i32, flags: m.dwFlags.0 }
                        } else {
                            let k = i.Anonymous.ki;
                            RawIn::Key { scan: k.wScan, flags: k.dwFlags.0 }
                        }
                    })
                    .collect();
                if l.input(raw).is_ok() {
                    return;
                }
                *link = None; // helper gone: fall back to normal input
            }
        }
        send(ins);
    }

    /// Lifts everything one guest is holding and unplugs their virtual pads.
    pub fn release(&mut self, g: Guest) {
        if let Some(h) = self.held.remove(&g) {
            let mut ins: Vec<INPUT> = h.keys.into_iter().map(|(s, e)| key(s, e, false)).collect();
            ins.extend(h.buttons.into_iter().map(|b| {
                let (f, d) = button_flags(b, false);
                mouse(0, 0, d, f)
            }));
            self.out(&ins);
        }
        self.drop_pads(g);
        self.no_pads.remove(&g);
    }

    /// Unplugs a guest's controllers (left, or controller permission turned off).
    pub fn drop_pads(&mut self, g: Guest) {
        self.pads.retain(|(owner, _), _| *owner != g);
        self.order.retain(|(owner, _)| *owner != g);
    }

    pub fn release_everyone(&mut self) {
        let guests: Vec<Guest> = self.held.keys().copied().collect();
        for g in guests {
            self.release(g);
        }
        self.pads.clear();
        self.order.clear();
    }

    fn vigem(&mut self) -> Option<Arc<Vigem>> {
        self.vigem.get_or_insert_with(|| Vigem::connect().map(Arc::new).map_err(|e| e.to_string())).as_ref().ok().cloned()
    }

    /// Creates and plugs in a virtual pad; Xbox pads get a rumble listener.
    fn plug(&mut self, g: Guest, id: u8, kind: u8, led: &Arc<AtomicU8>) -> Option<Target> {
        let v = self.vigem()?;
        if kind == PAD_PLAYSTATION {
            let mut t = DualShock4Wired::new(v, TargetId::DUALSHOCK4_WIRED);
            t.plugin().ok()?;
            t.wait_ready().ok()?;
            if let (Some(tx), Ok(n)) = (self.rumble_tx.clone(), t.request_notification()) {
                // Ends by itself when the pad is unplugged.
                n.spawn_thread(move |x| {
                    let _ = tx.send((g, id, x.large_motor, x.small_motor));
                });
            }
            return Some(Target::Ps(t));
        }
        let mut t = Xbox360Wired::new(v, TargetId::XBOX360_WIRED);
        if let Err(e) = t.plugin() {
            crate::plog!("pad plugin failed: {e:?}");
            return None;
        }
        if let Err(e) = t.wait_ready() {
            crate::plog!("pad wait_ready failed: {e:?}");
            return None;
        }
        if let (Some(tx), Ok(n)) = (self.rumble_tx.clone(), t.request_notification()) {
            let led = led.clone();
            // Ends by itself when the pad is unplugged. Notifications also carry the player LED.
            n.spawn_thread(move |_, x| {
                led.store(x.led_number, Relaxed);
                let _ = tx.send((g, id, x.large_motor, x.small_motor));
            });
        }
        Some(Target::Xbox(t))
    }

    fn push(target: &mut Target, s: &PadState) {
        let _ = match target {
            Target::Xbox(t) => t.update(&XGamepad {
                buttons: XButtons(s.buttons),
                left_trigger: s.lt,
                right_trigger: s.rt,
                thumb_lx: s.lx,
                thumb_ly: s.ly,
                thumb_rx: s.rx,
                thumb_ry: s.ry,
            }),
            Target::Ps(t) => t.update(&ds4_report(s)),
        };
    }

    fn pad(&mut self, g: Guest, id: u8, s: PadState) {
        if self.no_pads.contains(&g) || id > 7 || self.dry {
            return;
        }
        let key = (g, id);
        if self.pads.get(&key).is_none_or(|p| p.kind != s.kind) {
            self.pads.remove(&key);
            let led = Arc::new(AtomicU8::new(255));
            let Some(target) = self.plug(g, id, s.kind, &led) else { return };
            self.pads.insert(key, Pad { target, kind: s.kind, last: s, led });
            if !self.order.contains(&key) {
                self.order.push(key);
            }
        }
        let p = self.pads.get_mut(&key).unwrap();
        p.last = s;
        Self::push(&mut p.target, &s);
    }

    /// Controllers in player order, for the host UI.
    pub fn pads(&mut self) -> Vec<PadInfo> {
        let order = self.order.clone();
        order
            .into_iter()
            .filter_map(|k| {
                let p = self.pads.get_mut(&k)?;
                let led = p.led.load(Relaxed);
                let player = (matches!(p.target, Target::Xbox(_)) && led < 4).then(|| led as u32 + 1);
                Some(PadInfo { guest: k.0, id: k.1, playstation: p.kind == PAD_PLAYSTATION, player })
            })
            .collect()
    }

    /// Moves a controller one slot up/down by re-plugging every pad in the new order
    /// (Windows numbers players by plug-in order).
    pub fn move_pad(&mut self, index: usize, up: bool) {
        let j = if up { index.checked_sub(1) } else { Some(index + 1) };
        let Some(j) = j.filter(|j| *j < self.order.len()) else { return };
        self.order.swap(index, j);
        let old: Vec<((Guest, u8), u8, PadState)> = self.order.iter().filter_map(|k| self.pads.get(k).map(|p| (*k, p.kind, p.last))).collect();
        self.pads.clear(); // unplugs
        for ((g, id), kind, last) in old {
            let led = Arc::new(AtomicU8::new(255));
            if let Some(mut target) = self.plug(g, id, kind, &led) {
                Self::push(&mut target, &last);
                self.pads.insert((g, id), Pad { target, kind, last, led });
            }
        }
    }

    /// Human-readable gamepad availability for the UI.
    pub fn gamepad_status(&self) -> Option<String> {
        match &self.vigem {
            Some(Err(_)) => Some("Gamepads need the ViGEmBus driver on this PC (github.com/nefarius/ViGEmBus/releases)".into()),
            _ if self.pads.len() > 4 => Some("More than 4 controllers: most games only see the first 4".into()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_guest_release() {
        let mut inj = Injector { dry: true, ..Default::default() };
        inj.apply(1, Input::Key { scan: 0x1E, ext: false, down: true });
        inj.apply(2, Input::Key { scan: 0x1F, ext: false, down: true });
        inj.apply(2, Input::Button { b: 0, down: true });
        inj.release(1);
        assert!(!inj.held.contains_key(&1));
        assert_eq!(inj.held[&2].keys.len(), 1, "guest 2 keeps its keys");
        inj.release_everyone();
        assert!(inj.held.is_empty());
    }

    #[test]
    fn ds4_mapping() {
        let s = PadState { buttons: 0x1000 | 0x0001 | 0x0008 | 0x0400, lt: 200, ly: 32767, lx: -32768, ..Default::default() };
        let r = ds4_report(&s);
        assert_eq!(r.buttons & 0xF, 1, "up+right = NE hat");
        assert!(r.buttons & (1 << 5) != 0, "A -> Cross");
        assert!(r.buttons & (1 << 10) != 0, "L2 pressed");
        assert_eq!((r.special, r.trigger_l), (1, 200));
        assert_eq!((r.thumb_lx, r.thumb_ly), (0, 0), "full left, full up");
        assert_eq!(ds4_report(&PadState::default()).buttons & 0xF, 8, "no dpad = neutral hat");
    }
}
