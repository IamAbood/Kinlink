//! Finds Kinlink PCs on the same network: each PC that lets friends connect broadcasts its
//! friend ID every few seconds; the Friends page lists the ones heard so they can be added
//! without typing the ID. Adding still goes through the normal request + safety code.
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PORT: u16 = 47809;
const MAGIC: &str = "KINLINK1 ";

#[derive(Default)]
struct State {
    /// Our friend ID while we announce (hosting on), else None.
    me: Option<String>,
    /// Friend ID -> last heard.
    seen: HashMap<String, Instant>,
}

#[derive(Clone)]
pub struct Lan(Arc<Mutex<State>>);

impl Lan {
    pub fn start() -> Lan {
        let st = Arc::new(Mutex::new(State::default()));
        let Ok(sock) = bind() else {
            crate::plog!("lan: can't listen on UDP {PORT}; nearby PCs won't show");
            return Lan(st);
        };
        let s2 = st.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            let mut last_send = Instant::now() - Duration::from_secs(60);
            loop {
                let me = s2.lock().unwrap().me.clone();
                if let Some(me) = &me {
                    if last_send.elapsed() >= Duration::from_secs(3) {
                        last_send = Instant::now();
                        let _ = sock.send_to(format!("{MAGIC}{me}").as_bytes(), (Ipv4Addr::BROADCAST, PORT));
                    }
                }
                if let Ok((n, _)) = sock.recv_from(&mut buf) {
                    if let Some(id) = std::str::from_utf8(&buf[..n]).ok().and_then(|s| s.strip_prefix(MAGIC)) {
                        let id = id.trim();
                        if me.as_deref() != Some(id) && id.contains('#') && id.len() < 80 {
                            s2.lock().unwrap().seen.insert(id.to_string(), Instant::now());
                        }
                    }
                }
            }
        });
        Lan(st)
    }

    /// Announce this friend ID (Some) or stay quiet (None).
    pub fn set_announce(&self, me: Option<String>) {
        self.0.lock().unwrap().me = me;
    }

    /// Friend IDs heard in the last 10 s, sorted.
    pub fn nearby(&self) -> Vec<String> {
        let mut s = self.0.lock().unwrap();
        s.seen.retain(|_, t| t.elapsed() < Duration::from_secs(10));
        let mut v: Vec<String> = s.seen.keys().cloned().collect();
        v.sort();
        v
    }
}

fn bind() -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?; // several Kinlinks (test profiles) on one PC
    s.set_broadcast(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, PORT)).into())?;
    let s: UdpSocket = s.into();
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    Ok(s)
}
