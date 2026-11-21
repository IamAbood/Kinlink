//! Browser guests: anyone with a browser link joins from a web page, nothing to install.
//!
//! The link is `PAGE#<secret>`. Browsers never send the part after `#` to a server, so only the
//! link holder knows the secret. It names an MQTT topic (on the same public brokers as friends'
//! signaling) and an AES-256-GCM key. The browser posts its WebRTC offer there, sealed, and we
//! answer the same way. The answer carries our DTLS fingerprint, so nobody who only watches the
//! broker can step into the middle. After that it's plain WebRTC: H.264 video to the browser,
//! input back over a data channel.
use crate::host::Perms;
use crate::proto::Input;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::Aes256Gcm;
use rumqttc::{Client, Event as MqEvent, MqttOptions, Packet, QoS};
use sha2::{Digest, Sha256};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::media::{MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input as RtcInput, Output, Rtc, RtcConfig};

/// The web page browsers open (GitHub Pages, from docs/ in the repository).
pub const PAGE: &str = "https://iamabood.github.io/Kinlink/";
const LINK_FOR: Duration = Duration::from_secs(3600);

/// One encoded video frame (H.264 Annex B) for browser guests.
pub struct Frame {
    pub data: Arc<[u8]>,
    pub key: bool,
}

/// Called with (guest name, permissions, offer); returns our answer.
pub type Accept = dyn Fn(String, Perms, SdpOffer) -> Result<SdpAnswer, String> + Send + Sync;

struct Link {
    secret: [u8; 16],
    perms: Perms,
    expires: Instant,
    /// Watch party: many people can use it (otherwise one use, like invites).
    multi: bool,
}

struct Web {
    links: Mutex<Vec<Link>>,
    /// One slot per broker (None while disconnected).
    clients: Mutex<Vec<Option<Client>>>,
    /// Offer ids already handled (the browser may retry).
    seen: Mutex<Vec<String>>,
    /// The running host (replaced when hosting restarts).
    accept: Mutex<Option<Arc<Accept>>>,
}

static WEB: OnceLock<Arc<Web>> = OnceLock::new();

fn web() -> &'static Arc<Web> {
    WEB.get_or_init(|| {
        let w = Arc::new(Web { links: Mutex::new(Vec::new()), clients: Mutex::new(vec![None; crate::signal::BROKERS.len()]), seen: Mutex::new(Vec::new()), accept: Mutex::new(None) });
        for i in 0..crate::signal::BROKERS.len() {
            let w = w.clone();
            thread::spawn(move || w.broker_loop(i));
        }
        w
    })
}

/// The host that answers browser guests (None when hosting stops).
pub fn set_accept(a: Option<Arc<Accept>>) {
    if let Some(w) = WEB.get() {
        *w.accept.lock().unwrap() = a;
    }
}

/// A new browser link for guests with these permissions: one use for an hour, or a watch
/// party (`multi`): any number of uses for 3 hours.
pub fn new_link(perms: Perms, multi: bool, accept: Arc<Accept>) -> String {
    let w = web();
    *w.accept.lock().unwrap() = Some(accept);
    let secret: [u8; 16] = rand::random();
    let mut links = w.links.lock().unwrap();
    links.retain(|l| l.expires > Instant::now());
    let valid = if multi { LINK_FOR * 3 } else { LINK_FOR };
    links.push(Link { secret, perms, expires: Instant::now() + valid, multi });
    for c in w.clients.lock().unwrap().iter().flatten() {
        let _ = c.try_subscribe(format!("{}/b", topic(&secret)), QoS::AtLeastOnce);
    }
    format!("{PAGE}#{}", hex(&secret))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hash(label: &str, secret: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(label.as_bytes());
    h.update(secret);
    h.finalize().into()
}

/// "kinlink/web/<32 hex>": derived from the secret, says nothing about it.
fn topic(secret: &[u8]) -> String {
    format!("kinlink/web/{}", &hex(&hash("kinlink-web-topic", secret))[..32])
}

/// nonce (12) || AES-256-GCM(ciphertext + tag)
fn seal(secret: &[u8], plain: &[u8]) -> Vec<u8> {
    let key = hash("kinlink-web-key", secret);
    let nonce: [u8; 12] = rand::random();
    let ct = Aes256Gcm::new(&key.into()).encrypt(&nonce.into(), plain).expect("encrypt");
    [nonce.as_slice(), &ct].concat()
}

fn open(secret: &[u8], b: &[u8]) -> Option<Vec<u8>> {
    if b.len() < 12 + 16 {
        return None;
    }
    let key = hash("kinlink-web-key", secret);
    let nonce: [u8; 12] = b[..12].try_into().ok()?;
    Aes256Gcm::new(&key.into()).decrypt(&nonce.into(), &b[12..]).ok()
}

impl Web {
    fn broker_loop(self: &Arc<Self>, i: usize) {
        let (host, port) = crate::signal::BROKERS[i];
        loop {
            let mut opts = MqttOptions::new(format!("w{:016x}", rand::random::<u64>()), host, port);
            opts.set_keep_alive(Duration::from_secs(30));
            opts.set_transport(crate::signal::tls());
            let (client, mut conn) = Client::new(opts, 64);
            for ev in conn.iter() {
                match ev {
                    Ok(MqEvent::Incoming(Packet::ConnAck(_))) => {
                        for l in self.links.lock().unwrap().iter() {
                            let _ = client.try_subscribe(format!("{}/b", topic(&l.secret)), QoS::AtLeastOnce);
                        }
                        self.clients.lock().unwrap()[i] = Some(client.clone());
                    }
                    Ok(MqEvent::Incoming(Packet::Publish(p))) => {
                        let (me, c) = (self.clone(), client.clone());
                        thread::spawn(move || me.on_offer(&c, &p.topic, &p.payload));
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            self.clients.lock().unwrap()[i] = None;
            thread::sleep(Duration::from_secs(3));
        }
    }

    fn on_offer(&self, client: &Client, t: &str, payload: &[u8]) {
        let found = self.links.lock().unwrap().iter().find(|l| format!("{}/b", topic(&l.secret)) == t).map(|l| (l.secret, l.perms, l.expires, l.multi));
        let Some((secret, perms, expires, multi)) = found else { return };
        let Some(plain) = open(&secret, payload) else { return };
        let Ok(msg) = serde_json::from_slice::<serde_json::Value>(&plain) else { return };
        let id = msg["id"].as_str().unwrap_or_default().to_string();
        {
            let mut seen = self.seen.lock().unwrap();
            if id.is_empty() || seen.contains(&id) {
                return;
            }
            seen.push(id.clone());
            if seen.len() > 200 {
                seen.remove(0);
            }
        }
        let name: String = msg["name"].as_str().unwrap_or("").chars().filter(|c| !c.is_control()).take(24).collect();
        let name = if name.trim().is_empty() { "Guest".to_string() } else { name.trim().to_string() };
        let reply = if expires < Instant::now() {
            serde_json::json!({ "id": id, "error": "This link has expired. Ask for a new one." })
        } else {
            let accept = self.accept.lock().unwrap().clone();
            let offer = serde_json::from_value::<SdpOffer>(msg["offer"].clone()).map_err(|e| format!("bad offer: {e}"));
            match (accept, offer) {
                (None, _) => serde_json::json!({ "id": id, "error": "That PC isn't hosting right now." }),
                (_, Err(e)) => serde_json::json!({ "id": id, "error": e }),
                (Some(a), Ok(offer)) => match a(name.clone(), perms, offer) {
                    Ok(answer) => {
                        // One use, like invites (watch party links stay).
                        if !multi {
                            self.links.lock().unwrap().retain(|l| l.secret != secret);
                        }
                        serde_json::json!({ "id": id, "answer": answer })
                    }
                    Err(e) => serde_json::json!({ "id": id, "error": e }),
                },
            }
        };
        let _ = client.try_publish(format!("{}/h", topic(&secret)), QoS::AtLeastOnce, false, seal(&secret, reply.to_string().as_bytes()));
    }
}

/// Browser input arrives as small JSON arrays: ["m",x,y] ["b",button,down] ["w",dx,dy]
/// ["k",scan,ext,down] ["r"].
fn parse_input(s: &[u8]) -> Option<Input> {
    let v: serde_json::Value = serde_json::from_slice(s).ok()?;
    let a = v.as_array()?;
    let n = |i: usize| a.get(i).and_then(|x| x.as_f64());
    Some(match a.first()?.as_str()? {
        "m" => Input::MouseAbs { x: n(1)?.clamp(0.0, 1.0) as f32, y: n(2)?.clamp(0.0, 1.0) as f32 },
        "b" => Input::Button { b: (n(1)? as u8).min(4), down: n(2)? != 0.0 },
        "w" => Input::Wheel { dx: n(1)?.clamp(-1200.0, 1200.0) as i32, dy: n(2)?.clamp(-1200.0, 1200.0) as i32 },
        "k" => Input::Key { scan: n(1)? as u16, ext: n(2)? != 0.0, down: n(3)? != 0.0 },
        "r" => Input::ReleaseAll,
        _ => return None,
    })
}

/// Hooks from the host for one browser guest.
pub struct Guest {
    pub frames: Receiver<Arc<Frame>>,
    /// Cleared to end the session (kick).
    pub alive: Arc<AtomicBool>,
    pub on_input: Box<dyn Fn(Input) + Send>,
    pub on_keyframe: Box<dyn Fn() + Send>,
    pub on_end: Box<dyn FnOnce() + Send>,
}

/// Answers a browser's offer and runs the session on its own thread.
pub fn serve(offer: SdpOffer, g: Guest) -> Result<SdpAnswer, String> {
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    let ip = crate::net::local_ip(false).ok_or("no network")?;
    let local = SocketAddr::new(ip, sock.local_addr().map_err(|e| e.to_string())?.port());
    let mut rtc = RtcConfig::new().clear_codecs().enable_h264(true).build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").map_err(|e| e.to_string())?);
    // Public address (so it works across the internet, not just at home).
    if let Some(public) = crate::stun::query(&sock, Duration::from_millis(1500)) {
        if let Ok(c) = Candidate::server_reflexive(public, local, "udp") {
            rtc.add_local_candidate(c);
        }
    }
    let answer = rtc.sdp_api().accept_offer(offer).map_err(|e| format!("can't answer: {e}"))?;
    thread::spawn(move || {
        run(rtc, sock, local, &g);
        (g.on_end)();
    });
    Ok(answer)
}

/// The H.264 payload type to send with: High profile if the browser takes it.
fn pick_pt(rtc: &mut Rtc, mid: Mid) -> Option<Pt> {
    let w = rtc.writer(mid)?;
    let ps: Vec<_> = w.payload_params().filter(|p| p.spec().format.packetization_mode == Some(1)).map(|p| (p.pt(), p.spec().format.profile_level_id)).collect();
    ps.iter().find(|p| p.1.is_some_and(|id| id >> 16 == 0x64)).or(ps.first()).map(|p| p.0)
}

fn run(mut rtc: Rtc, sock: UdpSocket, local: SocketAddr, g: &Guest) {
    let start = Instant::now();
    let mut video: Option<(Mid, Pt)> = None;
    let mut connected = false;
    let mut need_key = true;
    let mut buf = vec![0u8; 2000];
    loop {
        if !g.alive.load(SeqCst) || (!connected && start.elapsed() > Duration::from_secs(30)) {
            rtc.disconnect();
            return;
        }
        // New video frames.
        loop {
            match g.frames.try_recv() {
                Ok(f) => {
                    let Some((mid, pt)) = video.filter(|_| connected) else { continue };
                    if need_key && !f.key {
                        continue;
                    }
                    need_key = false;
                    let ts = MediaTime::from_90khz(start.elapsed().as_micros() as u64 * 9 / 100);
                    if let Some(w) = rtc.writer(mid) {
                        if w.write(pt, Instant::now(), ts, f.data.clone()).is_err() {
                            return;
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return, // host stopped
            }
        }
        // Everything str0m wants to send or tell us.
        let timeout = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    let _ = sock.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(e)) => match e {
                    Event::Connected => {
                        connected = true;
                        crate::plog!("web: browser guest connected");
                        (g.on_keyframe)();
                    }
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => return,
                    Event::MediaAdded(m) if m.kind == MediaKind::Video => {
                        video = pick_pt(&mut rtc, m.mid).map(|pt| (m.mid, pt));
                    }
                    Event::KeyframeRequest(_) => (g.on_keyframe)(),
                    Event::ChannelData(d) => {
                        if let Some(i) = parse_input(&d.data) {
                            (g.on_input)(i);
                        }
                    }
                    Event::ChannelClose(_) => return,
                    _ => {}
                },
                Err(e) => {
                    crate::plog!("web: session error: {e}");
                    return;
                }
            }
        };
        // Wait a little for network input (frames come from another thread, so not too long).
        let wait = timeout.saturating_duration_since(Instant::now()).clamp(Duration::from_millis(1), Duration::from_millis(4));
        let _ = sock.set_read_timeout(Some(wait));
        let input = match sock.recv_from(&mut buf) {
            Ok((n, source)) => match buf[..n].try_into() {
                Ok(contents) => RtcInput::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: local, contents }),
                Err(_) => continue,
            },
            Err(_) => RtcInput::Timeout(Instant::now()),
        };
        if rtc.handle_input(input).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_roundtrip_and_input() {
        let s = [7u8; 16];
        let b = seal(&s, b"hello");
        assert_eq!(open(&s, &b).unwrap(), b"hello");
        assert!(open(&[8u8; 16], &b).is_none());
        assert!(topic(&s).starts_with("kinlink/web/") && topic(&s).len() == 12 + 32);
        assert_eq!(parse_input(br#"["k",30,0,1]"#), Some(Input::Key { scan: 30, ext: false, down: true }));
        assert_eq!(parse_input(br#"["m",2,-1]"#), Some(Input::MouseAbs { x: 1.0, y: 0.0 }));
        assert_eq!(parse_input(br#"["zz"]"#), None);
    }
}
