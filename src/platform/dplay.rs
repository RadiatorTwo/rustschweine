//! DirectPlay (IDirectPlay4A) for the game's network play, over our own transport; taken
//! from future_rust_cop, not yet tried with Hogs of War. The game also creates a
//! DirectPlayLobby (IDirectPlayLobby3A): it finds no lobby (no game was started by one).
//!
//! Transport: sessions are found by UDP (a broadcast on the local network, to this computer,
//! or to the address typed into the Join dialog); everything else goes over one TCP connection
//! per player that joins, to the host, which passes messages on to the others. Port 47624 (the
//! one DirectPlay used) for both; `host:port` in the Join dialog and `HOGS_NET_PORT` choose
//! another, `HOGS_NET_BIND` the address the host listens on. `HOGS_NET_LOG=1` logs.

use super::{Import, alloc, arg, cstr, free, guest_call, kernel32, log, mem};
use crate::rt::Cpu;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const DP_OK: u32 = 0;
const E_NOTIMPL: u32 = 0x8000_4001;
const E_NOINTERFACE: u32 = 0x8000_4002;
const DPERR_INVALIDPARAMS: u32 = 0x8007_0057;
const DPERR_ALREADYINITIALIZED: u32 = 0x8877_0005;
const DPERR_BUFFERTOOSMALL: u32 = 0x8877_001e;
const DPERR_CANTCREATESESSION: u32 = 0x8877_0046;
const DPERR_INVALIDPLAYER: u32 = 0x8877_0096;
const DPERR_NOCONNECTION: u32 = 0x8877_00aa;
const DPERR_NOMESSAGES: u32 = 0x8877_00be;
const DPERR_NOSESSIONS: u32 = 0x8877_00dc;
const DPERR_TIMEOUT: u32 = 0x8877_00f0;
const DPERR_UNINITIALIZED: u32 = 0x8877_0140;
const DPERR_NONEWPLAYERS: u32 = 0x8877_014a;

const DPOPEN_JOIN: u32 = 1;
const DPOPEN_CREATE: u32 = 2;
const DPRECEIVE_TOPLAYER: u32 = 2;
const DPRECEIVE_FROMPLAYER: u32 = 4;
const DPRECEIVE_PEEK: u32 = 8;
const DPENUMSESSIONS_AVAILABLE: u32 = 1;
const DPENUMSESSIONS_ASYNC: u32 = 0x10;
const DPESC_TIMEDOUT: u32 = 1;
/// sender of system messages, and the receiver "all players"
const DPID_SYSMSG: u32 = 0;
const DPID_ALLPLAYERS: u32 = 0;

const DPSYS_CREATEPLAYERORGROUP: u32 = 3;
const DPSYS_DESTROYPLAYERORGROUP: u32 = 5;
const DPSYS_SESSIONLOST: u32 = 0x31;

/// A GUID as its 16 bytes in memory.
const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> [u8; 16] {
    let (a, b, c) = (d1.to_le_bytes(), d2.to_le_bytes(), d3.to_le_bytes());
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]]
}

pub const CLSID_DIRECTPLAY: [u8; 16] = guid(0xd1eb6d20, 0x8923, 0x11d0, [0x9d, 0x97, 0x00, 0xa0, 0xc9, 0x0a, 0x43, 0xcb]);
const IID_IUNKNOWN: [u8; 16] = guid(0, 0, 0, [0xc0, 0, 0, 0, 0, 0, 0, 0x46]);
const IID_IDIRECTPLAY4A: [u8; 16] = guid(0x0ab1c531, 0x4745, 0x11d1, [0xa7, 0xa1, 0x00, 0x00, 0xf8, 0x03, 0xab, 0xfc]);
/// the TCP/IP service provider, and the address parts of a connection
const DPSPGUID_TCPIP: [u8; 16] = guid(0x36e95ee0, 0x8577, 0x11cf, [0x96, 0x0c, 0x00, 0x80, 0xc7, 0x53, 0x4e, 0x82]);
const DPAID_TOTALSIZE: [u8; 16] = guid(0x1318f560, 0x912c, 0x11d0, [0x9d, 0xaa, 0x00, 0xa0, 0xc9, 0x0a, 0x43, 0xcb]);
const DPAID_SERVICEPROVIDER: [u8; 16] = guid(0x07d916c0, 0xe0af, 0x11cf, [0x9c, 0x4e, 0x00, 0xa0, 0xc9, 0x05, 0x42, 0x5e]);

/// The one connection we offer.
const CONNECTION_NAME: &str = "TCP/IP (local network or Internet)";
const DEFAULT_PORT: u16 = 47624;
/// UDP messages that look for sessions and answer
const ASK: &[u8; 8] = b"HOGSNET?";
const ANSWER: &[u8; 8] = b"HOGSNET!";
/// sessions not heard of for this long are gone
const FOUND_FOR: Duration = Duration::from_secs(7);

// TCP frames: length (u32, of what follows), kind (u8), contents
const HELLO: u8 = 1; // instance GUID: the joining side
const WELCOME: u8 = 2;
const REFUSED: u8 = 3;
const PLAYER_ADD: u8 = 4; // id, existing before the join (u8), name
const PLAYER_DEL: u8 = 5; // id
const DATA: u8 = 6; // from, to, bytes

/// the connection to the host, among `Peer` ids (0 is this computer)
const TO_HOST: u32 = u32::MAX;

fn trace() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var("HOGS_NET_LOG").is_ok_and(|v| v != "0"))
}

macro_rules! net_log {
    ($($t:tt)*) => { if trace() { log!("net: {}", format!($($t)*)) } };
}

fn port() -> u16 {
    std::env::var("HOGS_NET_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT)
}

/// Not cryptographic: ids and session GUIDs that differ between computers and runs.
fn random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.finish()
}

// ---- state --------------------------------------------------------------------------------------

/// What a message carries.
enum Body {
    Data(Vec<u8>),
    Created { id: u32, name: String, players: u32 },
    Destroyed { id: u32, name: String },
    SessionLost,
}

struct Message {
    from: u32,
    to: u32,
    body: Body,
}

struct Player {
    id: u32,
    name: String,
    /// on this computer: its event (0 for none)
    local: Option<u32>,
    /// the host: the connection it came over
    peer: u32,
}

/// A TCP connection: to the host (`TO_HOST`), or (on the host) to a player's computer (from 1).
struct Peer {
    id: u32,
    out: Arc<Mutex<TcpStream>>,
}

struct Session {
    /// changes with every session: threads of an old one find out by it
    serial: u32,
    host: bool,
    instance: [u8; 16],
    app: [u8; 16],
    max: u32,
    flags: u32,
    name: String,
    players: Vec<Player>,
    peers: Vec<Peer>,
    queue: VecDeque<Message>,
    stop: Arc<AtomicBool>,
}

/// A session some host answered with.
struct Found {
    instance: [u8; 16],
    app: [u8; 16],
    current: u32,
    max: u32,
    flags: u32,
    name: String,
    addr: SocketAddr,
    seen: Instant,
}

/// The UDP socket that asks for sessions; its thread collects the answers.
struct Finder {
    socket: UdpSocket,
    stop: Arc<AtomicBool>,
}

#[derive(Default)]
struct Net {
    /// the guest object (one at a time: the game releases the old one before creating another)
    object: u32,
    refs: u32,
    initialized: bool,
    session: Option<Session>,
    found: Vec<Found>,
    finder: Option<Finder>,
    /// whether EnumSessions has asked before (the first asynchronous call waits a little)
    asked: bool,
    serial: u32,
}

static NET: Mutex<Option<Net>> = Mutex::new(None);

fn net() -> MutexGuard<'static, Option<Net>> {
    NET.lock().unwrap_or_else(|e| e.into_inner())
}

impl Session {
    /// Queue a message for each local player it is for (`to` 0: all of them but `from`), and
    /// set their events.
    fn deliver(&mut self, from: u32, to: u32, body: impl Fn() -> Body) {
        let targets: Vec<(u32, u32)> = self
            .players
            .iter()
            .filter_map(|p| p.local.map(|e| (p.id, e)))
            .filter(|&(id, _)| if to == DPID_ALLPLAYERS { id != from } else { id == to })
            .collect();
        for (id, event) in targets {
            self.queue.push_back(Message { from, to: id, body: body() });
            if event != 0 {
                kernel32::set_event_state(event, true);
            }
        }
    }

    /// Connections a message from `origin` (0: from here) to `to` goes on to: the host passes
    /// messages on, the others send everything to the host.
    fn routes(&self, origin: u32, to: u32) -> Vec<Arc<Mutex<TcpStream>>> {
        self.peers
            .iter()
            .filter(|p| p.id != origin)
            .filter(|p| !self.host || to == DPID_ALLPLAYERS || self.players.iter().any(|q| q.id == to && q.peer == p.id && q.local.is_none()))
            .map(|p| p.out.clone())
            .collect()
    }

    fn close(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for p in &self.peers {
            let _ = p.out.lock().unwrap_or_else(|e| e.into_inner()).shutdown(Shutdown::Both);
        }
        self.peers.clear();
    }
}

// ---- frames -------------------------------------------------------------------------------------

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + body.len());
    f.extend_from_slice(&(body.len() as u32 + 1).to_le_bytes());
    f.push(kind);
    f.extend_from_slice(body);
    f
}

fn send_to(outs: &[Arc<Mutex<TcpStream>>], f: &[u8]) {
    for o in outs {
        if let Err(e) = o.lock().unwrap_or_else(|e| e.into_inner()).write_all(f) {
            net_log!("send failed: {e}");
        }
    }
}

fn read_frame(s: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut n = [0; 4];
    s.read_exact(&mut n)?;
    let n = u32::from_le_bytes(n) as usize;
    if n == 0 || n > 1 << 20 {
        return Err(std::io::Error::other("bad frame"));
    }
    let mut b = vec![0; n];
    s.read_exact(&mut b)?;
    let kind = b.remove(0);
    Ok((kind, b))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}

fn text_of(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

fn player_add(id: u32, existing: bool, name: &str) -> Vec<u8> {
    let mut b = id.to_le_bytes().to_vec();
    b.push(existing as u8);
    b.extend(name.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }));
    frame(PLAYER_ADD, &b)
}

/// A connection's thread: frames in until it closes.
fn reader(serial: u32, peer: u32, mut s: TcpStream) {
    loop {
        let Ok((kind, b)) = read_frame(&mut s) else { break };
        if !handle(serial, peer, &s, kind, &b) {
            break;
        }
    }
    let _ = s.shutdown(Shutdown::Both);
    lost(serial, peer);
}

/// One frame from connection `peer`; false ends the connection.
fn handle(serial: u32, peer: u32, s: &TcpStream, kind: u8, b: &[u8]) -> bool {
    let mut g = net();
    let Some(ss) = g.as_mut().and_then(|n| n.session.as_mut()).filter(|ss| ss.serial == serial) else { return false };
    match kind {
        HELLO if ss.host => {
            let joined = ss.peers.iter().filter(|p| p.id != peer).count() as u32;
            if b.get(..16) != Some(&ss.instance[..]) || joined + 1 >= ss.max.max(2) {
                net_log!("player refused");
                let _ = (&*s).write_all(&frame(REFUSED, &[]));
                return false;
            }
            let Ok(out) = s.try_clone() else { return false };
            let out = Arc::new(Mutex::new(out));
            let mut f = frame(WELCOME, &[]);
            for p in &ss.players {
                f.extend(player_add(p.id, true, &p.name));
            }
            send_to(std::slice::from_ref(&out), &f);
            ss.peers.push(Peer { id: peer, out });
            net_log!("player's computer {peer} joined");
        }
        PLAYER_ADD => {
            let (id, existing, name) = (u32_at(b, 0), b.get(4) == Some(&1), text_of(b.get(5..).unwrap_or(&[])));
            if id == 0 || ss.players.iter().any(|p| p.id == id) {
                return true;
            }
            net_log!("player {id:#x} {name:?} added");
            ss.players.push(Player { id, name: name.clone(), local: None, peer });
            if !existing {
                let players = ss.players.len() as u32;
                ss.deliver(DPID_SYSMSG, DPID_ALLPLAYERS, || Body::Created { id, name: name.clone(), players });
            }
            if ss.host {
                let outs = ss.routes(peer, DPID_ALLPLAYERS);
                send_to(&outs, &player_add(id, existing, &name));
            }
        }
        PLAYER_DEL => {
            let id = u32_at(b, 0);
            gone(ss, id);
            if ss.host {
                let outs = ss.routes(peer, DPID_ALLPLAYERS);
                send_to(&outs, &frame(PLAYER_DEL, &id.to_le_bytes()));
            }
        }
        DATA => {
            let (from, to) = (u32_at(b, 0), u32_at(b, 4));
            let data = b.get(8..).unwrap_or(&[]).to_vec();
            if ss.host {
                let outs = ss.routes(peer, to);
                let mut body = from.to_le_bytes().to_vec();
                body.extend_from_slice(&to.to_le_bytes());
                body.extend_from_slice(&data);
                send_to(&outs, &frame(DATA, &body));
            }
            ss.deliver(from, to, || Body::Data(data.clone()));
        }
        _ => {}
    }
    true
}

/// A player left: its destroy message for the local players.
fn gone(ss: &mut Session, id: u32) {
    if let Some(k) = ss.players.iter().position(|p| p.id == id && p.local.is_none()) {
        let p = ss.players.remove(k);
        net_log!("player {id:#x} {:?} left", p.name);
        ss.deliver(DPID_SYSMSG, DPID_ALLPLAYERS, || Body::Destroyed { id, name: p.name.clone() });
    }
}

/// A connection closed: on the host its players left, elsewhere the session is lost.
fn lost(serial: u32, peer: u32) {
    let mut g = net();
    let Some(ss) = g.as_mut().and_then(|n| n.session.as_mut()).filter(|ss| ss.serial == serial) else { return };
    if ss.stop.load(Ordering::Relaxed) {
        return;
    }
    net_log!("connection {peer} closed");
    ss.peers.retain(|p| p.id != peer);
    if ss.host {
        let ids: Vec<u32> = ss.players.iter().filter(|p| p.peer == peer && p.local.is_none()).map(|p| p.id).collect();
        for id in ids {
            gone(ss, id);
            let outs = ss.routes(peer, DPID_ALLPLAYERS);
            send_to(&outs, &frame(PLAYER_DEL, &id.to_le_bytes()));
        }
    } else {
        ss.players.retain(|p| p.local.is_some());
        ss.deliver(DPID_SYSMSG, DPID_ALLPLAYERS, || Body::SessionLost);
    }
}

/// The host's threads: TCP connections coming in, UDP questions answered.
fn start_host(serial: u32, listener: TcpListener, udp: Option<UdpSocket>, stop: Arc<AtomicBool>) {
    let s = stop.clone();
    std::thread::spawn(move || {
        let _ = listener.set_nonblocking(true);
        let mut next = 1;
        while !s.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((c, addr)) => {
                    net_log!("connection {next} from {addr}");
                    let _ = c.set_nonblocking(false);
                    let _ = c.set_nodelay(true);
                    let peer = next;
                    next += 1;
                    std::thread::spawn(move || reader(serial, peer, c));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => {
                    net_log!("accept: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });
    let Some(udp) = udp else { return };
    std::thread::spawn(move || {
        let _ = udp.set_read_timeout(Some(Duration::from_millis(100)));
        let mut buf = [0; 64];
        while !stop.load(Ordering::Relaxed) {
            let Ok((n, from)) = udp.recv_from(&mut buf) else { continue };
            if n < 24 || &buf[..8] != ASK {
                continue;
            }
            let answer = {
                let g = net();
                let Some(ss) = g.as_ref().and_then(|n| n.session.as_ref()).filter(|ss| ss.serial == serial) else { break };
                if buf[8..24] != ss.app {
                    continue;
                }
                let mut a = ANSWER.to_vec();
                a.extend_from_slice(&ss.instance);
                a.extend_from_slice(&ss.app);
                a.extend_from_slice(&(ss.players.len() as u32).to_le_bytes());
                a.extend_from_slice(&ss.max.to_le_bytes());
                a.extend_from_slice(&ss.flags.to_le_bytes());
                a.extend(ss.name.chars().take(64).map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }));
                a
            };
            net_log!("asked by {from}");
            let _ = udp.send_to(&answer, from);
        }
    });
}

/// The UDP socket that asks for sessions, and its thread that collects the answers.
fn finder(n: &mut Net) -> Option<&UdpSocket> {
    if n.finder.is_none() {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
        let _ = socket.set_broadcast(true);
        let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
        let stop = Arc::new(AtomicBool::new(false));
        let (r, s) = (socket.try_clone().ok()?, stop.clone());
        std::thread::spawn(move || {
            let mut buf = [0; 200];
            while !s.load(Ordering::Relaxed) {
                let Ok((len, from)) = r.recv_from(&mut buf) else { continue };
                let b = &buf[..len];
                if len < 56 || &b[..8] != ANSWER {
                    continue;
                }
                let f = Found {
                    instance: b[8..24].try_into().unwrap(),
                    app: b[24..40].try_into().unwrap(),
                    current: u32_at(b, 40),
                    max: u32_at(b, 44),
                    flags: u32_at(b, 48),
                    name: text_of(&b[52..]),
                    addr: from,
                    seen: Instant::now(),
                };
                let mut g = net();
                let Some(n) = g.as_mut() else { break };
                net_log!("session {:?} at {from}", f.name);
                n.found.retain(|o| o.instance != f.instance);
                n.found.push(f);
            }
        });
        n.finder = Some(Finder { socket, stop });
    }
    n.finder.as_ref().map(|f| &f.socket)
}

/// Where to ask for sessions: `HOGS_NET_JOIN` (an address), else the local network and this
/// computer.
fn targets() -> Vec<SocketAddr> {
    let typed = std::env::var("HOGS_NET_JOIN").unwrap_or_default();
    let typed = typed.trim();
    if typed.is_empty() {
        let p = port();
        return vec![SocketAddr::from((Ipv4Addr::BROADCAST, p)), SocketAddr::from((Ipv4Addr::LOCALHOST, p))];
    }
    let with_port = if typed.contains(':') { typed.to_string() } else { format!("{typed}:{}", port()) };
    match with_port.to_socket_addrs() {
        Ok(a) => a.filter(|a| a.is_ipv4()).take(1).collect(),
        Err(e) => {
            net_log!("address {typed:?}: {e}");
            Vec::new()
        }
    }
}

// ---- IDirectPlayLobby3A ----------------------------------------------------------------------------

pub const CLSID_DIRECTPLAYLOBBY: [u8; 16] = guid(0x2fe8_f810, 0xb2a5, 0x11d0, [0xa7, 0x87, 0x00, 0x00, 0xf8, 0x03, 0xab, 0xfc]);

/// CoCreateInstance for CLSID_DirectPlayLobby.
pub fn create_lobby(out: u32) -> u32 {
    mem().wr32(out, super::com_new(LOBBY, 0));
    DP_OK
}

fn lobby_ret(_: &mut Cpu) -> u32 {
    DP_OK
}
fn lobby_not_lobbied(_: &mut Cpu) -> u32 {
    0x8877_0410 // DPERR_NOTLOBBIED
}
fn lobby_unsupported(_: &mut Cpu) -> u32 {
    E_NOTIMPL
}

static LOBBY: &[Import] = &[
    ("IDirectPlayLobby3A::QueryInterface", 3, lobby_unsupported),
    ("IDirectPlayLobby3A::AddRef", 1, lobby_ret),
    ("IDirectPlayLobby3A::Release", 1, lobby_ret),
    ("IDirectPlayLobby3A::Connect", 4, lobby_not_lobbied),
    ("IDirectPlayLobby3A::CreateAddress", 7, lobby_unsupported),
    ("IDirectPlayLobby3A::EnumAddress", 5, lobby_unsupported),
    ("IDirectPlayLobby3A::EnumAddressTypes", 5, lobby_unsupported),
    ("IDirectPlayLobby3A::EnumLocalApplications", 4, lobby_ret),
    ("IDirectPlayLobby3A::GetConnectionSettings", 4, lobby_not_lobbied),
    ("IDirectPlayLobby3A::ReceiveLobbyMessage", 6, lobby_unsupported),
    ("IDirectPlayLobby3A::RunApplication", 5, lobby_unsupported),
    ("IDirectPlayLobby3A::SendLobbyMessage", 5, lobby_unsupported),
    ("IDirectPlayLobby3A::SetConnectionSettings", 4, lobby_unsupported),
    ("IDirectPlayLobby3A::SetLobbyMessageEvent", 4, lobby_ret),
    ("IDirectPlayLobby3A::CreateCompoundAddress", 5, lobby_unsupported),
    ("IDirectPlayLobby3A::ConnectEx", 5, lobby_not_lobbied),
    ("IDirectPlayLobby3A::RegisterApplication", 3, lobby_ret),
    ("IDirectPlayLobby3A::UnregisterApplication", 3, lobby_ret),
    ("IDirectPlayLobby3A::WaitForConnectionSettings", 2, lobby_ret),
];

// ---- creation and IUnknown ----------------------------------------------------------------------

/// CoCreateInstance for CLSID_DirectPlay (from `misc::co_create_instance`).
pub fn create(iid: u32, out: u32) -> u32 {
    let mut g = net();
    if let Some(old) = g.as_mut() {
        close(old);
        if let Some(f) = old.finder.take() {
            f.stop.store(true, Ordering::Relaxed);
        }
        free(old.object);
    }
    let object = super::com_new(DIRECT_PLAY, 0);
    *g = Some(Net { object, ..Net::default() });
    drop(g);
    query(iid, out)
}

fn query(iid: u32, out: u32) -> u32 {
    let id: [u8; 16] = mem().slice(iid, 16).try_into().unwrap();
    let mut g = net();
    let Some(n) = g.as_mut() else { return E_NOINTERFACE };
    if id != IID_IUNKNOWN && id != IID_IDIRECTPLAY4A {
        log!("warn: DirectPlay: interface {id:02x?} not supported");
        mem().wr32(out, 0);
        return E_NOINTERFACE;
    }
    n.refs += 1;
    mem().wr32(out, n.object);
    DP_OK
}

fn query_interface(c: &mut Cpu) -> u32 {
    query(arg(c, 1), arg(c, 2))
}

fn add_ref(_: &mut Cpu) -> u32 {
    net().as_mut().map_or(0, |n| {
        n.refs += 1;
        n.refs
    })
}

fn release(_: &mut Cpu) -> u32 {
    let mut g = net();
    let Some(n) = g.as_mut() else { return 0 };
    n.refs = n.refs.saturating_sub(1);
    let refs = n.refs;
    if refs == 0 {
        close(n);
        if let Some(f) = n.finder.take() {
            f.stop.store(true, Ordering::Relaxed);
        }
        n.initialized = false;
    }
    refs
}

fn not_impl(_: &mut Cpu) -> u32 {
    E_NOTIMPL
}

// ---- connections --------------------------------------------------------------------------------

/// Our connection: a DirectPlay address with its total size and the TCP/IP service provider.
fn connection() -> Vec<u8> {
    let mut a = DPAID_TOTALSIZE.to_vec();
    a.extend_from_slice(&4u32.to_le_bytes());
    a.extend_from_slice(&(2 * 20 + 4 + 16u32).to_le_bytes());
    a.extend_from_slice(&DPAID_SERVICEPROVIDER);
    a.extend_from_slice(&16u32.to_le_bytes());
    a.extend_from_slice(&DPSPGUID_TCPIP);
    a
}

/// EnumConnections(this, application, callback, context, flags): one, TCP/IP.
fn enum_connections(c: &mut Cpu) -> u32 {
    let (cb, ctx) = (arg(c, 2), arg(c, 3));
    let conn = connection();
    let name = CONNECTION_NAME.as_bytes();
    let block = alloc(16 + conn.len() as u32 + 16 + name.len() as u32 + 1);
    let m = mem();
    let (sp, address, dpname, text) = (block, block + 16, block + 16 + conn.len() as u32, block + 32 + conn.len() as u32);
    m.slice(sp, 16).copy_from_slice(&DPSPGUID_TCPIP);
    m.slice(address, conn.len()).copy_from_slice(&conn);
    m.wr32(dpname, 16);
    m.wr32(dpname + 8, text);
    m.slice(text, name.len()).copy_from_slice(name);
    guest_call(c, cb, &[sp, address, conn.len() as u32, dpname, 0, ctx]);
    free(block);
    DP_OK
}

/// InitializeConnection(this, connection, flags).
fn initialize_connection(c: &mut Cpu) -> u32 {
    let a = arg(c, 1);
    let conn = connection();
    let ours = a != 0 && mem().slice(a, conn.len()) == &conn[..];
    let mut g = net();
    let Some(n) = g.as_mut() else { return DPERR_UNINITIALIZED };
    if !ours {
        return DPERR_INVALIDPARAMS;
    }
    if n.initialized {
        return DPERR_ALREADYINITIALIZED;
    }
    n.initialized = true;
    DP_OK
}

// ---- sessions -----------------------------------------------------------------------------------

/// EnumSessions(this, desc, timeout, callback, context, flags): sessions of the desc's
/// application. Asynchronous (as the game asks): the ones found so far, while the question
/// goes out again; the first time it waits a moment for answers.
fn enum_sessions(c: &mut Cpu) -> u32 {
    let (desc, timeout, cb, ctx, flags) = (arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5));
    let app: [u8; 16] = mem().slice(desc + 0x18, 16).try_into().unwrap();
    let wait = {
        let mut g = net();
        let Some(n) = g.as_mut() else { return DPERR_UNINITIALIZED };
        if !n.initialized {
            return DPERR_UNINITIALIZED;
        }
        let mut ask = ASK.to_vec();
        ask.extend_from_slice(&app);
        let to = targets();
        if let Some(s) = finder(n) {
            for t in &to {
                if let Err(e) = s.send_to(&ask, t) {
                    net_log!("ask {t}: {e}");
                }
            }
        }
        let first = !n.asked;
        n.asked = true;
        if flags & DPENUMSESSIONS_ASYNC == 0 {
            Duration::from_millis(if timeout == 0 { 1000 } else { timeout as u64 })
        } else if first {
            Duration::from_millis(300)
        } else {
            Duration::ZERO
        }
    };
    std::thread::sleep(wait);
    let sessions: Vec<([u8; 16], u32, u32, u32, String)> = {
        let mut g = net();
        let Some(n) = g.as_mut() else { return DPERR_UNINITIALIZED };
        n.found.retain(|f| f.seen.elapsed() < FOUND_FOR);
        n.found
            .iter()
            .filter(|f| f.app == app && (flags & DPENUMSESSIONS_AVAILABLE == 0 || f.current < f.max))
            .map(|f| (f.instance, f.current, f.max, f.flags, f.name.clone()))
            .collect()
    };
    let m = mem();
    let block = alloc(0x50 + 4 + 68);
    let (sd, time, text) = (block, block + 0x50, block + 0x54);
    for (instance, current, max, sflags, name) in sessions {
        m.slice(sd, 0x50).fill(0);
        m.wr32(sd, 0x50);
        m.wr32(sd + 4, sflags);
        m.slice(sd + 8, 16).copy_from_slice(&instance);
        m.slice(sd + 0x18, 16).copy_from_slice(&app);
        m.wr32(sd + 0x28, max);
        m.wr32(sd + 0x2c, current);
        super::put_cstr(text, &name, 65);
        m.wr32(sd + 0x30, text);
        m.wr32(time, timeout);
        if guest_call(c, cb, &[sd, time, 0, ctx]) == 0 {
            free(block);
            return DP_OK;
        }
    }
    m.wr32(time, timeout);
    guest_call(c, cb, &[0, time, DPESC_TIMEDOUT, ctx]);
    free(block);
    DP_OK
}

/// Open(this, desc, flags): create a session (DPOPEN_CREATE) or join the one with the desc's
/// instance GUID that EnumSessions found.
fn open(c: &mut Cpu) -> u32 {
    let (desc, flags) = (arg(c, 1), arg(c, 2));
    let m = mem();
    let mut g = net();
    let Some(n) = g.as_mut() else { return DPERR_UNINITIALIZED };
    if !n.initialized {
        return DPERR_UNINITIALIZED;
    }
    close(n);
    n.serial += 1;
    let serial = n.serial;
    let stop = Arc::new(AtomicBool::new(false));
    if flags & DPOPEN_CREATE != 0 {
        let bind: IpAddr = std::env::var("HOGS_NET_BIND").ok().and_then(|a| a.parse().ok()).unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let listener = match TcpListener::bind((bind, port())) {
            Ok(l) => l,
            Err(e) => {
                log!("warn: DirectPlay: cannot listen on port {}: {e}", port());
                return DPERR_CANTCREATESESSION;
            }
        };
        let udp = UdpSocket::bind((bind, port())).map_err(|e| log!("warn: DirectPlay: no session search on port {}: {e}", port())).ok();
        let r = random();
        let mut instance = [0; 16];
        instance[..8].copy_from_slice(&r.to_le_bytes());
        instance[8..].copy_from_slice(&random().rotate_left(17).to_le_bytes());
        let name = cstr(m.rd32(desc + 0x30));
        net_log!("hosting {name:?} on port {}", port());
        n.session = Some(Session {
            serial,
            host: true,
            instance,
            app: m.slice(desc + 0x18, 16).try_into().unwrap(),
            max: m.rd32(desc + 0x28),
            flags: m.rd32(desc + 4),
            name,
            players: Vec::new(),
            peers: Vec::new(),
            queue: VecDeque::new(),
            stop: stop.clone(),
        });
        start_host(serial, listener, udp, stop);
        return DP_OK;
    }
    if flags & DPOPEN_JOIN == 0 {
        return DPERR_INVALIDPARAMS;
    }
    let instance: [u8; 16] = m.slice(desc + 8, 16).try_into().unwrap();
    let Some((addr, app, max, sflags, name)) = n.found.iter().find(|f| f.instance == instance).map(|f| (f.addr, f.app, f.max, f.flags, f.name.clone())) else {
        return DPERR_NOSESSIONS;
    };
    drop(g);
    // the host's TCP port is the one it answered from
    net_log!("joining {name:?} at {addr}");
    let mut s = match TcpStream::connect_timeout(&addr, Duration::from_secs(4)) {
        Ok(s) => s,
        Err(e) => {
            log!("warn: DirectPlay: cannot connect to {addr}: {e}");
            return DPERR_NOCONNECTION;
        }
    };
    let _ = s.set_nodelay(true);
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    if s.write_all(&frame(HELLO, &instance)).is_err() {
        return DPERR_NOCONNECTION;
    }
    match read_frame(&mut s) {
        Ok((WELCOME, _)) => {}
        Ok((REFUSED, _)) => return DPERR_NONEWPLAYERS,
        Ok(_) => return DPERR_NOCONNECTION,
        Err(_) => return DPERR_TIMEOUT,
    }
    let _ = s.set_read_timeout(None);
    let Ok(out) = s.try_clone() else { return DPERR_NOCONNECTION };
    let mut g = net();
    let Some(n) = g.as_mut().filter(|n| n.serial == serial) else { return DPERR_NOCONNECTION };
    n.session = Some(Session {
        serial,
        host: false,
        instance,
        app,
        max,
        flags: sflags,
        name,
        players: Vec::new(),
        peers: vec![Peer { id: TO_HOST, out: Arc::new(Mutex::new(out)) }],
        queue: VecDeque::new(),
        stop,
    });
    std::thread::spawn(move || reader(serial, TO_HOST, s));
    DP_OK
}

/// Leave the session: our players go, connections close.
fn close(n: &mut Net) {
    if let Some(mut ss) = n.session.take() {
        let outs = ss.routes(0, DPID_ALLPLAYERS);
        for p in ss.players.iter().filter(|p| p.local.is_some()) {
            send_to(&outs, &frame(PLAYER_DEL, &p.id.to_le_bytes()));
        }
        ss.close();
        net_log!("session closed");
    }
}

fn close_session(_: &mut Cpu) -> u32 {
    if let Some(n) = net().as_mut() {
        close(n);
    }
    DP_OK
}

// ---- players and messages -----------------------------------------------------------------------

/// CreatePlayer(this, &id, name (DPNAME), event, data, size, flags).
fn create_player(c: &mut Cpu) -> u32 {
    let (out, dpname, event) = (arg(c, 1), arg(c, 2), arg(c, 3));
    let m = mem();
    let name = if dpname != 0 && m.rd32(dpname + 8) != 0 { cstr(m.rd32(dpname + 8)) } else { String::new() };
    let mut g = net();
    let Some(ss) = g.as_mut().and_then(|n| n.session.as_mut()) else { return DPERR_INVALIDPARAMS };
    let id = loop {
        let id = random() as u32;
        if id != 0 && id != u32::MAX && !ss.players.iter().any(|p| p.id == id) {
            break id;
        }
    };
    net_log!("local player {id:#x} {name:?}");
    ss.players.push(Player { id, name: name.clone(), local: Some(event), peer: 0 });
    let outs = ss.routes(0, DPID_ALLPLAYERS);
    send_to(&outs, &player_add(id, false, &name));
    m.wr32(out, id);
    DP_OK
}

/// DestroyPlayer(this, id): one of ours.
fn destroy_player(c: &mut Cpu) -> u32 {
    let id = arg(c, 1);
    let mut g = net();
    let Some(ss) = g.as_mut().and_then(|n| n.session.as_mut()) else { return DPERR_INVALIDPLAYER };
    let Some(k) = ss.players.iter().position(|p| p.id == id && p.local.is_some()) else { return DPERR_INVALIDPLAYER };
    ss.players.remove(k);
    ss.queue.retain(|q| q.to != id);
    let outs = ss.routes(0, DPID_ALLPLAYERS);
    send_to(&outs, &frame(PLAYER_DEL, &id.to_le_bytes()));
    DP_OK
}

/// Send(this, from, to, flags, data, size): everything arrives, in order (TCP).
fn send(c: &mut Cpu) -> u32 {
    let (from, to, data, size) = (arg(c, 1), arg(c, 2), arg(c, 4), arg(c, 5));
    let bytes = mem().slice(data, size as usize).to_vec();
    let mut g = net();
    let Some(ss) = g.as_mut().and_then(|n| n.session.as_mut()) else { return DPERR_NOCONNECTION };
    if !ss.players.iter().any(|p| p.id == from && p.local.is_some()) {
        return DPERR_INVALIDPLAYER;
    }
    ss.deliver(from, to, || Body::Data(bytes.clone()));
    let outs = ss.routes(0, to);
    let mut body = from.to_le_bytes().to_vec();
    body.extend_from_slice(&to.to_le_bytes());
    body.extend_from_slice(&bytes);
    send_to(&outs, &frame(DATA, &body));
    DP_OK
}

/// A message as the game gets it, at guest address `at` (system messages hold pointers to
/// their names, which follow them).
fn bytes_of(m: &Message, at: u32) -> Vec<u8> {
    let name_at = |b: &mut Vec<u8>, slot: usize, name: &str| {
        let p = at + b.len() as u32;
        b[slot..slot + 4].copy_from_slice(&p.to_le_bytes());
        b.extend(name.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }));
        b.push(0);
    };
    let w = |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    match &m.body {
        Body::Data(d) => d.clone(),
        Body::Created { id, name, players } => {
            // DPMSG_CREATEPLAYERORGROUP: type, player type, id, players, data, size, DPNAME
            // (size, flags, short name, long name), parent, flags
            let mut b = vec![0; 0x30];
            w(&mut b, 0, DPSYS_CREATEPLAYERORGROUP);
            w(&mut b, 4, 1);
            w(&mut b, 8, *id);
            w(&mut b, 0xc, *players);
            w(&mut b, 0x18, 16);
            name_at(&mut b, 0x20, name);
            b
        }
        Body::Destroyed { id, name } => {
            // DPMSG_DESTROYPLAYERORGROUP: type, player type, id, local data and size, remote
            // data and size, DPNAME, parent, flags
            let mut b = vec![0; 0x34];
            w(&mut b, 0, DPSYS_DESTROYPLAYERORGROUP);
            w(&mut b, 4, 1);
            w(&mut b, 8, *id);
            w(&mut b, 0x1c, 16);
            name_at(&mut b, 0x24, name);
            b
        }
        Body::SessionLost => DPSYS_SESSIONLOST.to_le_bytes().to_vec(),
    }
}

/// Receive(this, &from, &to, flags, data, &size).
fn receive(c: &mut Cpu) -> u32 {
    let (pfrom, pto, flags, data, psize) = (arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5));
    let m = mem();
    let (want_from, want_to) = (m.rd32(pfrom), m.rd32(pto));
    let mut g = net();
    let Some(ss) = g.as_mut().and_then(|n| n.session.as_mut()) else { return DPERR_NOMESSAGES };
    let Some(k) = ss
        .queue
        .iter()
        .position(|q| (flags & DPRECEIVE_FROMPLAYER == 0 || q.from == want_from) && (flags & DPRECEIVE_TOPLAYER == 0 || q.to == want_to))
    else {
        return DPERR_NOMESSAGES;
    };
    let bytes = bytes_of(&ss.queue[k], data);
    let room = m.rd32(psize);
    m.wr32(psize, bytes.len() as u32);
    if data == 0 || room < bytes.len() as u32 {
        return DPERR_BUFFERTOOSMALL;
    }
    m.slice(data, bytes.len()).copy_from_slice(&bytes);
    m.wr32(pfrom, ss.queue[k].from);
    m.wr32(pto, ss.queue[k].to);
    if flags & DPRECEIVE_PEEK == 0 {
        ss.queue.remove(k);
    }
    DP_OK
}

/// GetMessageCount(this, player, &count).
fn get_message_count(c: &mut Cpu) -> u32 {
    let (id, out) = (arg(c, 1), arg(c, 2));
    let n = net().as_ref().and_then(|n| n.session.as_ref()).map_or(0, |ss| ss.queue.iter().filter(|q| q.to == id).count());
    mem().wr32(out, n as u32);
    DP_OK
}

static DIRECT_PLAY: &[Import] = &[
    ("IDirectPlay4A::QueryInterface", 3, query_interface),
    ("IDirectPlay4A::AddRef", 1, add_ref),
    ("IDirectPlay4A::Release", 1, release),
    ("IDirectPlay4A::AddPlayerToGroup", 3, not_impl),
    ("IDirectPlay4A::Close", 1, close_session),
    ("IDirectPlay4A::CreateGroup", 6, not_impl),
    ("IDirectPlay4A::CreatePlayer", 7, create_player),
    ("IDirectPlay4A::DeletePlayerFromGroup", 3, not_impl),
    ("IDirectPlay4A::DestroyGroup", 2, not_impl),
    ("IDirectPlay4A::DestroyPlayer", 2, destroy_player),
    ("IDirectPlay4A::EnumGroupPlayers", 6, not_impl),
    ("IDirectPlay4A::EnumGroups", 5, not_impl),
    ("IDirectPlay4A::EnumPlayers", 5, not_impl),
    ("IDirectPlay4A::EnumSessions", 6, enum_sessions),
    ("IDirectPlay4A::GetCaps", 3, not_impl),
    ("IDirectPlay4A::GetGroupData", 5, not_impl),
    ("IDirectPlay4A::GetGroupName", 4, not_impl),
    ("IDirectPlay4A::GetMessageCount", 3, get_message_count),
    ("IDirectPlay4A::GetPlayerAddress", 4, not_impl),
    ("IDirectPlay4A::GetPlayerCaps", 4, not_impl),
    ("IDirectPlay4A::GetPlayerData", 5, not_impl),
    ("IDirectPlay4A::GetPlayerName", 4, not_impl),
    ("IDirectPlay4A::GetSessionDesc", 3, not_impl),
    ("IDirectPlay4A::Initialize", 2, not_impl),
    ("IDirectPlay4A::Open", 3, open),
    ("IDirectPlay4A::Receive", 6, receive),
    ("IDirectPlay4A::Send", 6, send),
    ("IDirectPlay4A::SetGroupData", 5, not_impl),
    ("IDirectPlay4A::SetGroupName", 4, not_impl),
    ("IDirectPlay4A::SetPlayerData", 5, not_impl),
    ("IDirectPlay4A::SetPlayerName", 4, not_impl),
    ("IDirectPlay4A::SetSessionDesc", 3, not_impl),
    ("IDirectPlay4A::AddGroupToGroup", 3, not_impl),
    ("IDirectPlay4A::CreateGroupInGroup", 7, not_impl),
    ("IDirectPlay4A::DeleteGroupFromGroup", 3, not_impl),
    ("IDirectPlay4A::EnumConnections", 5, enum_connections),
    ("IDirectPlay4A::EnumGroupsInGroup", 6, not_impl),
    ("IDirectPlay4A::GetGroupConnectionSettings", 5, not_impl),
    ("IDirectPlay4A::InitializeConnection", 3, initialize_connection),
    ("IDirectPlay4A::SecureOpen", 5, not_impl),
    ("IDirectPlay4A::SendChatMessage", 5, not_impl),
    ("IDirectPlay4A::SetGroupConnectionSettings", 4, not_impl),
    ("IDirectPlay4A::StartSession", 3, not_impl),
    ("IDirectPlay4A::GetGroupFlags", 3, not_impl),
    ("IDirectPlay4A::GetGroupParent", 3, not_impl),
    ("IDirectPlay4A::GetPlayerAccount", 5, not_impl),
    ("IDirectPlay4A::GetPlayerFlags", 3, not_impl),
    ("IDirectPlay4A::GetGroupOwner", 3, not_impl),
    ("IDirectPlay4A::SetGroupOwner", 3, not_impl),
    ("IDirectPlay4A::SendEx", 10, not_impl),
    ("IDirectPlay4A::GetMessageQueue", 6, not_impl),
    ("IDirectPlay4A::CancelMessage", 3, not_impl),
    ("IDirectPlay4A::CancelPriority", 4, not_impl),
];
