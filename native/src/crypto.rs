//! Identity (name#1234 bound to an X25519 key), friend-to-friend message sealing, and the
//! encrypted UDP session. No logins: a PC's secret key *is* its account.
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, XChaCha20Poly1305};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::sync::Mutex;
use x25519_dalek::{PublicKey, StaticSecret};

pub type Pub = [u8; 32];

fn sha(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

pub fn random32() -> [u8; 32] {
    rand::random()
}

/// Crockford base32 (no I, L, O, U: nothing to misread).
const B32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Tag derived from name + key, "1234-K7QF2M": 4 digits (the old short tag) plus 6 base32
/// characters, about 10^13 combinations, so nobody can brute-force a key that fakes your ID.
/// After pairing, friends are pinned to the key anyway.
pub fn tag_for(name: &str, public: &Pub) -> String {
    let h = sha(&[b"pc1-tag", name.to_lowercase().as_bytes(), public]);
    let digits = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) % 10000;
    let mut x = u64::from_le_bytes(h[4..12].try_into().unwrap());
    let tail: String = (0..6)
        .map(|_| {
            let c = B32[(x & 31) as usize] as char;
            x >>= 5;
            c
        })
        .collect();
    format!("{digits:04}-{tail}")
}

/// The short "name#1234" form older versions use (still listened on during the switch).
pub fn legacy_id(id: &str) -> String {
    match id.rsplit_once('#') {
        Some((name, tag)) if tag.len() > 4 => format!("{name}#{}", &tag[..4]),
        _ => id.to_string(),
    }
}

/// Splits "name#1234-K7QF2M" (or the old "name#1234"); the tag comes back upper-case.
pub fn parse_id(s: &str) -> Option<(String, String)> {
    let (name, tag) = s.trim().rsplit_once('#')?;
    let tag = tag.trim().to_uppercase();
    let b = tag.as_bytes();
    let short = b.len() == 4 && b.iter().all(|c| c.is_ascii_digit());
    let long = b.len() == 11 && b[..4].iter().all(|c| c.is_ascii_digit()) && b[4] == b'-' && b[5..].iter().all(|c| B32.contains(c));
    (valid_name(name) && (short || long)).then(|| (name.to_string(), tag))
}

/// Code both friends see when pairing: if it matches on both screens, nobody is in between.
pub fn safety_code(a: &Pub, b: &Pub) -> String {
    let (x, y) = if a < b { (a, b) } else { (b, a) };
    let h = sha(&[b"pc1-safety", x, y]);
    let mut v = u64::from_le_bytes(h[..8].try_into().unwrap());
    let s: String = (0..8)
        .map(|_| {
            let c = B32[(v & 31) as usize] as char;
            v >>= 5;
            c
        })
        .collect();
    format!("{} {}", &s[..4], &s[4..])
}

/// Names people pick from now on (setup / rename): letters and digits only.
pub fn valid_new_name(n: &str) -> bool {
    (2..=20).contains(&n.chars().count()) && n.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Minimum time between username changes (stops impersonation games and spam).
/// PCLONE_NO_RENAME_LIMIT=1 turns it off for testing.
pub const RENAME_COOLDOWN_S: u64 = 90 * 86_400;

pub fn valid_name(n: &str) -> bool {
    (2..=20).contains(&n.chars().count()) && n.chars().all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
}

/// Opaque broker topic for an id, so observers can't tell who is online.
pub fn topic_for(id: &str) -> String {
    let h = sha(&[b"pc1-topic", id.to_lowercase().as_bytes()]);
    h[..12].iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone)]
pub struct Identity {
    pub name: String,
    secret: StaticSecret,
    pub public: Pub,
    /// Previous name#tag ids (newest last): still listened on, so friends who missed the
    /// rename get told the new name the next time they message the old one.
    pub old: Vec<String>,
    /// Unix seconds of the last rename (0 = never).
    pub renamed_at: u64,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    name: String,
    secret: Vec<u8>, // DPAPI-protected
    #[serde(default)]
    old: Vec<String>,
    #[serde(default)]
    renamed_at: u64,
}

impl Identity {
    pub fn new(name: &str) -> Self {
        let secret = StaticSecret::from(random32());
        let public = PublicKey::from(&secret).to_bytes();
        Self { name: name.to_string(), secret, public, old: Vec::new(), renamed_at: 0 }
    }

    /// Same key, new name (so a new tag). Friends stay friends: they know us by key.
    pub fn renamed(&self, name: &str, now: u64) -> Self {
        let mut old = self.old.clone();
        old.retain(|o| !o.eq_ignore_ascii_case(&self.id()));
        old.push(self.id());
        let next = format!("{name}#{}", tag_for(name, &self.public));
        old.retain(|o| !o.eq_ignore_ascii_case(&next)); // renamed back to an old name
        if old.len() > 10 {
            old.remove(0);
        }
        Self { name: name.to_string(), secret: self.secret.clone(), public: self.public, old, renamed_at: now }
    }

    /// Seconds until another rename is allowed (0 = now).
    pub fn rename_wait(&self, now: u64) -> u64 {
        if self.renamed_at == 0 || std::env::var_os("PCLONE_NO_RENAME_LIMIT").is_some() {
            return 0;
        }
        (self.renamed_at + RENAME_COOLDOWN_S).saturating_sub(now)
    }

    pub fn tag(&self) -> String {
        tag_for(&self.name, &self.public)
    }

    pub fn id(&self) -> String {
        format!("{}#{}", self.name, self.tag())
    }

    fn path() -> Option<std::path::PathBuf> {
        Some(crate::settings::data_dir()?.join("identity.json"))
    }

    pub fn load() -> Option<Self> {
        let s: Stored = serde_json::from_str(&std::fs::read_to_string(Self::path()?).ok()?).ok()?;
        let raw: [u8; 32] = dpapi(&s.secret, false)?.try_into().ok()?;
        let secret = StaticSecret::from(raw);
        let public = PublicKey::from(&secret).to_bytes();
        Some(Self { name: s.name, secret, public, old: s.old, renamed_at: s.renamed_at })
    }

    pub fn save(&self) -> bool {
        let Some(p) = Self::path() else { return false };
        let Some(secret) = dpapi(&self.secret.to_bytes(), true) else { return false };
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        std::fs::write(p, serde_json::to_string(&Stored { name: self.name.clone(), secret, old: self.old.clone(), renamed_at: self.renamed_at }).unwrap()).is_ok()
    }

    fn shared_key(&self, other: &Pub, ctx: &[u8]) -> [u8; 32] {
        let dh = self.secret.diffie_hellman(&PublicKey::from(*other));
        let (a, b) = if self.public < *other { (self.public, *other) } else { (*other, self.public) };
        sha(&[ctx, dh.as_bytes(), &a, &b])
    }

    /// Encrypts + authenticates for one recipient: only `to` can read it, and only we could have made it.
    pub fn seal(&self, to: &Pub, plaintext: &[u8]) -> Vec<u8> {
        let key = self.shared_key(to, b"pc1-sig");
        let nonce: [u8; 24] = rand::random();
        let ct = XChaCha20Poly1305::new(&key.into()).encrypt(&nonce.into(), Payload { msg: plaintext, aad: &self.public }).expect("encrypt");
        [&self.public[..], &nonce, &ct].concat()
    }

    /// Returns (sender public key, plaintext) if the message was sealed for us.
    pub fn open(&self, data: &[u8]) -> Option<(Pub, Vec<u8>)> {
        if data.len() < 32 + 24 + 16 {
            return None;
        }
        let from: Pub = data[..32].try_into().ok()?;
        let nonce: [u8; 24] = data[32..56].try_into().ok()?;
        let key = self.shared_key(&from, b"pc1-sig");
        let pt = XChaCha20Poly1305::new(&key.into()).decrypt(&nonce.into(), Payload { msg: &data[56..], aad: &from }).ok()?;
        Some((from, pt))
    }
}

/// Android: the identity file lives in the app's private storage (sandboxed per app).
#[cfg(not(windows))]
fn dpapi(data: &[u8], _protect: bool) -> Option<Vec<u8>> {
    Some(data.to_vec())
}

/// Windows DPAPI: ties the stored secret to this Windows user account.
#[cfg(windows)]
fn dpapi(data: &[u8], protect: bool) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB};
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        let ok = if protect {
            CryptProtectData(&input, None, None, None, None, 0, &mut out)
        } else {
            CryptUnprotectData(&input, None, None, None, None, 0, &mut out)
        };
        ok.ok()?;
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
        Some(v)
    }
}

// ---------------------------------------------------------------- UDP session

pub const PKT_DATA: u8 = 0xC5;
pub const PKT_PUNCH: u8 = 0xC6;
const HDR: usize = 1 + 8 + 8;

/// Fresh per-session keypair; the public half travels inside the (authenticated) signaling message.
pub struct Ephemeral {
    secret: StaticSecret,
    pub public: Pub,
}

impl Ephemeral {
    pub fn new() -> Self {
        let secret = StaticSecret::from(random32());
        let public = PublicKey::from(&secret).to_bytes();
        Self { secret, public }
    }

    /// Derives the two directional session ciphers.
    pub fn cipher(&self, theirs: &Pub, session: u64, i_am_client: bool) -> Cipher {
        let dh = self.secret.diffie_hellman(&PublicKey::from(*theirs));
        let s = session.to_le_bytes();
        let c2h = sha(&[b"pc1-c2h", dh.as_bytes(), &s]);
        let h2c = sha(&[b"pc1-h2c", dh.as_bytes(), &s]);
        let (tx, rx) = if i_am_client { (c2h, h2c) } else { (h2c, c2h) };
        Cipher {
            session,
            tx: ChaCha20Poly1305::new(&tx.into()),
            rx: ChaCha20Poly1305::new(&rx.into()),
            ctr: AtomicU64::new(1),
            seen: Mutex::new(Replay::default()),
        }
    }
}

#[derive(Default)]
struct Replay {
    max: u64,
    recent: HashSet<u64>,
}

impl Replay {
    /// Accepts each counter once, within a 4096-packet window (UDP may reorder).
    fn check(&mut self, n: u64) -> bool {
        if n + 4096 <= self.max || !self.recent.insert(n) {
            return false;
        }
        if n > self.max {
            self.max = n;
        }
        if self.recent.len() > 8192 {
            let floor = self.max.saturating_sub(4096);
            self.recent.retain(|&c| c > floor);
        }
        true
    }
}

pub struct Cipher {
    pub session: u64,
    tx: ChaCha20Poly1305,
    rx: ChaCha20Poly1305,
    ctr: AtomicU64,
    seen: Mutex<Replay>,
}

fn nonce(ctr: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&ctr.to_le_bytes());
    n
}

impl Cipher {
    /// [0xC5][session][counter][ciphertext+tag]
    pub fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let ctr = self.ctr.fetch_add(1, SeqCst);
        let mut hdr = Vec::with_capacity(HDR + plaintext.len() + 16);
        hdr.push(PKT_DATA);
        hdr.extend_from_slice(&self.session.to_le_bytes());
        hdr.extend_from_slice(&ctr.to_le_bytes());
        let ct = self.tx.encrypt(&nonce(ctr).into(), Payload { msg: plaintext, aad: &hdr }).expect("encrypt");
        hdr.extend_from_slice(&ct);
        hdr
    }

    pub fn open(&self, pkt: &[u8]) -> Option<Vec<u8>> {
        if pkt.len() < HDR + 16 || pkt[0] != PKT_DATA || session_of(pkt)? != self.session {
            return None;
        }
        let ctr = u64::from_le_bytes(pkt[9..17].try_into().ok()?);
        let pt = self.rx.decrypt(&nonce(ctr).into(), Payload { msg: &pkt[HDR..], aad: &pkt[..HDR] }).ok()?;
        self.seen.lock().unwrap().check(ctr).then_some(pt)
    }
}

/// Session id of a data or punch packet.
pub fn session_of(pkt: &[u8]) -> Option<u64> {
    (pkt.len() >= 9 && (pkt[0] == PKT_DATA || pkt[0] == PKT_PUNCH)).then(|| u64::from_le_bytes(pkt[1..9].try_into().unwrap()))
}

pub fn punch_packet(session: u64) -> Vec<u8> {
    let mut p = vec![PKT_PUNCH];
    p.extend_from_slice(&session.to_le_bytes());
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        let a = Identity::new("Abood");
        assert_eq!(a.tag().len(), 11);
        assert_eq!(parse_id(&a.id()), Some(("Abood".into(), a.tag())));
        assert_eq!(parse_id(&a.id().to_lowercase()).map(|x| x.1), Some(a.tag()), "tag is case-insensitive");
        assert_eq!(parse_id(&legacy_id(&a.id())).map(|x| x.1), Some(a.tag()[..4].to_string()), "old short ids still parse");
        assert_eq!(topic_for("abood#0001"), topic_for("ABOOD#0001"));
        assert!(parse_id("x#1234").is_none() && parse_id("abood#12a4").is_none() && parse_id("abood").is_none());
        assert!(parse_id("abood#1234-ILOU00").is_none(), "no look-alike letters");
        let b = Identity::new("bravo");
        assert_eq!(safety_code(&a.public, &b.public), safety_code(&b.public, &a.public));
        assert_ne!(safety_code(&a.public, &b.public), safety_code(&a.public, &Identity::new("x").public));
        assert_eq!(safety_code(&a.public, &b.public).len(), 9);
    }

    #[test]
    fn seal_open() {
        let (a, b, c) = (Identity::new("aa"), Identity::new("bb"), Identity::new("cc"));
        let m = a.seal(&b.public, b"hi bob");
        assert_eq!(b.open(&m), Some((a.public, b"hi bob".to_vec())));
        assert!(c.open(&m).is_none(), "third party must not read it");
        let mut forged = m.clone();
        forged[..32].copy_from_slice(&c.public); // claim to be someone else
        assert!(b.open(&forged).is_none());
    }

    #[test]
    fn session_cipher() {
        let (ce, he) = (Ephemeral::new(), Ephemeral::new());
        let client = ce.cipher(&he.public, 77, true);
        let host = he.cipher(&ce.public, 77, false);
        let p = client.seal(b"input");
        assert_eq!(host.open(&p).as_deref(), Some(&b"input"[..]));
        assert!(host.open(&p).is_none(), "replay rejected");
        assert!(client.open(&p).is_none(), "wrong direction rejected");
        let mut tampered = client.seal(b"input");
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(host.open(&tampered).is_none());
        assert_eq!(client.open(&host.seal(b"video")).as_deref(), Some(&b"video"[..]));
    }

    #[test]
    #[cfg(windows)]
    fn dpapi_roundtrip() {
        let enc = dpapi(b"secret", true).unwrap();
        assert_ne!(enc, b"secret");
        assert_eq!(dpapi(&enc, false).unwrap(), b"secret");
    }

    #[test]
    fn rename_keeps_key_changes_tag() {
        let a = Identity::new("alpha");
        let b = a.renamed("bravo", 100);
        assert_eq!(a.public, b.public);
        assert_eq!(b.tag(), tag_for("bravo", &a.public));
        assert_eq!(b.old, vec![a.id()]);
        assert_eq!(b.renamed("bravo2", 200).old, vec![a.id(), b.id()]);
        assert!(valid_new_name("Bob42") && !valid_new_name("bob_42") && !valid_new_name("b"));
    }
}
