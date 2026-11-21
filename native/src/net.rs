//! Small cross-platform network helpers (shared with the Android app).
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros() as u64
}

/// Local address the OS would use to reach the internet (no packet is sent).
pub fn local_ip(v6: bool) -> Option<IpAddr> {
    let (bind, probe): (SocketAddr, SocketAddr) = if v6 {
        ("[::]:0".parse().unwrap(), "[2001:4860:4860::8888]:80".parse().unwrap())
    } else {
        ("0.0.0.0:0".parse().unwrap(), "8.8.8.8:80".parse().unwrap())
    };
    let s = UdpSocket::bind(bind).ok()?;
    s.connect(probe).ok()?;
    Some(s.local_addr().ok()?.ip())
}
