//! Friends, presence and connection setup over public MQTT brokers.
//! Every message is sealed end-to-end (crypto::Identity::seal) and topics are opaque hashes,
//! so the brokers only relay random-looking bytes. Two brokers at once = no single point of failure.
use crate::crypto::{self, Identity, Pub};
use rumqttc::{Client, Event as MqEvent, MqttOptions, Packet, QoS, Transport};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const BROKERS: &[(&str, u16)] = &[("broker.emqx.io", 8883), ("broker.hivemq.com", 8883)];
const PREFIX: &str = "pc1x";
const ONLINE_FOR: Duration = Duration::from_secs(70);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Friend {
    pub name: String,
    pub tag: String,
    pub public: Pub,
    pub allow_mouse: bool,
    pub allow_keyboard: bool,
    #[serde(default = "yes")]
    pub allow_gamepad: bool,
    pub auto_connect: bool,
    /// Optional local name, e.g. "Ahmad's gaming PC".
    #[serde(default)]
    pub nickname: String,
    /// Optional group, e.g. "Home PCs".
    #[serde(default)]
    pub group: String,
    /// Their profile picture (theme::PICS index + 1, 0 = first letter), as they last told us.
    #[serde(default)]
    pub pic: u8,
    /// Privacy mode: my monitors go black while they're connected to this PC.
    #[serde(default)]
    pub hide_screen: bool,
}

fn yes() -> bool {
    true
}

impl Friend {
    pub fn id(&self) -> String {
        format!("{}#{}", self.name, self.tag)
    }

    /// Nickname if set, else name#1234.
    pub fn display(&self) -> String {
        if self.nickname.trim().is_empty() { self.id() } else { self.nickname.trim().to_string() }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Sig {
    FriendRequest { name: String },
    FriendAccept { name: String },
    FriendRemove,
    Ping { reply: bool },
    Bye,
    Connect { req: u64, eph: Pub, addrs: Vec<SocketAddr> },
    ConnectOk { req: u64, session: u64, eph: Pub, addrs: Vec<SocketAddr> },
    ConnectNo { req: u64, reason: String },
    Cancel { req: u64 },
    /// "I'm now <name>" (same key, so the tag follows from it).
    Renamed { name: String },
    /// My status line ("" = available, "Busy", "Playing FIFA", ...).
    Status { text: String },
    /// A chat message outside sessions; acked so the sender knows it arrived.
    Message { id: u64, text: String, ts: u64 },
    MessageAck { id: u64 },
    /// Connect with an invite code instead of being friends. `name` = the guest's full id.
    GuestConnect { code: String, name: String, req: u64, eph: Pub, addrs: Vec<SocketAddr> },
    /// "Please connect to my PC and help me" (older versions ignore it).
    HelpRequest,
    /// A mini game: the whole move list so far (empty = an invite). See minigames.rs.
    Game { id: u64, kind: u8, moves: Vec<u8> },
    /// Group chat: the room as the sender knows it, plus a message (empty text = only a room update).
    RoomMsg { room: Room, id: u64, text: String, ts: u64 },
    RoomAck { room: u64, id: u64 },
    /// My profile picture (theme::PICS index + 1, 0 = first letter).
    Avatar { pic: u8 },
}

/// A group chat. No server: everyone in it messages everyone else directly.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Room {
    pub id: u64,
    pub name: String,
    /// Everyone in it, me included: (name#tag, key).
    pub members: Vec<(String, Pub)>,
    /// Unix ms of the last change; the newest version wins.
    pub rev: u64,
}

pub const ROOM_MAX: usize = 12;

impl Room {
    pub fn has(&self, p: &Pub) -> bool {
        self.members.iter().any(|(_, m)| m == p)
    }
}

/// An invite this PC handed out: one use, expires after an hour.
#[derive(Clone)]
pub struct Invite {
    pub code: String,
    pub mouse: bool,
    pub keyboard: bool,
    pub gamepad: bool,
    pub expires: Instant,
}

const INVITE_FOR: Duration = Duration::from_secs(3600);

/// "kinlink://join/name%231234-ABCDEF/CODE" (or "name#1234-ABCDEF CODE") -> (host id, code).
pub fn parse_invite(s: &str) -> Option<(String, String)> {
    let s = s.trim().trim_end_matches('/');
    let rest = s.strip_prefix("kinlink://join/").map(|r| r.replace("%23", "#").replace("%20", " ")).unwrap_or_else(|| s.to_string());
    let (id, code) = rest.rsplit_once(['/', ' '])?;
    let (name, tag) = crypto::parse_id(id.trim())?;
    let code = code.trim().to_uppercase();
    (code.len() == 8 && code.chars().all(|c| c.is_ascii_alphanumeric())).then(|| (format!("{name}#{tag}"), code))
}

/// One chat message with a friend.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatMsg {
    pub id: u64,
    pub mine: bool,
    pub text: String,
    /// Unix seconds when it was written.
    pub ts: u64,
    /// Mine: the friend's app confirmed it. Theirs: always true.
    pub delivered: bool,
    /// Group chats: who wrote it (name#tag).
    #[serde(default)]
    pub from: String,
    /// Group chats, mine: members who haven't confirmed it yet.
    #[serde(default)]
    pub waiting: Vec<Pub>,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    ts: u64,
    nonce: u64,
    sig: Sig,
}

#[derive(Serialize, Deserialize)]
struct KeyAnnounce {
    name: String,
    public: Pub,
}

#[derive(Debug)]
pub enum Event {
    IncomingConnect { from: Pub, req: u64, eph: Pub, addrs: Vec<SocketAddr> },
    ConnectReply { from: Pub, sig: Sig },
    Cancelled { from: Pub, req: u64 },
    Toast(String),
    /// A friend just came online (was offline before).
    FriendOnline(Pub),
    /// Someone sent a friend request.
    FriendRequest(String),
    /// A friend sent a chat message.
    Message { from: Pub, text: String },
    /// Someone used one of our invite codes (already checked; one use).
    GuestConnect { from: Pub, name: String, req: u64, eph: Pub, addrs: Vec<SocketAddr>, mouse: bool, keyboard: bool, gamepad: bool },
    /// The host we're joining by invite was found: send it the request now.
    JoinReady { host: Pub, host_id: String },
    /// A friend asked us to connect and help them.
    HelpRequest(Pub),
    /// A friend's mini game state (invite or move).
    Game { from: Pub, id: u64, kind: u8, moves: Vec<u8> },
    /// A group chat message (`from` = the writer's name#tag).
    RoomMessage { room: u64, from: String, text: String },
}

/// Writes a friends file directly (test profiles).
pub fn write_friends(path: &std::path::Path, friends: Vec<Friend>) {
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::write(path, serde_json::to_string_pretty(&Stored { friends, outgoing: vec![] }).unwrap());
}

#[derive(Serialize, Deserialize, Default)]
struct Stored {
    friends: Vec<Friend>,
    outgoing: Vec<String>,
}

struct Outgoing {
    id: String,
    public: Option<Pub>,
    asked: Instant,
    last_sent: Option<Instant>,
    /// Two different keys answered for this id (someone faking it): the user must check the code.
    conflict: bool,
}

/// A friend request we sent, for the UI.
pub struct OutgoingInfo {
    pub id: String,
    /// Safety code, once their key is known.
    pub code: Option<String>,
    pub conflict: bool,
}

#[derive(Default)]
struct State {
    friends: Vec<Friend>,
    last_seen: HashMap<Pub, Instant>,
    incoming: Vec<(String, Pub)>,
    outgoing: Vec<Outgoing>,
    seen: HashMap<u64, Instant>,
    up: Vec<bool>,
    /// Friends' status lines.
    status: HashMap<Pub, String>,
    /// Chat history per friend (oldest first) and unread counts.
    messages: HashMap<Pub, Vec<ChatMsg>>,
    unread: HashMap<Pub, u32>,
    /// Invites we handed out (host) and hosts we're joining by invite (guest: id -> key once known).
    invites: Vec<Invite>,
    joins: Vec<(String, Option<Pub>)>,
    /// When each unconfirmed chat message was last sent.
    msg_tries: HashMap<u64, Instant>,
    /// Group chats (incl. ones I left: I'm not in `members`), their messages and unread counts.
    rooms: Vec<Room>,
    room_msgs: HashMap<u64, Vec<ChatMsg>>,
    room_unread: HashMap<u64, u32>,
    /// Room changes not yet confirmed: (room, their id, their key, last sent).
    room_pending: Vec<(u64, String, Pub, Instant)>,
}

struct Inner {
    me: Identity,
    store: Option<PathBuf>,
    clients: Mutex<Vec<Option<Client>>>,
    st: Mutex<State>,
    events: Mutex<VecDeque<Event>>,
    wake: Box<dyn Fn() + Send + Sync>,
    stop: AtomicBool,
    my_status: Mutex<String>,
    my_pic: Mutex<u8>,
}

#[derive(Clone)]
pub struct Signal(Arc<Inner>);

/// Message name for the log (no keys or addresses).
fn kind(s: &Sig) -> String {
    match s {
        Sig::ConnectNo { reason, .. } => format!("ConnectNo ({reason})"),
        _ => format!("{s:?}").split([' ', '{', '(']).next().unwrap_or_default().to_string(),
    }
}

/// TLS with Mozilla's root list compiled in (same on Windows and Android).
pub(crate) fn tls() -> Transport {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("tls versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Transport::tls_with_config(rumqttc::TlsConfiguration::Rustls(Arc::new(cfg)))
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn clean_room_name(s: &str) -> String {
    let s: String = s.chars().filter(|c| !c.is_control()).take(40).collect();
    if s.trim().is_empty() { "Group chat".into() } else { s.trim().to_string() }
}

fn push_capped(list: &mut Vec<ChatMsg>, m: ChatMsg) {
    list.push(m);
    if list.len() > 500 {
        list.remove(0);
    }
}

/// People who left a room don't need to confirm its messages any more.
fn prune_waiting(st: &mut State, r: &Room) {
    for m in st.room_msgs.get_mut(&r.id).into_iter().flatten().filter(|m| m.mine && !m.waiting.is_empty()) {
        m.waiting.retain(|p| r.has(p));
        m.delivered = m.waiting.is_empty();
    }
}

fn inbox(id: &str) -> String {
    format!("{PREFIX}/{}/i", crypto::topic_for(id))
}

fn keytopic(id: &str) -> String {
    format!("{PREFIX}/{}/k", crypto::topic_for(id))
}

impl Signal {
    /// `store`: friends.json path (None = memory only, for tests).
    pub fn start(me: Identity, store: Option<PathBuf>, wake: impl Fn() + Send + Sync + 'static) -> Signal {
        let mut saved: Stored = store.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        for f in &mut saved.friends {
            f.tag = crypto::tag_for(&f.name, &f.public); // short tags from older versions become long ones
        }
        let st = State {
            friends: saved.friends,
            outgoing: saved.outgoing.into_iter().map(|id| Outgoing { id, public: None, asked: Instant::now(), last_sent: None, conflict: false }).collect(),
            up: vec![false; BROKERS.len()],
            ..Default::default()
        };
        let messages: Vec<(Pub, Vec<ChatMsg>)> = store.as_ref().and_then(|p| std::fs::read_to_string(p.with_file_name("messages.json")).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let (rooms, room_msgs): (Vec<Room>, Vec<(u64, Vec<ChatMsg>)>) = store.as_ref().and_then(|p| std::fs::read_to_string(p.with_file_name("rooms.json")).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let st = State { messages: messages.into_iter().collect(), rooms, room_msgs: room_msgs.into_iter().collect(), ..st };
        let s = Signal(Arc::new(Inner {
            me,
            store,
            clients: Mutex::new(vec![None, None]),
            st: Mutex::new(st),
            events: Mutex::new(VecDeque::new()),
            wake: Box::new(wake),
            stop: AtomicBool::new(false),
            my_status: Mutex::new(String::new()),
            my_pic: Mutex::new(0),
        }));
        for i in 0..BROKERS.len() {
            let s2 = s.clone();
            thread::spawn(move || s2.broker_loop(i));
        }
        let s2 = s.clone();
        thread::spawn(move || s2.ticker());
        s
    }

    pub fn id(&self) -> String {
        self.0.me.id()
    }
    pub fn online(&self) -> bool {
        self.0.st.lock().unwrap().up.iter().any(|u| *u)
    }

    pub fn friends(&self) -> Vec<(Friend, bool)> {
        let st = self.0.st.lock().unwrap();
        st.friends.iter().map(|f| (f.clone(), st.last_seen.get(&f.public).is_some_and(|t| t.elapsed() < ONLINE_FOR))).collect()
    }
    /// A friend's status line ("" = none).
    pub fn status_of(&self, public: &Pub) -> String {
        self.0.st.lock().unwrap().status.get(public).cloned().unwrap_or_default()
    }

    /// Sets my status and tells every friend.
    /// Asks a friend to connect to this PC and help.
    pub fn ask_help(&self, to: &Pub) {
        self.send(to, Sig::HelpRequest);
    }

    /// Sets my profile picture (theme::PICS, 0 = letter) and tells every friend.
    pub fn set_pic(&self, pic: u8) {
        *self.0.my_pic.lock().unwrap() = pic;
        let pubs: Vec<Pub> = self.0.st.lock().unwrap().friends.iter().map(|f| f.public).collect();
        for p in pubs {
            self.send(&p, Sig::Avatar { pic });
        }
    }

    pub fn set_status(&self, text: &str) {
        let text: String = text.trim().chars().take(40).collect();
        *self.0.my_status.lock().unwrap() = text.clone();
        let pubs: Vec<Pub> = self.0.st.lock().unwrap().friends.iter().map(|f| f.public).collect();
        for p in pubs {
            self.send(&p, Sig::Status { text: text.clone() });
        }
    }

    pub fn friend(&self, public: &Pub) -> Option<Friend> {
        self.0.st.lock().unwrap().friends.iter().find(|f| &f.public == public).cloned()
    }
    pub fn incoming(&self) -> Vec<(String, Pub)> {
        self.0.st.lock().unwrap().incoming.clone()
    }
    pub fn outgoing(&self) -> Vec<String> {
        self.0.st.lock().unwrap().outgoing.iter().map(|o| o.id.clone()).collect()
    }
    pub fn outgoing_info(&self) -> Vec<OutgoingInfo> {
        let me = self.0.me.public;
        self.0.st.lock().unwrap().outgoing.iter().map(|o| OutgoingInfo { id: o.id.clone(), code: o.public.map(|p| crypto::safety_code(&me, &p)), conflict: o.conflict }).collect()
    }
    /// The code both sides should see for this person (compare it by phone or in person).
    pub fn safety_code(&self, other: &Pub) -> String {
        crypto::safety_code(&self.0.me.public, other)
    }
    pub fn take_events(&self) -> Vec<Event> {
        self.0.events.lock().unwrap().drain(..).collect()
    }

    fn event(&self, e: Event) {
        self.0.events.lock().unwrap().push_back(e);
        (self.0.wake)();
    }

    fn save_messages(&self) {
        if let Some(p) = &self.0.store {
            let all: Vec<(Pub, Vec<ChatMsg>)> = self.0.st.lock().unwrap().messages.iter().map(|(k, v)| (*k, v.clone())).collect();
            let _ = std::fs::write(p.with_file_name("messages.json"), serde_json::to_string(&all).unwrap_or_default());
        }
    }

    /// Creates a one-use invite (valid 1 hour); returns the link to share.
    pub fn create_invite(&self, mouse: bool, keyboard: bool, gamepad: bool) -> String {
        const B32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let code: String = rand::random::<[u8; 8]>().iter().map(|b| B32[(b & 31) as usize] as char).collect();
        self.0.st.lock().unwrap().invites.push(Invite { code: code.clone(), mouse, keyboard, gamepad, expires: Instant::now() + INVITE_FOR });
        format!("kinlink://join/{}/{code}", self.id().replace('#', "%23"))
    }
    /// Invites still valid.
    pub fn invites(&self) -> Vec<Invite> {
        let mut st = self.0.st.lock().unwrap();
        st.invites.retain(|i| i.expires > Instant::now());
        st.invites.clone()
    }
    pub fn revoke_invite(&self, code: &str) {
        self.0.st.lock().unwrap().invites.retain(|i| i.code != code);
    }
    pub fn invite_link(&self, code: &str) -> String {
        format!("kinlink://join/{}/{code}", self.id().replace('#', "%23"))
    }

    /// Starts joining a host by its invite: looks up its key, then Event::JoinReady.
    pub fn join(&self, host_id: &str) {
        {
            let mut st = self.0.st.lock().unwrap();
            if !st.joins.iter().any(|(id, _)| id.eq_ignore_ascii_case(host_id)) {
                st.joins.push((host_id.to_string(), None));
            }
        }
        if let Some(p) = self.0.st.lock().unwrap().friends.iter().find(|f| f.id().eq_ignore_ascii_case(host_id)).map(|f| f.public) {
            self.0.st.lock().unwrap().joins.iter_mut().for_each(|j| if j.0.eq_ignore_ascii_case(host_id) { j.1 = Some(p) });
            return self.event(Event::JoinReady { host: p, host_id: host_id.to_string() });
        }
        self.lookup(host_id);
    }

    /// Sends to anyone whose id and key we know (invites: the other side isn't a friend).
    pub fn send_to(&self, id: &str, public: &Pub, sig: Sig) {
        self.send_to_id(id, public, sig);
    }

    /// Chat history with a friend, oldest first.
    pub fn messages(&self, with: &Pub) -> Vec<ChatMsg> {
        self.0.st.lock().unwrap().messages.get(with).cloned().unwrap_or_default()
    }

    /// Sends a chat message (kept and re-sent until the friend's app confirms it).
    pub fn send_message(&self, to: &Pub, text: &str) {
        let text: String = text.trim().chars().take(2000).collect();
        if text.is_empty() {
            return;
        }
        let m = ChatMsg { id: rand::random(), mine: true, text: text.clone(), ts: now_s(), delivered: false, from: String::new(), waiting: vec![] };
        let (id, ts) = (m.id, m.ts);
        {
            let mut st = self.0.st.lock().unwrap();
            let list = st.messages.entry(*to).or_default();
            list.push(m);
            if list.len() > 500 {
                list.remove(0);
            }
        }
        self.save_messages();
        self.0.st.lock().unwrap().msg_tries.insert(id, Instant::now());
        self.send(to, Sig::Message { id, text, ts });
    }

    pub fn unread(&self, with: &Pub) -> u32 {
        self.0.st.lock().unwrap().unread.get(with).copied().unwrap_or(0)
    }
    pub fn total_unread(&self) -> u32 {
        let st = self.0.st.lock().unwrap();
        st.unread.values().sum::<u32>() + st.room_unread.values().sum::<u32>()
    }
    pub fn mark_read(&self, with: &Pub) {
        self.0.st.lock().unwrap().unread.remove(with);
    }

    // ------------------------------------------------------------ group chats

    /// Group chats I'm in.
    pub fn rooms(&self) -> Vec<Room> {
        let me = self.0.me.public;
        self.0.st.lock().unwrap().rooms.iter().filter(|r| r.has(&me)).cloned().collect()
    }
    pub fn room(&self, id: u64) -> Option<Room> {
        self.rooms().into_iter().find(|r| r.id == id)
    }
    pub fn room_messages(&self, id: u64) -> Vec<ChatMsg> {
        self.0.st.lock().unwrap().room_msgs.get(&id).cloned().unwrap_or_default()
    }
    pub fn room_unread(&self, id: u64) -> u32 {
        self.0.st.lock().unwrap().room_unread.get(&id).copied().unwrap_or(0)
    }
    pub fn mark_room_read(&self, id: u64) {
        self.0.st.lock().unwrap().room_unread.remove(&id);
    }

    /// Starts a group chat with some friends; returns its id.
    pub fn create_room(&self, name: &str, friends: &[Pub]) -> u64 {
        let mut members = vec![(self.id(), self.0.me.public)];
        members.extend(friends.iter().filter_map(|p| self.friend(p)).map(|f| (f.id(), f.public)).take(ROOM_MAX - 1));
        let room = Room { id: rand::random(), name: clean_room_name(name), members, rev: 0 };
        let id = room.id;
        self.set_room(room);
        id
    }

    /// Adds friends to a group chat.
    pub fn add_to_room(&self, id: u64, friends: &[Pub]) {
        let Some(mut r) = self.room(id) else { return };
        for f in friends.iter().filter_map(|p| self.friend(p)) {
            if !r.has(&f.public) && r.members.len() < ROOM_MAX {
                r.members.push((f.id(), f.public));
            }
        }
        self.set_room(r);
    }

    pub fn leave_room(&self, id: u64) {
        let Some(mut r) = self.room(id) else { return };
        let me = self.0.me.public;
        r.members.retain(|(_, p)| *p != me);
        {
            let mut st = self.0.st.lock().unwrap();
            st.room_msgs.remove(&id);
            st.room_unread.remove(&id);
        }
        self.set_room(r);
    }

    /// Saves a changed room and tells everyone who was or is in it.
    fn set_room(&self, mut r: Room) {
        r.rev = now_ms();
        let tell: Vec<(String, Pub)> = {
            let mut st = self.0.st.lock().unwrap();
            let mut tell = r.members.clone();
            if let Some(old) = st.rooms.iter_mut().find(|x| x.id == r.id) {
                tell.extend(old.members.iter().filter(|(_, p)| !r.has(p)).cloned());
                *old = r.clone();
            } else {
                st.rooms.push(r.clone());
            }
            prune_waiting(&mut st, &r);
            tell.retain(|(_, p)| *p != self.0.me.public);
            // ponytail: kept in memory only; a change not confirmed before quitting reaches people with the next message.
            st.room_pending.retain(|(room, ..)| *room != r.id);
            st.room_pending.extend(tell.iter().map(|(id, p)| (r.id, id.clone(), *p, Instant::now())));
            tell
        };
        self.save_rooms();
        for (id, p) in &tell {
            self.send_to_id(id, p, Sig::RoomMsg { room: r.clone(), id: r.rev, text: String::new(), ts: 0 });
        }
        (self.0.wake)();
    }

    /// Sends a group chat message (re-sent to each member until their app confirms it).
    pub fn send_room(&self, id: u64, text: &str) {
        let text: String = text.trim().chars().take(2000).collect();
        let Some(r) = self.room(id) else { return };
        if text.is_empty() {
            return;
        }
        let me = self.0.me.public;
        let others: Vec<Pub> = r.members.iter().map(|(_, p)| *p).filter(|p| *p != me).collect();
        let m = ChatMsg { id: rand::random(), mine: true, text: text.clone(), ts: now_s(), delivered: others.is_empty(), from: self.id(), waiting: others.clone() };
        let (mid, ts) = (m.id, m.ts);
        {
            let mut st = self.0.st.lock().unwrap();
            push_capped(st.room_msgs.entry(id).or_default(), m);
            st.msg_tries.insert(mid, Instant::now());
        }
        self.save_rooms();
        for p in others {
            self.room_send(&r, &p, Sig::RoomMsg { room: r.clone(), id: mid, text: text.clone(), ts });
        }
    }

    /// Sends to a room member (they may not be my friend).
    fn room_send(&self, r: &Room, to: &Pub, sig: Sig) {
        let id = self.friend(to).map(|f| f.id()).or_else(|| r.members.iter().find(|(_, p)| p == to).map(|(i, _)| i.clone()));
        if let Some(id) = id {
            self.send_to_id(&id, to, sig);
        }
    }

    fn save_rooms(&self) {
        if let Some(p) = &self.0.store {
            let st = self.0.st.lock().unwrap();
            let all = (&st.rooms, st.room_msgs.iter().collect::<Vec<_>>());
            let _ = std::fs::write(p.with_file_name("rooms.json"), serde_json::to_string(&all).unwrap_or_default());
        }
    }

    fn on_room_msg(&self, from: Pub, room: Room, id: u64, text: String, ts: u64) {
        let me = self.0.me.public;
        if room.members.len() > ROOM_MAX || room.members.iter().any(|(i, p)| crypto::parse_id(i).is_none_or(|(n, t)| crypto::tag_for(&n, p) != t)) {
            return; // every id must match its key
        }
        // Their id: from the room they sent, my copy of it (they may have just left) or my friends.
        let mine = self.0.st.lock().unwrap().rooms.iter().find(|r| r.id == room.id).map(|r| r.members.clone()).unwrap_or_default();
        let Some(from_id) = room.members.iter().chain(&mine).find(|(_, p)| *p == from).map(|(i, _)| i.clone()).or_else(|| self.friend(&from).map(|f| f.id())) else { return };
        let is_friend = self.friend(&from).is_some();
        let (current, reply, joined) = {
            let mut st = self.0.st.lock().unwrap();
            match st.rooms.iter().position(|r| r.id == room.id) {
                // A room I don't know: only a friend can add me to one.
                None if room.has(&me) && room.has(&from) && is_friend => {
                    let r = Room { name: clean_room_name(&room.name), ..room.clone() };
                    st.rooms.push(r.clone());
                    (Some(r), None, true)
                }
                None => return,
                Some(i) if !(st.rooms[i].has(&from) || room.has(&from)) => return,
                Some(i) if room.rev > st.rooms[i].rev => {
                    let r = Room { name: clean_room_name(&room.name), ..room.clone() };
                    let joined = !st.rooms[i].has(&me) && r.has(&me);
                    st.rooms[i] = r.clone();
                    prune_waiting(&mut st, &r);
                    (r.has(&me).then(|| r.clone()), None, joined)
                }
                // They have an older version (missed a change, e.g. that I left): send them mine.
                Some(i) if room.rev < st.rooms[i].rev => (st.rooms[i].has(&me).then(|| st.rooms[i].clone()), Some(st.rooms[i].clone()), false),
                Some(i) => (st.rooms[i].has(&me).then(|| st.rooms[i].clone()), None, false),
            }
        };
        if let Some(r) = reply {
            self.send_to_id(&from_id, &from, Sig::RoomMsg { id: r.rev, room: r, text: String::new(), ts: 0 });
        }
        if joined {
            self.event(Event::Toast(format!("{from_id} added you to the group chat \"{}\"", clean_room_name(&room.name))));
        }
        self.save_rooms();
        (self.0.wake)();
        // Confirms the message, or for a room change (no text) its version.
        self.send_to_id(&from_id, &from, Sig::RoomAck { room: room.id, id });
        if text.is_empty() {
            return;
        }
        let Some(r) = current else { return }; // not in it (left): just confirm so they stop re-sending
        let text: String = text.chars().take(2000).collect();
        let new = {
            let mut st = self.0.st.lock().unwrap();
            let list = st.room_msgs.entry(r.id).or_default();
            let new = !list.iter().any(|m| m.id == id && m.from == from_id);
            if new {
                push_capped(list, ChatMsg { id, mine: false, text: text.clone(), ts, delivered: true, from: from_id.clone(), waiting: vec![] });
                list.sort_by_key(|m| m.ts); // late arrivals (they were offline) go in their place
                *st.room_unread.entry(r.id).or_default() += 1;
            }
            new
        };
        if new {
            self.save_rooms();
            self.event(Event::RoomMessage { room: r.id, from: from_id, text });
        }
    }

    fn save(&self) {
        if let Some(p) = &self.0.store {
            let st = self.0.st.lock().unwrap();
            let s = Stored { friends: st.friends.clone(), outgoing: st.outgoing.iter().map(|o| o.id.clone()).collect() };
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(p, serde_json::to_string_pretty(&s).unwrap());
        }
    }

    fn publish(&self, topic: &str, payload: Vec<u8>, retain: bool) {
        for c in self.0.clients.lock().unwrap().iter().flatten() {
            let _ = c.try_publish(topic, QoS::AtLeastOnce, retain, payload.clone());
        }
    }

    fn send_to_id(&self, id: &str, public: &Pub, sig: Sig) {
        if !matches!(sig, Sig::Ping { .. }) {
            let up = self.0.st.lock().unwrap().up.iter().filter(|u| **u).count();
            crate::plog!("signal: send {} to {id} ({up} broker(s) connected)", kind(&sig));
        }
        let w = Wire { ts: now_s(), nonce: rand::random(), sig };
        let sealed = self.0.me.seal(public, &bincode::serialize(&w).unwrap());
        let legacy = crypto::legacy_id(id);
        if legacy != id {
            self.publish(&inbox(&legacy), sealed.clone(), false); // friends on versions before long ids
        }
        self.publish(&inbox(id), sealed, false);
    }

    /// Sends to a friend (no-op for strangers).
    pub fn send(&self, to: &Pub, sig: Sig) {
        if let Some(f) = self.friend(to) {
            self.send_to_id(&f.id(), to, sig);
        }
    }

    pub fn add_friend(&self, id: &str) -> Result<(), String> {
        let Some((name, tag)) = crypto::parse_id(id) else { return Err("Enter it like name#1234-ABCDEF (copy it from their app)".into()) };
        let id = format!("{name}#{tag}");
        if id.eq_ignore_ascii_case(&self.id()) || id.eq_ignore_ascii_case(&crypto::legacy_id(&self.id())) {
            return Err("That's you".into());
        }
        {
            let mut st = self.0.st.lock().unwrap();
            if st.friends.iter().any(|f| f.id().eq_ignore_ascii_case(&id) || crypto::legacy_id(&f.id()).eq_ignore_ascii_case(&id)) {
                return Err("Already friends".into());
            }
            if st.outgoing.iter().any(|o| o.id.eq_ignore_ascii_case(&id)) {
                return Err("Request already sent".into());
            }
            st.outgoing.push(Outgoing { id: id.clone(), public: None, asked: Instant::now(), last_sent: None, conflict: false });
        }
        self.save();
        self.lookup(&id);
        Ok(())
    }

    fn lookup(&self, id: &str) {
        for c in self.0.clients.lock().unwrap().iter().flatten() {
            let _ = c.try_subscribe(keytopic(id), QoS::AtLeastOnce);
        }
    }

    pub fn cancel_request(&self, id: &str) {
        self.0.st.lock().unwrap().outgoing.retain(|o| o.id != id);
        self.save();
    }

    pub fn accept(&self, public: Pub) {
        let req = {
            let mut st = self.0.st.lock().unwrap();
            let Some(i) = st.incoming.iter().position(|(_, p)| *p == public) else { return };
            let (id, p) = st.incoming.remove(i);
            let (name, tag) = crypto::parse_id(&id).unwrap();
            st.friends.push(Friend { name, tag, public: p, allow_mouse: false, allow_keyboard: false, allow_gamepad: true, auto_connect: false, nickname: String::new(), group: String::new(), pic: 0, hide_screen: false });
            st.last_seen.insert(p, Instant::now());
            id
        };
        self.save();
        let name = self.0.me.name.clone();
        self.send_to_id(&req, &public, Sig::FriendAccept { name });
        self.send(&public, Sig::Ping { reply: true });
        (self.0.wake)();
    }

    pub fn decline(&self, public: Pub) {
        self.0.st.lock().unwrap().incoming.retain(|(_, p)| *p != public);
        (self.0.wake)();
    }

    pub fn remove(&self, public: Pub) {
        self.send(&public, Sig::FriendRemove);
        self.0.st.lock().unwrap().friends.retain(|f| f.public != public);
        self.save();
    }

    pub fn update(&self, f: Friend) {
        if let Some(x) = self.0.st.lock().unwrap().friends.iter_mut().find(|x| x.public == f.public) {
            *x = f;
        }
        self.save();
    }

    /// Tells every friend our new name (call before restarting with the renamed identity).
    pub fn tell_renamed(&self, name: &str) {
        let pubs: Vec<Pub> = self.0.st.lock().unwrap().friends.iter().map(|f| f.public).collect();
        for p in pubs {
            self.send(&p, Sig::Renamed { name: name.to_string() });
        }
    }

    pub fn shutdown(&self) {
        let pubs: Vec<Pub> = self.0.st.lock().unwrap().friends.iter().map(|f| f.public).collect();
        for p in pubs {
            self.send(&p, Sig::Bye);
        }
        thread::sleep(Duration::from_millis(300)); // let the Byes flush
        self.0.stop.store(true, SeqCst);
        for c in self.0.clients.lock().unwrap().iter().flatten() {
            let _ = c.disconnect();
        }
    }

    // ------------------------------------------------------------ broker plumbing

    fn broker_loop(&self, i: usize) {
        let (host, port) = BROKERS[i];
        while !self.0.stop.load(SeqCst) {
            let cid = format!("p{:016x}", rand::random::<u64>());
            let mut opts = MqttOptions::new(cid, host, port);
            opts.set_keep_alive(Duration::from_secs(30));
            opts.set_transport(tls());
            let (client, mut conn) = Client::new(opts, 256);
            for ev in conn.iter() {
                if self.0.stop.load(SeqCst) {
                    break;
                }
                match ev {
                    Ok(MqEvent::Incoming(Packet::ConnAck(_))) => {
                        crate::plog!("signal: connected to {host}");
                        self.0.clients.lock().unwrap()[i] = Some(client.clone());
                        self.0.st.lock().unwrap().up[i] = true;
                        self.on_connected(&client);
                        (self.0.wake)();
                    }
                    Ok(MqEvent::Incoming(Packet::Publish(p))) => self.on_publish(&p.topic, &p.payload),
                    Ok(_) => {}
                    Err(e) => {
                        crate::plog!("signal: {host} disconnected: {e}");
                        break;
                    }
                }
            }
            self.0.clients.lock().unwrap()[i] = None;
            self.0.st.lock().unwrap().up[i] = false;
            (self.0.wake)();
            for _ in 0..30 {
                if self.0.stop.load(SeqCst) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    fn on_connected(&self, c: &Client) {
        let me = &self.0.me;
        let _ = c.try_subscribe(inbox(&me.id()), QoS::AtLeastOnce);
        let ann = KeyAnnounce { name: me.name.clone(), public: me.public };
        let _ = c.try_publish(keytopic(&me.id()), QoS::AtLeastOnce, true, bincode::serialize(&ann).unwrap());
        // Old short id: friends on older versions still message it and look us up by it.
        let short = crypto::legacy_id(&me.id());
        let _ = c.try_subscribe(inbox(&short), QoS::AtLeastOnce);
        let _ = c.try_publish(keytopic(&short), QoS::AtLeastOnce, true, bincode::serialize(&ann).unwrap());
        for old in &me.old {
            // Old ids: keep hearing friends who missed the rename; stop new people finding us there.
            let _ = c.try_subscribe(inbox(old), QoS::AtLeastOnce);
            let _ = c.try_publish(keytopic(old), QoS::AtLeastOnce, true, Vec::<u8>::new());
        }
        let pending: Vec<String> = self.0.st.lock().unwrap().outgoing.iter().filter(|o| o.public.is_none()).map(|o| o.id.clone()).collect();
        for id in pending {
            let _ = c.try_subscribe(keytopic(&id), QoS::AtLeastOnce);
        }
        let pubs: Vec<Pub> = self.0.st.lock().unwrap().friends.iter().map(|f| f.public).collect();
        for p in pubs {
            self.send(&p, Sig::Ping { reply: true });
        }
    }

    fn on_publish(&self, topic: &str, payload: &[u8]) {
        if topic.ends_with("/k") {
            return self.on_key(topic, payload);
        }
        let Some((from, pt)) = self.0.me.open(payload) else { return };
        let me_id = self.0.me.id();
        if topic != inbox(&me_id) && topic != inbox(&crypto::legacy_id(&me_id)) && self.friend(&from).is_some() {
            // A friend still uses one of our old ids: tell them the new one.
            let now = self.0.me.name.clone();
            self.send(&from, Sig::Renamed { name: now });
        }
        let Ok(w) = bincode::deserialize::<Wire>(&pt) else { return };
        {
            let mut st = self.0.st.lock().unwrap();
            if now_s().abs_diff(w.ts) > 300 || st.seen.contains_key(&w.nonce) {
                return; // stale or duplicate (we listen on two brokers)
            }
            st.seen.insert(w.nonce, Instant::now());
        }
        self.dispatch(from, w.sig);
    }

    /// A looked-up user's public key arrived; verify it matches name#tag, then send the request.
    fn on_key(&self, topic: &str, payload: &[u8]) {
        let Ok(k) = bincode::deserialize::<KeyAnnounce>(payload) else { return };
        let id = format!("{}#{}", k.name, crypto::tag_for(&k.name, &k.public));
        let short = crypto::legacy_id(&id);
        if keytopic(&id) != topic && keytopic(&short) != topic {
            return; // key doesn't produce this tag: forged or stale
        }
        let mut conflict = None;
        let send = {
            let mut st = self.0.st.lock().unwrap();
            match st.outgoing.iter_mut().find(|o| o.id.eq_ignore_ascii_case(&id) || o.id.eq_ignore_ascii_case(&short)) {
                Some(o) if o.public.is_none() => {
                    o.public = Some(k.public);
                    o.last_sent = Some(Instant::now());
                    Some((o.id.clone(), k.public))
                }
                Some(o) if o.public != Some(k.public) && !o.conflict => {
                    // A second, different key claims the same id: someone is faking it.
                    o.conflict = true;
                    conflict = Some(o.id.clone());
                    None
                }
                _ => None,
            }
        };
        let join = {
            let mut st = self.0.st.lock().unwrap();
            st.joins.iter_mut().find(|j| j.1.is_none() && (j.0.eq_ignore_ascii_case(&id) || j.0.eq_ignore_ascii_case(&short))).map(|j| {
                j.1 = Some(k.public);
                j.0.clone()
            })
        };
        if let Some(host_id) = join {
            self.event(Event::JoinReady { host: k.public, host_id });
        }
        if let Some(id) = conflict {
            crate::plog!("signal: two different PCs claim {id}");
            self.event(Event::Toast(format!("Warning: two different PCs claim to be {id}. Check the safety code with your friend before trusting it.")));
        }
        if let Some((id, p)) = send {
            // Stay subscribed a while: a second key answering means someone is faking this id.
            let name = self.0.me.name.clone();
            self.send_to_id(&id, &p, Sig::FriendRequest { name });
            (self.0.wake)();
        }
    }

    fn dispatch(&self, from: Pub, sig: Sig) {
        let is_friend = self.friend(&from).is_some();
        if !matches!(sig, Sig::Ping { .. }) {
            let who = self.friend(&from).map(|f| f.id()).unwrap_or_else(|| "a stranger".into());
            crate::plog!("signal: got {} from {who}", kind(&sig));
        }
        match sig {
            // Group chats: members needn't be my friends (checked inside).
            Sig::RoomMsg { room, id, text, ts } => self.on_room_msg(from, room, id, text, ts),
            Sig::RoomAck { room, id } => {
                let changed = {
                    let mut st = self.0.st.lock().unwrap();
                    if st.rooms.iter().any(|r| r.id == room && r.rev == id) {
                        st.room_pending.retain(|(r, _, p, _)| !(*r == room && *p == from)); // they have the latest version
                    }
                    st.room_msgs.get_mut(&room).and_then(|l| l.iter_mut().find(|m| m.mine && m.id == id)).filter(|m| m.waiting.contains(&from)).map(|m| {
                        m.waiting.retain(|p| *p != from);
                        m.delivered = m.waiting.is_empty();
                    })
                };
                if changed.is_some() {
                    self.save_rooms();
                    (self.0.wake)();
                }
            }
            Sig::FriendRequest { name } | Sig::FriendAccept { name } if !crypto::valid_name(&name) => {}
            Sig::FriendRequest { name } => {
                let id = format!("{name}#{}", crypto::tag_for(&name, &from));
                if is_friend {
                    let me = self.0.me.name.clone();
                    self.send_to_id(&id, &from, Sig::FriendAccept { name: me });
                    return;
                }
                let mutual = self.0.st.lock().unwrap().outgoing.iter().any(|o| o.public == Some(from));
                if mutual {
                    // We both asked: that's an accept.
                    return self.dispatch(from, Sig::FriendAccept { name });
                }
                let mut st = self.0.st.lock().unwrap();
                let new = !st.incoming.iter().any(|(_, p)| *p == from);
                if new {
                    st.incoming.push((id.clone(), from));
                }
                drop(st);
                if new {
                    self.event(Event::FriendRequest(id));
                } else {
                    (self.0.wake)();
                }
            }
            Sig::FriendAccept { name } => {
                let id = format!("{name}#{}", crypto::tag_for(&name, &from));
                let added = {
                    let mut st = self.0.st.lock().unwrap();
                    let before = st.outgoing.len();
                    let done: Vec<String> = st.outgoing.iter().filter(|o| o.public == Some(from) || o.id.eq_ignore_ascii_case(&id)).map(|o| o.id.clone()).collect();
                    for d in &done {
                        for c in self.0.clients.lock().unwrap().iter().flatten() {
                            let _ = c.try_unsubscribe(keytopic(d));
                        }
                    }
                    st.outgoing.retain(|o| !(o.public == Some(from) || o.id.eq_ignore_ascii_case(&id)));
                    if st.outgoing.len() < before && !st.friends.iter().any(|f| f.public == from) {
                        let tag = crypto::tag_for(&name, &from);
                        st.friends.push(Friend { name, tag, public: from, allow_mouse: false, allow_keyboard: false, allow_gamepad: true, auto_connect: false, nickname: String::new(), group: String::new(), pic: 0, hide_screen: false });
                        st.last_seen.insert(from, Instant::now());
                        true
                    } else {
                        false
                    }
                };
                if added {
                    self.save();
                    self.event(Event::Toast(format!("{id} accepted your friend request")));
                }
            }
            Sig::GuestConnect { code, name, req, eph, addrs } => {
                // Their id must match their key, like any lookup.
                let Some((n, tag)) = crypto::parse_id(&name) else { return };
                if crypto::tag_for(&n, &from) != tag {
                    return;
                }
                let found = {
                    let mut st = self.0.st.lock().unwrap();
                    st.invites.retain(|i| i.expires > Instant::now());
                    st.invites.iter().position(|i| i.code.eq_ignore_ascii_case(&code)).map(|p| st.invites.remove(p)) // one use
                };
                match found {
                    Some(i) => self.event(Event::GuestConnect { from, name, req, eph, addrs, mouse: i.mouse, keyboard: i.keyboard, gamepad: i.gamepad }),
                    None => self.send_to_id(&name, &from, Sig::ConnectNo { req, reason: "says this invite code is wrong, expired or already used".into() }),
                }
            }
            s @ (Sig::ConnectOk { .. } | Sig::ConnectNo { .. }) if !is_friend && self.0.st.lock().unwrap().joins.iter().any(|j| j.1 == Some(from)) => {
                self.event(Event::ConnectReply { from, sig: s })
            }
            _ if !is_friend => {}
            Sig::FriendRemove => {
                self.0.st.lock().unwrap().friends.retain(|f| f.public != from);
                self.save();
                (self.0.wake)();
            }
            Sig::Ping { reply } => {
                let was_online = self.0.st.lock().unwrap().last_seen.insert(from, Instant::now()).is_some_and(|t| t.elapsed() < ONLINE_FOR);
                if reply {
                    self.send(&from, Sig::Ping { reply: false });
                }
                if reply || !was_online {
                    let text = self.0.my_status.lock().unwrap().clone();
                    self.send(&from, Sig::Status { text });
                    let pic = *self.0.my_pic.lock().unwrap();
                    if pic != 0 {
                        self.send(&from, Sig::Avatar { pic });
                    }
                }
                if !was_online {
                    self.event(Event::FriendOnline(from));
                }
            }
            Sig::Bye => {
                self.0.st.lock().unwrap().last_seen.remove(&from);
                (self.0.wake)();
            }
            Sig::Connect { req, eph, addrs } => self.event(Event::IncomingConnect { from, req, eph, addrs }),
            s @ (Sig::ConnectOk { .. } | Sig::ConnectNo { .. }) => self.event(Event::ConnectReply { from, sig: s }),
            Sig::Cancel { req } => self.event(Event::Cancelled { from, req }),
            Sig::Message { id, text, ts } => {
                self.send(&from, Sig::MessageAck { id });
                let text: String = text.chars().take(2000).collect();
                let new = {
                    let mut st = self.0.st.lock().unwrap();
                    let list = st.messages.entry(from).or_default();
                    if list.iter().any(|m| m.id == id && !m.mine) {
                        false // a resend we already have
                    } else {
                        list.push(ChatMsg { id, mine: false, text: text.clone(), ts, delivered: true, from: String::new(), waiting: vec![] });
                        if list.len() > 500 {
                            list.remove(0);
                        }
                        *st.unread.entry(from).or_default() += 1;
                        true
                    }
                };
                if new {
                    self.save_messages();
                    self.event(Event::Message { from, text });
                }
            }
            Sig::HelpRequest => {
                if self.friend(&from).is_some() {
                    self.event(Event::HelpRequest(from));
                }
            }
            Sig::Game { id, kind, moves } => {
                if self.friend(&from).is_some() && moves.len() <= 64 {
                    self.event(Event::Game { from, id, kind, moves });
                }
            }
            Sig::MessageAck { id } => {
                let changed = {
                    let mut st = self.0.st.lock().unwrap();
                    st.messages.get_mut(&from).and_then(|l| l.iter_mut().find(|m| m.mine && m.id == id && !m.delivered)).map(|m| m.delivered = true).is_some()
                };
                if changed {
                    self.save_messages();
                    (self.0.wake)();
                }
            }
            Sig::Avatar { pic } => {
                let changed = self.0.st.lock().unwrap().friends.iter_mut().find(|f| f.public == from && f.pic != pic).map(|f| f.pic = pic).is_some();
                if changed {
                    self.save();
                    (self.0.wake)();
                }
            }
            Sig::Status { text } => {
                let text: String = text.chars().filter(|c| !c.is_control()).take(40).collect();
                self.0.st.lock().unwrap().status.insert(from, text);
                (self.0.wake)();
            }
            Sig::Renamed { name } => {
                if !crypto::valid_name(&name) {
                    return;
                }
                let tag = crypto::tag_for(&name, &from);
                let change = {
                    let mut st = self.0.st.lock().unwrap();
                    st.friends.iter_mut().find(|f| f.public == from && !(f.name == name && f.tag == tag)).map(|f| {
                        let was = f.id();
                        f.name = name.clone();
                        f.tag = tag.clone();
                        (was, f.id())
                    })
                };
                if let Some((was, now)) = change {
                    crate::plog!("signal: {was} is now {now}");
                    self.save();
                    self.event(Event::Toast(format!("{was} is now {now}")));
                }
            }
        }
    }

    /// Presence heartbeat, request retries, housekeeping.
    fn ticker(&self) {
        let mut last_ping = Instant::now();
        while !self.0.stop.load(SeqCst) {
            thread::sleep(Duration::from_millis(500));
            // Re-send chat messages the friend hasn't confirmed (lost on the way, or they were offline).
            let resend: Vec<(Pub, ChatMsg)> = {
                let mut st = self.0.st.lock().unwrap();
                let due: Vec<(Pub, ChatMsg)> = st.messages.iter().flat_map(|(p, l)| l.iter().filter(|m| m.mine && !m.delivered).rev().take(20).map(move |m| (*p, m.clone()))).collect();
                let due: Vec<(Pub, ChatMsg)> = due.into_iter().filter(|(_, m)| st.msg_tries.get(&m.id).is_none_or(|t| t.elapsed() > Duration::from_secs(8))).collect();
                for (_, m) in &due {
                    st.msg_tries.insert(m.id, Instant::now());
                }
                due
            };
            for (p, m) in resend {
                self.send(&p, Sig::Message { id: m.id, text: m.text, ts: m.ts });
            }
            // Same for group chats, to each member still waiting.
            let resend: Vec<(Room, ChatMsg)> = {
                let mut st = self.0.st.lock().unwrap();
                let st = &mut *st;
                let mut due = Vec::new();
                for r in &st.rooms {
                    for m in st.room_msgs.get(&r.id).into_iter().flatten().filter(|m| m.mine && !m.waiting.is_empty()).rev().take(20) {
                        if st.msg_tries.get(&m.id).is_none_or(|t| t.elapsed() > Duration::from_secs(8)) {
                            st.msg_tries.insert(m.id, Instant::now());
                            due.push((r.clone(), m.clone()));
                        }
                    }
                }
                due
            };
            // Unconfirmed room changes (made, joined, left).
            let changes: Vec<(String, Pub, Room)> = {
                let mut st = self.0.st.lock().unwrap();
                let st = &mut *st;
                let mut due = Vec::new();
                for (room, id, p, t) in st.room_pending.iter_mut().filter(|x| x.3.elapsed() > Duration::from_secs(8)) {
                    *t = Instant::now();
                    if let Some(r) = st.rooms.iter().find(|r| r.id == *room) {
                        due.push((id.clone(), *p, r.clone()));
                    }
                }
                due
            };
            for (id, p, r) in changes {
                self.send_to_id(&id, &p, Sig::RoomMsg { id: r.rev, room: r, text: String::new(), ts: 0 });
            }
            for (r, m) in resend {
                for p in &m.waiting {
                    self.room_send(&r, p, Sig::RoomMsg { room: r.clone(), id: m.id, text: m.text.clone(), ts: m.ts });
                }
            }
            if last_ping.elapsed() > Duration::from_secs(25) {
                last_ping = Instant::now();

                let pubs: Vec<Pub> = self.0.st.lock().unwrap().friends.iter().map(|f| f.public).collect();
                for p in pubs {
                    self.send(&p, Sig::Ping { reply: false });
                }
                self.0.st.lock().unwrap().seen.retain(|_, t| t.elapsed() < Duration::from_secs(600));
                (self.0.wake)(); // refresh online dots
                // Re-announce our key: anyone looking us up while a faker also answers sees both
                // keys and gets warned (the broker only keeps the latest retained one).
                let me = &self.0.me;
                let ann = bincode::serialize(&KeyAnnounce { name: me.name.clone(), public: me.public }).unwrap();
                for c in self.0.clients.lock().unwrap().iter().flatten() {
                    let _ = c.try_publish(keytopic(&me.id()), QoS::AtLeastOnce, true, ann.clone());
                    let _ = c.try_publish(keytopic(&crypto::legacy_id(&me.id())), QoS::AtLeastOnce, true, ann.clone());
                }
            }
            // Re-send pending friend requests (the other side may have been offline), re-lookup unknown keys.
            let (resend, relook) = {
                let mut st = self.0.st.lock().unwrap();
                let mut resend = Vec::new();
                let mut relook = Vec::new();
                for o in st.outgoing.iter_mut() {
                    match o.public {
                        Some(p) if o.last_sent.is_none_or(|t| t.elapsed() > Duration::from_secs(30)) => {
                            o.last_sent = Some(Instant::now());
                            resend.push((o.id.clone(), p));
                        }
                        None if o.asked.elapsed() > Duration::from_secs(20) => {
                            o.asked = Instant::now();
                            relook.push(o.id.clone());
                        }
                        _ => {}
                    }
                }
                (resend, relook)
            };
            for (id, p) in resend {
                let name = self.0.me.name.clone();
                self.send_to_id(&id, &p, Sig::FriendRequest { name });
            }
            for id in relook {
                self.lookup(&id);
            }
        }
    }
}
