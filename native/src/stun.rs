//! Minimal STUN (RFC 5389) Binding request: learns the public address a UDP socket appears as,
//! which the other PC needs for hole punching. Stateless public servers; no data but the request.
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

const COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
pub const SERVERS: &[&str] = &["stun.l.google.com:19302", "stun.cloudflare.com:3478", "stun1.l.google.com:19302"];

pub fn request(txid: &[u8; 12]) -> Vec<u8> {
    [&[0x00, 0x01, 0x00, 0x00][..], &COOKIE, txid].concat()
}

pub fn is_stun(b: &[u8]) -> bool {
    b.len() >= 20 && b[4..8] == COOKIE && b[0] & 0xC0 == 0
}

/// Parses a Binding success response: (transaction id, mapped address).
pub fn parse(b: &[u8]) -> Option<([u8; 12], SocketAddr)> {
    if !is_stun(b) || b[0..2] != [0x01, 0x01] {
        return None;
    }
    let txid: [u8; 12] = b[8..20].try_into().ok()?;
    let mut i = 20;
    let mut plain = None;
    while i + 4 <= b.len() {
        let t = u16::from_be_bytes([b[i], b[i + 1]]);
        let l = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        let v = b.get(i + 4..i + 4 + l)?;
        if (t == 0x0020 || t == 0x0001) && l >= 8 && v[1] == 0x01 {
            let x = t == 0x0020;
            let mut port = u16::from_be_bytes([v[2], v[3]]);
            let mut ip = [v[4], v[5], v[6], v[7]];
            if x {
                port ^= 0x2112;
                for k in 0..4 {
                    ip[k] ^= COOKIE[k];
                }
            }
            let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port);
            if x {
                return Some((txid, a));
            }
            plain = Some(a);
        }
        i += 4 + (l + 3) / 4 * 4;
    }
    plain.map(|a| (txid, a))
}

/// Sends the request to all servers; returns the first public address reported.
/// Uses the socket directly, so only call it before another thread starts reading from it.
pub fn query(sock: &UdpSocket, timeout: Duration) -> Option<SocketAddr> {
    let txid: [u8; 12] = rand::random();
    let req = request(&txid);
    for s in SERVERS {
        if let Some(a) = s.to_socket_addrs().ok().and_then(|mut it| it.find(|a| a.is_ipv4())) {
            let _ = sock.send_to(&req, a);
        }
    }
    let old = sock.read_timeout().ok().flatten();
    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
    let mut buf = [0u8; 512];
    let deadline = Instant::now() + timeout;
    let mut found = None;
    while Instant::now() < deadline {
        if let Ok((n, _)) = sock.recv_from(&mut buf) {
            if let Some((t, a)) = parse(&buf[..n]) {
                if t == txid {
                    found = Some(a);
                    break;
                }
            }
        }
    }
    let _ = sock.set_read_timeout(old);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_xor_mapped() {
        // Response for 203.0.113.9:40000 using XOR-MAPPED-ADDRESS
        let txid = [7u8; 12];
        let port = 40000u16 ^ 0x2112;
        let ip = [203 ^ 0x21, 0 ^ 0x12, 113 ^ 0xA4, 9 ^ 0x42];
        let mut r = vec![0x01, 0x01, 0x00, 0x0C];
        r.extend_from_slice(&COOKIE);
        r.extend_from_slice(&txid);
        r.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
        r.extend_from_slice(&port.to_be_bytes());
        r.extend_from_slice(&ip);
        assert_eq!(parse(&r), Some((txid, "203.0.113.9:40000".parse().unwrap())));
        assert!(is_stun(&request(&txid)));
        assert!(!is_stun(&[0xC5; 30]));
    }
}
