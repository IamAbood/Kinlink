//! Messages carried inside the encrypted UDP session (see crypto::Cipher).
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

pub const DEFAULT_PORT: u16 = 47800;
pub const VERSION: u32 = 2;
/// Video payload per datagram; stays under ~1200 bytes after bincode + encryption headers.
pub const CHUNK: usize = 1100;
/// PadState::kind values: how the controller should appear on the host.
pub const PAD_XBOX: u8 = 0;
pub const PAD_PLAYSTATION: u8 = 1;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Msg {
    // client -> host
    Hello { version: u32 },
    Ping { t: u64, lost: u32 },
    Input(Input),
    KeyframeRequest,
    SelectMonitor(u8),
    /// max_height 0 = native resolution; mbps = bitrate cap.
    SetQuality { max_height: u32, mbps: u32 },
    Bye,
    // host -> client
    Welcome { monitors: Vec<String>, current: u8, audio_rate: u32, mouse: bool, keyboard: bool, gamepad: bool },
    /// Vibration for one of the guest's controllers (0-255 per motor).
    Rumble { pad: u8, large: u8, small: u8 },
    Reject { reason: String },
    Pong { t: u64 },
    /// `t` = host capture time in µs (host clock; only comparable on the same PC)
    Video { frame: u32, idx: u16, count: u16, key: bool, t: u64, data: Vec<u8> },
    Audio { seq: u32, pcm: Vec<i16> },
    /// Host cursor drawn by the client (snappy, no video lag). RGBA, sent when the shape changes.
    CursorShape { version: u32, w: u16, h: u16, hx: u16, hy: u16, rgba: Vec<u8> },
    /// Cursor position, 0..1 within the streamed monitor.
    CursorPos { x: f32, y: f32, visible: bool, version: u32 },
    // both directions
    FileOffer { id: u32, name: String, size: u64 },
    FileChunk { id: u32, idx: u64, data: Vec<u8> },
    /// Everything below `have_until` arrived; `bits` marks chunks after it.
    FileAck { id: u32, have_until: u64, bits: Vec<u8> },
    FileCancel { id: u32 },
    /// Voice chat: 20 ms of 24 kHz mono. The host fills in `from`.
    Voice { from: String, pcm: Vec<i16> },
    /// Text chat. The host fills in `from` (guests can't pick a name).
    Chat { from: String, text: String },
    Clipboard { seq: u32, text: String },
    ClipboardAck { seq: u32 },
    // client -> host (added later: older hosts just ignore it)
    /// Sharp text: more detail per frame instead of smoothness (frame rate capped at 30).
    SetSharp(bool),
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum Input {
    /// Absolute position, 0..1 within the currently streamed monitor.
    MouseAbs { x: f32, y: f32 },
    MouseRel { dx: i32, dy: i32 },
    /// 0 left, 1 right, 2 middle, 3 back (X1), 4 forward (X2)
    Button { b: u8, down: bool },
    /// 120 units = one wheel notch (Windows WHEEL_DELTA)
    Wheel { dx: i32, dy: i32 },
    /// Windows hardware scan code; `ext` = E0-prefixed key (arrows, right ctrl, ...)
    Key { scan: u16, ext: bool, down: bool },
    Pad { id: u8, state: PadState },
    PadRemove { id: u8 },
    ReleaseAll,
}

/// XInput gamepad snapshot.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Default)]
pub struct PadState {
    pub buttons: u16,
    pub lt: u8,
    pub rt: u8,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    /// PAD_XBOX or PAD_PLAYSTATION
    pub kind: u8,
}

pub fn encode(m: &Msg) -> Vec<u8> {
    bincode::serialize(m).expect("serialize")
}

pub fn decode(b: &[u8]) -> Option<Msg> {
    bincode::deserialize(b).ok()
}

/// True for addresses that can't be reached from the internet (LAN, CGNAT, loopback, link-local).
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            v.is_private() || v.is_loopback() || v.is_link_local() || (o[0] == 100 && (o[1] & 0xC0) == 64)
        }
        IpAddr::V6(v) => {
            let s = v.segments()[0];
            v.is_loopback() || (s & 0xffc0) == 0xfe80 || (s & 0xfe00) == 0xfc00
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges() {
        assert!(is_private("100.64.1.1".parse().unwrap())); // CGNAT
        assert!(is_private("192.168.0.1".parse().unwrap()));
        assert!(!is_private("8.8.8.8".parse().unwrap()));
        assert!(is_private("fe80::1".parse().unwrap()));
        assert!(!is_private("2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn file_chunk_fits_after_encryption() {
        let m = Msg::FileChunk { id: u32::MAX, idx: u64::MAX, data: vec![0; crate::files::CHUNK] };
        assert!(encode(&m).len() + 17 + 16 <= 1200);
    }

    #[test]
    fn video_packet_fits_after_encryption() {
        let m = Msg::Video { frame: u32::MAX, idx: 1, count: 2, key: true, t: u64::MAX, data: vec![0; CHUNK] };
        assert!(encode(&m).len() + 17 + 16 <= 1200);
    }
}
