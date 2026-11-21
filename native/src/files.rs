//! Reliable file transfer over the lossy UDP session: fixed-size chunks, a sliding window,
//! cumulative + bitmap acks, and resends. Used by both host and client.
use crate::proto::Msg;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

/// Ask before accepting incoming files (Settings > General). Offers wait for Accept/Decline.
pub static ASK: AtomicBool = AtomicBool::new(false);
/// Unanswered offers decline themselves after this long.
const OFFER_TIMEOUT: Duration = Duration::from_secs(120);
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const CHUNK: usize = 1100;
const WINDOW: u64 = 512; // chunks in flight (~560 KB)
const RESEND_AFTER: Duration = Duration::from_millis(250);
// ponytail: fixed send rate (~8.8 MB/s) so files don't starve the video; adapt if links vary a lot
const MAX_PER_PUMP: usize = 40;

#[derive(Clone, Debug)]
pub struct Progress {
    pub id: u32,
    pub name: String,
    pub sending: bool,
    pub done: u64,
    pub size: u64,
    pub finished: bool,
    pub failed: Option<String>,
    pub path: Option<PathBuf>,
}

struct Out {
    name: String,
    size: u64,
    file: File,
    chunks: u64,
    base: u64,
    acked: Vec<bool>,
    sent: HashMap<u64, Instant>,
    started: Instant,
    finished: bool,
    /// The receiver has answered (acked) at least once; until then only a trickle is sent.
    got_ack: bool,
}

struct In {
    name: String,
    size: u64,
    path: PathBuf,
    file: File,
    chunks: u64,
    have: Vec<bool>,
    have_until: u64,
    since_ack: u32,
    last_ack: Instant,
    finished: bool,
}

#[derive(Default)]
pub struct Transfers {
    next_id: u32,
    out: HashMap<u32, Out>,
    inc: HashMap<u32, In>,
    finished: Vec<Progress>,
    /// Offers waiting for the user to accept (id, name, size, when).
    pending: Vec<(u32, String, u64, Instant)>,
    /// Overrides ASK for this transfer set (tests).
    pub ask: Option<bool>,
    /// Where received files go (Downloads\Kinlink by default).
    pub save_dir: Option<PathBuf>,
}

fn chunks_for(size: u64) -> u64 {
    size.div_ceil(CHUNK as u64).max(1)
}

/// Creates a new file with a free name in `dir` ("a.txt", "a (1).txt", ...). create_new makes
/// claiming the name atomic, so two transfers of the same name can't overwrite each other.
fn create_unique(dir: &Path, name: &str) -> Option<(PathBuf, File)> {
    let first = free_name(dir, name);
    let stem_ext = first.file_name()?.to_string_lossy().to_string();
    for i in 0..1000 {
        let p = if i == 0 { first.clone() } else { free_name(dir, &stem_ext) };
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(f) => return Some((p, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Picks a free file name in `dir` ("a.txt", "a (1).txt", ...), stripping any path parts.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let base = Path::new(name).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
    let clean: String = base.chars().map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { '_' } else { c }).collect();
    let (stem, ext) = match clean.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (clean.clone(), String::new()),
    };
    (0..)
        .map(|i| if i == 0 { dir.join(&clean) } else { dir.join(format!("{stem} ({i}){ext}")) })
        .find(|p| !p.exists())
        .unwrap()
}

pub fn default_save_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("PCLONE_SAVE_DIR") {
        return Some(PathBuf::from(d)); // tests
    }
    Some(PathBuf::from(std::env::var_os("USERPROFILE")?).join("Downloads").join("Kinlink"))
}

impl Transfers {
    /// Starts sending a file; returns the offer to send to the other side.
    pub fn send(&mut self, path: &Path) -> Result<Msg, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let size = file.metadata().map_err(|e| e.to_string())?.len();
        if path.is_dir() {
            return Err("Folders can't be sent (zip it first)".into());
        }
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
        let chunks = chunks_for(size);
        self.out.insert(id, Out { name: name.clone(), size, file, chunks, base: 0, acked: vec![false; chunks as usize], sent: HashMap::new(), started: Instant::now(), finished: false, got_ack: false });
        Ok(Msg::FileOffer { id, name, size })
    }

    /// Handles a file message from the other side; returns replies to send.
    pub fn on_msg(&mut self, m: Msg) -> Vec<Msg> {
        match m {
            Msg::FileOffer { id, name, size } => {
                if self.inc.contains_key(&id) || self.pending.iter().any(|p| p.0 == id) {
                    return vec![];
                }
                if self.ask.unwrap_or_else(|| ASK.load(Relaxed)) {
                    self.pending.push((id, name, size, Instant::now()));
                    return vec![];
                }
                self.start_receiving(id, name, size)
            }
            Msg::FileChunk { id, idx, data } => {
                let Some(t) = self.inc.get_mut(&id) else { return vec![] };
                if idx >= t.chunks || t.have[idx as usize] {
                    t.since_ack += 1;
                } else {
                    if t.file.seek(SeekFrom::Start(idx * CHUNK as u64)).is_ok() && t.file.write_all(&data).is_ok() {
                        t.have[idx as usize] = true;
                        while t.have_until < t.chunks && t.have[t.have_until as usize] {
                            t.have_until += 1;
                        }
                    }
                    t.since_ack += 1;
                }
                let complete = t.have_until == t.chunks;
                if complete || t.since_ack >= 32 {
                    if complete && !t.finished {
                        t.finished = true;
                        let _ = t.file.flush();
                        self.finished.push(Progress { id, name: t.name.clone(), sending: false, done: t.size, size: t.size, finished: true, failed: None, path: Some(t.path.clone()) });
                    }
                    return vec![self.ack(id)];
                }
                vec![]
            }
            Msg::FileAck { id, have_until, bits } => {
                let Some(t) = self.out.get_mut(&id) else { return vec![] };
                t.got_ack = true;
                for i in t.base..have_until.min(t.chunks) {
                    t.acked[i as usize] = true;
                }
                for (byte_i, b) in bits.iter().enumerate() {
                    for bit in 0..8 {
                        let i = have_until + (byte_i * 8 + bit) as u64;
                        if b & (1 << bit) != 0 && i < t.chunks {
                            t.acked[i as usize] = true;
                        }
                    }
                }
                t.base = t.base.max(have_until.min(t.chunks));
                if t.base >= t.chunks && !t.finished {
                    t.finished = true;
                    self.finished.push(Progress { id, name: t.name.clone(), sending: true, done: t.size, size: t.size, finished: true, failed: None, path: None });
                }
                vec![]
            }
            Msg::FileCancel { id } => {
                self.pending.retain(|p| p.0 != id);
                if let Some(t) = self.out.remove(&id) {
                    self.finished.push(Progress { id, name: t.name, sending: true, done: 0, size: t.size, finished: false, failed: Some("cancelled by the other side".into()), path: None });
                }
                if let Some(t) = self.inc.remove(&id) {
                    if !t.finished {
                        let _ = std::fs::remove_file(&t.path);
                        self.finished.push(Progress { id, name: t.name, sending: false, done: 0, size: t.size, finished: false, failed: Some("cancelled".into()), path: None });
                    }
                }
                vec![]
            }
            _ => vec![],
        }
    }

    fn start_receiving(&mut self, id: u32, name: String, size: u64) -> Vec<Msg> {
        let Some(dir) = self.save_dir.clone().or_else(default_save_dir) else { return vec![Msg::FileCancel { id }] };
        let _ = std::fs::create_dir_all(&dir);
        let Some((path, file)) = create_unique(&dir, &name) else { return vec![Msg::FileCancel { id }] };
        let _ = file.set_len(size);
        let chunks = chunks_for(size);
        self.inc.insert(id, In { name, size, path, file, chunks, have: vec![false; chunks as usize], have_until: 0, since_ack: 0, last_ack: Instant::now(), finished: false });
        vec![self.ack(id)]
    }

    /// Offers waiting for Accept/Decline: (id, name, size).
    pub fn pending(&self) -> Vec<(u32, String, u64)> {
        self.pending.iter().map(|p| (p.0, p.1.clone(), p.2)).collect()
    }

    /// The user accepted an offer: start receiving (returns replies to send).
    pub fn accept(&mut self, id: u32) -> Vec<Msg> {
        let Some(i) = self.pending.iter().position(|p| p.0 == id) else { return vec![] };
        let (id, name, size, _) = self.pending.remove(i);
        self.start_receiving(id, name, size)
    }

    /// The user declined an offer.
    pub fn decline(&mut self, id: u32) -> Msg {
        self.pending.retain(|p| p.0 != id);
        Msg::FileCancel { id }
    }

    fn ack(&mut self, id: u32) -> Msg {
        let t = self.inc.get_mut(&id).unwrap();
        t.since_ack = 0;
        t.last_ack = Instant::now();
        let mut bits = vec![0u8; (WINDOW / 8) as usize];
        for k in 0..WINDOW {
            let i = t.have_until + k;
            if i < t.chunks && t.have[i as usize] {
                bits[(k / 8) as usize] |= 1 << (k % 8);
            }
        }
        Msg::FileAck { id, have_until: t.have_until, bits }
    }

    /// Call every few ms: returns chunks (and periodic acks) to send now.
    pub fn pump(&mut self) -> Vec<Msg> {
        let mut out = Vec::new();
        // Offers nobody answered: decline them.
        let expired: Vec<u32> = self.pending.iter().filter(|p| p.3.elapsed() > OFFER_TIMEOUT).map(|p| p.0).collect();
        for id in expired {
            out.push(self.decline(id));
        }
        for (&id, t) in self.out.iter_mut().filter(|(_, t)| !t.finished) {
            // Until the receiver answers (it may be asking its user), only a trickle.
            let (window, resend) = if t.got_ack { (WINDOW, RESEND_AFTER) } else { (4, Duration::from_secs(1)) };
            let end = (t.base + window).min(t.chunks);
            for i in t.base..end {
                if out.len() >= MAX_PER_PUMP {
                    break;
                }
                if t.acked[i as usize] || t.sent.get(&i).is_some_and(|s| s.elapsed() < resend) {
                    continue;
                }
                let len = (t.size - i * CHUNK as u64).min(CHUNK as u64) as usize;
                let mut data = vec![0u8; len];
                if t.file.seek(SeekFrom::Start(i * CHUNK as u64)).is_err() || t.file.read_exact(&mut data).is_err() {
                    continue;
                }
                t.sent.insert(i, Instant::now());
                out.push(Msg::FileChunk { id, idx: i, data });
            }
            t.sent.retain(|i, _| *i >= t.base);
        }
        // Keep the sender informed even when chunks stop arriving (lost acks / last chunks).
        let ids: Vec<u32> = self.inc.iter().filter(|(_, t)| t.last_ack.elapsed() > Duration::from_millis(100) && (!t.finished || t.last_ack.elapsed() < Duration::from_secs(2))).map(|(i, _)| *i).collect();
        for id in ids {
            out.push(self.ack(id));
        }
        // Forget finished transfers after a while.
        self.out.retain(|_, t| !t.finished || t.started.elapsed() < Duration::from_secs(3600));
        out
    }

    pub fn cancel(&mut self, id: u32) -> Msg {
        self.on_msg(Msg::FileCancel { id });
        Msg::FileCancel { id }
    }

    /// Active transfers plus ones that finished since the last call to `take_finished`.
    pub fn active(&self) -> Vec<Progress> {
        let mut v: Vec<Progress> = self
            .out
            .iter()
            .filter(|(_, t)| !t.finished)
            .map(|(&id, t)| Progress { id, name: t.name.clone(), sending: true, done: (t.base * CHUNK as u64).min(t.size), size: t.size, finished: false, failed: None, path: None })
            .collect();
        v.extend(self.inc.iter().filter(|(_, t)| !t.finished).map(|(&id, t)| Progress {
            id,
            name: t.name.clone(),
            sending: false,
            done: (t.have_until * CHUNK as u64).min(t.size),
            size: t.size,
            finished: false,
            failed: None,
            path: Some(t.path.clone()),
        }));
        v
    }

    pub fn take_finished(&mut self) -> Vec<Progress> {
        std::mem::take(&mut self.finished)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sends a file through a simulated link that drops ~20% of packets in both directions.
    #[test]
    fn lossy_transfer() {
        let dir = std::env::temp_dir().join(format!("pclone-ft-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("source.bin");
        let data: Vec<u8> = (0..1_000_003u32).map(|i| (i * 7 + i / 13) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let mut a = Transfers::default();
        let mut b = Transfers { save_dir: Some(dir.join("recv")), ..Default::default() };
        let mut to_b = vec![a.send(&src).unwrap()];
        let mut lost = 0u64;
        let mut drop = |n: &mut u64| {
            *n += 1;
            n.wrapping_mul(2654435761) % 5 == 0
        };
        for _ in 0..20_000 {
            let mut to_a = Vec::new();
            for m in to_b.drain(..) {
                if drop(&mut lost) {
                    continue;
                }
                to_a.extend(b.on_msg(m));
            }
            to_a.extend(b.pump());
            for m in to_a {
                if drop(&mut lost) {
                    continue;
                }
                a.on_msg(m);
            }
            to_b.extend(a.pump());
            if b.finished.iter().any(|p| p.finished) && a.finished.iter().any(|p| p.finished) {
                break;
            }
            std::thread::sleep(Duration::from_micros(50));
        }
        let got = b.finished.iter().find(|p| p.finished).expect("receiver finished").path.clone().unwrap();
        assert_eq!(std::fs::read(&got).unwrap(), data, "file arrives intact despite loss");
        assert!(a.finished.iter().any(|p| p.finished), "sender saw completion");
        assert_eq!(got.file_name().unwrap(), "source.bin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn safe_names() {
        let d = std::env::temp_dir();
        assert_eq!(free_name(&d, "..\\..\\evil.exe").file_name().unwrap(), "evil.exe");
        assert!(free_name(&d, "a<b>.txt").to_string_lossy().ends_with("a_b_.txt"));
    }

    #[test]
    fn ask_before_receiving() {
        let dir = std::env::temp_dir().join(format!("pclone-ask-{}", std::process::id()));
        let src = dir.join("hello.txt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&src, b"hi there").unwrap();
        let (mut a, mut b) = (Transfers::default(), Transfers { save_dir: Some(dir.join("in")), ask: Some(true), ..Default::default() });
        let offer = a.send(&src).unwrap();
        assert!(b.on_msg(offer).is_empty(), "no reply until the user decides");
        assert_eq!(b.pending().len(), 1);
        assert!(a.pump().len() <= 4, "sender only trickles before an answer");
        let replies = b.accept(b.pending()[0].0);
        assert!(matches!(replies[0], Msg::FileAck { .. }));
        assert!(b.pending().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
