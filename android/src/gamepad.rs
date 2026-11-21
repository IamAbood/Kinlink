//! On-screen Xbox-style controller: the phone becomes a gamepad for PC games (needs the
//! ViGEmBus driver on the PC, like any controller). Multi-touch: each finger holds one control.
use crate::proto::{PadState, PAD_XBOX};
use egui::{pos2, vec2, Color32, FontId, Painter, Pos2, Rect, Stroke};
use std::collections::HashMap;

// XInput button bits
const UP: u16 = 0x0001;
const DOWN: u16 = 0x0002;
const LEFT: u16 = 0x0004;
const RIGHT: u16 = 0x0008;
const START: u16 = 0x0010;
const BACK: u16 = 0x0020;
const LTHUMB: u16 = 0x0040;
const RTHUMB: u16 = 0x0080;
const LB: u16 = 0x0100;
const RB: u16 = 0x0200;
const A: u16 = 0x1000;
const B: u16 = 0x2000;
const X: u16 = 0x4000;
const Y: u16 = 0x8000;

#[derive(Clone, Copy, PartialEq)]
enum Ctl {
    LeftStick,
    RightStick,
    Button(u16),
    LeftTrigger,
    RightTrigger,
}

struct Shape {
    ctl: Ctl,
    center: Pos2,
    radius: f32,
    label: &'static str,
}

#[derive(Default)]
pub struct TouchPad {
    /// finger -> (control, where the finger is now)
    fingers: HashMap<u64, (Ctl, Pos2)>,
    last_sent: Option<PadState>,
}

/// Where everything goes on a screen of this size (points).
fn layout(s: Rect) -> Vec<Shape> {
    // Stay clear of the edges: Android uses edge swipes for back / notifications.
    let s = s.shrink2(vec2(s.width() * 0.06, s.height() * 0.10));
    let (w, h, o) = (s.width(), s.height(), s.min);
    let u = h.min(w) * 0.1; // unit: 10% of the short side
    let at = |x: f32, y: f32| pos2(o.x + x * w, o.y + y * h);
    let mut v = vec![
        Shape { ctl: Ctl::LeftStick, center: at(0.16, 0.58), radius: 1.6 * u, label: "" },
        Shape { ctl: Ctl::RightStick, center: at(0.68, 0.78), radius: 1.3 * u, label: "" },
        Shape { ctl: Ctl::LeftTrigger, center: at(0.08, 0.13), radius: 0.7 * u, label: "LT" },
        Shape { ctl: Ctl::RightTrigger, center: at(0.92, 0.13), radius: 0.7 * u, label: "RT" },
        Shape { ctl: Ctl::Button(LB), center: at(0.08, 0.30), radius: 0.6 * u, label: "LB" },
        Shape { ctl: Ctl::Button(RB), center: at(0.92, 0.30), radius: 0.6 * u, label: "RB" },
        Shape { ctl: Ctl::Button(BACK), center: at(0.41, 0.16), radius: 0.45 * u, label: "Back" },
        Shape { ctl: Ctl::Button(START), center: at(0.59, 0.16), radius: 0.45 * u, label: "Start" },
        Shape { ctl: Ctl::Button(LTHUMB), center: at(0.06, 0.88), radius: 0.4 * u, label: "L3" },
        Shape { ctl: Ctl::Button(RTHUMB), center: at(0.94, 0.92), radius: 0.4 * u, label: "R3" },
    ];
    let abxy = at(0.86, 0.52);
    let d = 1.05 * u;
    for (bit, dx, dy, label) in [(A, 0.0, 1.0, "A"), (B, 1.0, 0.0, "B"), (X, -1.0, 0.0, "X"), (Y, 0.0, -1.0, "Y")] {
        v.push(Shape { ctl: Ctl::Button(bit), center: abxy + vec2(dx * d, dy * d), radius: 0.55 * u, label });
    }
    let pad = at(0.33, 0.80);
    for (bit, dx, dy, label) in [(UP, 0.0, -1.0, ""), (DOWN, 0.0, 1.0, ""), (LEFT, -1.0, 0.0, ""), (RIGHT, 1.0, 0.0, "")] {
        v.push(Shape { ctl: Ctl::Button(bit), center: pad + vec2(dx * 0.9 * u, dy * 0.9 * u), radius: 0.5 * u, label });
    }
    v
}

fn face(bit: u16) -> Color32 {
    match bit {
        A => Color32::from_rgb(0x3D, 0xDC, 0x97),
        B => Color32::from_rgb(0xFF, 0x7A, 0x96),
        X => Color32::from_rgb(0x6C, 0xB4, 0xFF),
        Y => Color32::from_rgb(0xFF, 0xD2, 0x6A),
        _ => Color32::WHITE,
    }
}

impl TouchPad {
    /// A finger moved on the screen (points). Returns the new pad state when it changed.
    pub fn touch(&mut self, id: u64, phase: winit::event::TouchPhase, p: Pos2, screen: Rect) -> Option<PadState> {
        use winit::event::TouchPhase::*;
        match phase {
            Started => {
                // The control under the finger (a bit of slack around each one).
                let hit = layout(screen).into_iter().filter(|s| s.center.distance(p) < s.radius * 1.35).min_by(|a, b| a.center.distance(p).total_cmp(&b.center.distance(p)));
                if let Some(s) = hit {
                    self.fingers.insert(id, (s.ctl, p));
                }
            }
            Moved => {
                if let Some(f) = self.fingers.get_mut(&id) {
                    f.1 = p;
                }
            }
            Ended | Cancelled => {
                self.fingers.remove(&id);
            }
        }
        let st = self.state(screen);
        if self.last_sent != Some(st) {
            self.last_sent = Some(st);
            Some(st)
        } else {
            None
        }
    }

    /// This finger is holding one of our controls.
    pub fn owns(&self, id: u64) -> bool {
        self.fingers.contains_key(&id)
    }

    /// Everything let go (leaving controller mode).
    pub fn release(&mut self) {
        self.fingers.clear();
        self.last_sent = None;
    }

    fn state(&self, screen: Rect) -> PadState {
        let shapes = layout(screen);
        let mut s = PadState { kind: PAD_XBOX, ..Default::default() };
        for (ctl, p) in self.fingers.values() {
            let Some(shape) = shapes.iter().find(|x| x.ctl == *ctl) else { continue };
            let stick = |p: Pos2| {
                let d = (p - shape.center) / shape.radius;
                let d = if d.length() > 1.0 { d.normalized() } else { d };
                ((d.x * 32767.0) as i16, (-d.y * 32767.0) as i16) // screen y down, XInput y up
            };
            match ctl {
                Ctl::LeftStick => (s.lx, s.ly) = stick(*p),
                Ctl::RightStick => (s.rx, s.ry) = stick(*p),
                Ctl::LeftTrigger => s.lt = 255,
                Ctl::RightTrigger => s.rt = 255,
                Ctl::Button(b) => s.buttons |= b,
            }
        }
        s
    }

    /// Draws the controller over the video.
    pub fn draw(&self, p: &Painter, screen: Rect) {
        let st = self.state(screen);
        for s in layout(screen) {
            let pressed = match s.ctl {
                Ctl::Button(b) => st.buttons & b != 0,
                Ctl::LeftTrigger => st.lt > 0,
                Ctl::RightTrigger => st.rt > 0,
                _ => false,
            };
            let tint = match s.ctl {
                Ctl::Button(b) => face(b),
                _ => Color32::WHITE,
            };
            match s.ctl {
                Ctl::LeftStick | Ctl::RightStick => {
                    p.circle(s.center, s.radius, Color32::from_black_alpha(70), Stroke::new(2.0, Color32::from_white_alpha(90)));
                    let (x, y) = if s.ctl == Ctl::LeftStick { (st.lx, st.ly) } else { (st.rx, st.ry) };
                    let knob = s.center + vec2(x as f32 / 32767.0, -(y as f32) / 32767.0) * s.radius;
                    p.circle(knob, s.radius * 0.42, Color32::from_white_alpha(if x != 0 || y != 0 { 170 } else { 110 }), Stroke::NONE);
                }
                _ => {
                    let fill = if pressed { tint.gamma_multiply(0.85) } else { Color32::from_black_alpha(80) };
                    p.circle(s.center, s.radius, fill, Stroke::new(2.0, tint.gamma_multiply(0.7)));
                    let fg = if pressed { Color32::BLACK } else { tint };
                    if let Ctl::Button(b @ (UP | DOWN | LEFT | RIGHT)) = s.ctl {
                        let dir = match b { UP => vec2(0.0, -1.0), DOWN => vec2(0.0, 1.0), LEFT => vec2(-1.0, 0.0), _ => vec2(1.0, 0.0) };
                        let side = vec2(-dir.y, dir.x);
                        let r = s.radius * 0.45;
                        p.add(egui::Shape::convex_polygon(vec![s.center + dir * r, s.center - dir * r * 0.6 + side * r * 0.8, s.center - dir * r * 0.6 - side * r * 0.8], fg, Stroke::NONE));
                    } else {
                        p.text(s.center, egui::Align2::CENTER_CENTER, s.label, FontId::proportional(s.radius * 0.7), fg);
                    }
                }
            }
        }
    }
}
