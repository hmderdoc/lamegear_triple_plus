//! Door-side multiplayer client, ported from lameboy: one persistent,
//! non-blocking, length-framed TCP connection to the link server, held for the
//! whole session so a challenge can reach the caller even mid-game.
//!
//! Framing and the control plane (HELLO/roster/challenge/link lifecycle) are
//! identical to lameboy's — the same link-server binary relays for both doors.
//! What differs is the session payload: there is no Game Boy serial byte
//! bridge and no host-rendered video stream. LameGear+ sessions are
//! input-mirroring lockstep (spec §4): each peer simulates the whole session
//! and renders locally; only `{frame, slot, buttons}` messages plus periodic
//! `{frame, crc32}` state checks cross the network.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const FRAME_CONTROL: u8 = 1;
/// Opaque lockstep-session traffic, relayed verbatim to the session peer by
/// the server. Sub-typed by the first payload byte (SESSION_* below).
const FRAME_SESSION: u8 = 3;
const MAX_FRAME: usize = 8192;

// Lockstep session payload kinds (payload[0]). Deliberately disjoint from
// lameboy's SETUP/INPUT/VIDEO numbering: a lameboy client that somehow ends up
// linked to a lamegear door must not half-parse our traffic.
const SESSION_HELLO: u8 = 0x10; // handshake: rom sha + timing + proposed delay
const SESSION_ACCEPT: u8 = 0x11; // handshake ack (rom sha echoed)
const SESSION_INPUT: u8 = 0x12; // {frame: u32, slot: u8, buttons: u16}
const SESSION_CRC: u8 = 0x13; // {frame: u32, crc: u32}
const SESSION_END: u8 = 0x14; // {reason: utf8}
const SESSION_PING: u8 = 0x15; // {token: u32} — RTT probe, echo back as PONG
const SESSION_PONG: u8 = 0x16; // {token: u32}
const SESSION_CHAT: u8 = 0x17; // {text: utf8} — console-scoped chat line

/// How a linked session wires two players' inputs (spec §4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionShape {
    /// One console, two pads: slot 0 = P1, slot 1 = P2.
    SharedConsole,
    /// Two Game Gears joined by an in-process Gear-to-Gear cable; each slot
    /// drives its own machine and each peer renders its own.
    GearToGear,
}

impl SessionShape {
    pub fn to_byte(self) -> u8 {
        match self {
            SessionShape::SharedConsole => 0,
            SessionShape::GearToGear => 1,
        }
    }
    pub fn from_byte(b: u8) -> SessionShape {
        if b == 1 { SessionShape::GearToGear } else { SessionShape::SharedConsole }
    }
}

/// A decoded lockstep-session message from the peer (drained via
/// `take_session`).
#[derive(Clone, Debug, PartialEq)]
pub enum SessionMsg {
    /// Initiator -> acceptor: session parameters. `delay` is the negotiated
    /// input delay D in frames; `timing` 0=NTSC 1=PAL; `shape` the session
    /// wiring; `rom_sha` the SHA-256 of the initiator's ROM (contract: must
    /// match ours exactly).
    Hello { rom_sha: [u8; 32], timing: u8, delay: u8, shape: SessionShape },
    /// Acceptor -> initiator: agreed; echoes its own ROM SHA-256.
    Accept { rom_sha: [u8; 32] },
    /// One frame's held-button mask for one player slot.
    Input { frame: u32, slot: u8, buttons: u16 },
    /// Periodic full-state CRC for the desync check.
    Crc { frame: u32, crc: u32 },
    /// Session over.
    End { reason: String },
    /// RTT probe / echo.
    Ping { token: u32 },
    Pong { token: u32 },
    /// Console-scoped chat from the linked peer (the "cable" carries talk
    /// too; never mixed with global chat).
    Chat { text: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct RosterEntry {
    pub id: String, // unique display name (handle, or handle#2)
    pub status: String, // menu | game | linking
    pub game: String,
    /// System slug (gg/sms/md/...) while in-game; empty from older doors.
    pub system: String,
    /// "open" = their 2P controller port accepts JOINs without a challenge.
    pub port: String,
    /// Linked-session peer id and this entry's pad slot ("0" = P1), when the
    /// server has them in an open session; empty otherwise.
    pub peer: String,
    pub slot: String,
}

impl RosterEntry {
    pub fn port_open(&self) -> bool {
        self.port == "open"
    }
    pub fn in_session(&self) -> bool {
        !self.peer.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Incoming {
    pub from: String, // challenger's display id
    pub game: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LinkState {
    Idle,
    /// Accepted; both sides counting down before the session goes live.
    Starting { peer: String, game: String },
    /// Relay open — session traffic flows.
    Open { peer: String, game: String },
}

/// Things the UI reacts to (drained via `take_event`).
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    ChallengeReceived { from: String, game: String },
    ChallengeCanceled,
    ChallengeRejected { by: String },
    LinkStarting { peer: String, game: String },
    LinkOpen { peer: String, game: String },
    LinkEnded { reason: String },
    /// Live global chat line (history replays don't fire this).
    Chat { from: String, text: String },
    /// Server refused something (JOIN a console that just closed, ...).
    Error { msg: String },
}

/// One global-chat line, kept in the client-side ring (live + history).
#[derive(Clone, Debug, PartialEq)]
pub struct ChatMsg {
    pub from: String,
    pub text: String,
}

/// Client-side chat ring size (matches the server's replay depth).
const CHAT_KEEP: usize = 50;

pub struct Multiplayer {
    sock: TcpStream,
    inbuf: Vec<u8>,
    outbuf: VecDeque<u8>,
    alive: bool,
    my_id: String,
    roster: Vec<RosterEntry>,
    incoming: Vec<Incoming>,
    outgoing: Option<String>, // id I have an outstanding challenge to
    link: LinkState,
    link_initiator: bool, // this side's challenge started the current link
    session_in: VecDeque<SessionMsg>,
    events: VecDeque<Event>,
    /// Global chat, oldest first (server history replay + live lines). Kept
    /// here — not in the UI — so lines arriving mid-game aren't lost.
    chat: VecDeque<ChatMsg>,
}

impl Multiplayer {
    /// Connect and announce presence (`HELLO <handle>`). Blocks briefly on the
    /// TCP connect (fails fast if the server is down), then goes non-blocking.
    pub fn connect(addr: &str, handle: &str) -> io::Result<Multiplayer> {
        let sock = {
            use std::net::ToSocketAddrs;
            match addr.to_socket_addrs().ok().and_then(|mut it| it.next()) {
                Some(sa) => TcpStream::connect_timeout(&sa, Duration::from_secs(3))?,
                None => TcpStream::connect(addr)?,
            }
        };
        sock.set_nonblocking(true)?;
        sock.set_nodelay(true)?;
        let handle = sanitize(handle);
        let mut mp = Multiplayer {
            sock,
            inbuf: Vec::new(),
            outbuf: VecDeque::new(),
            alive: true,
            my_id: handle.clone(), // provisional until WELCOME confirms
            roster: Vec::new(),
            incoming: Vec::new(),
            outgoing: None,
            link: LinkState::Idle,
            link_initiator: false,
            session_in: VecDeque::new(),
            events: VecDeque::new(),
            chat: VecDeque::new(),
        };
        mp.send_control(&["HELLO", &handle]);
        // Flush the HELLO right away so the server registers us immediately.
        mp.pump();
        Ok(mp)
    }

    // --- accessors for the UI ---
    pub fn is_alive(&self) -> bool {
        self.alive
    }
    pub fn my_id(&self) -> &str {
        &self.my_id
    }
    /// Other callers (everyone but me) — the lobby's challengeable list.
    pub fn others(&self) -> impl Iterator<Item = &RosterEntry> {
        self.roster.iter().filter(move |r| r.id != self.my_id)
    }
    pub fn incoming(&self) -> &[Incoming] {
        &self.incoming
    }
    pub fn outgoing(&self) -> Option<&str> {
        self.outgoing.as_deref()
    }
    pub fn link_state(&self) -> &LinkState {
        &self.link
    }
    pub fn take_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
    /// Whether this side's challenge initiated the current link. The initiator
    /// runs the session handshake and plays slot 0 (player 1).
    pub fn is_link_initiator(&self) -> bool {
        self.link_initiator
    }

    // --- caller actions ---
    /// `system` is the SYSTEMS slug while in-game; `port_open` advertises the
    /// 2P controller port (JOINable without a challenge).
    pub fn set_status(&mut self, state: &str, game: &str, system: &str, port_open: bool) {
        let port = if port_open { "open" } else { "closed" };
        self.send_control(&["STATUS", state, &sanitize(game), &sanitize(system), port]);
    }
    pub fn challenge(&mut self, target_id: &str, game: &str) {
        self.outgoing = Some(target_id.to_string());
        self.send_control(&["CHALLENGE", target_id, &sanitize(game)]);
    }
    /// Sit down at `target_id`'s open console as player 2 (no challenge
    /// round-trip; the server validates the port is still open).
    pub fn join(&mut self, target_id: &str) {
        self.send_control(&["JOIN", target_id]);
    }
    /// Global chat: broadcast to everyone connected. The server echoes it
    /// back, which is when it lands in our own history.
    pub fn send_chat(&mut self, text: &str) {
        let text = sanitize(text);
        let text = text.trim();
        if !text.is_empty() {
            self.send_control(&["CHAT", text]);
        }
    }
    /// Global chat transcript, oldest first.
    pub fn chat_log(&self) -> impl Iterator<Item = &ChatMsg> {
        self.chat.iter()
    }
    pub fn cancel(&mut self) {
        if let Some(t) = self.outgoing.take() {
            self.send_control(&["CANCEL", &t]);
        }
    }
    pub fn accept(&mut self, from_id: &str) {
        self.incoming.retain(|c| c.from != from_id);
        self.send_control(&["ACCEPT", from_id]);
    }
    pub fn reject(&mut self, from_id: &str) {
        self.incoming.retain(|c| c.from != from_id);
        self.send_control(&["REJECT", from_id]);
    }
    pub fn ready(&mut self) {
        self.send_control(&["READY"]);
    }
    pub fn abort(&mut self) {
        self.link = LinkState::Idle;
        // Drop buffered peer messages: they belong to the session being torn
        // down and must not bleed a stale End into the next one.
        self.session_in.clear();
        self.send_control(&["ABORT"]);
    }

    // --- lockstep session traffic (relayed verbatim by the server) ---

    pub fn send_session_hello(
        &mut self,
        rom_sha: &[u8; 32],
        timing: u8,
        delay: u8,
        shape: SessionShape,
    ) {
        let mut p = vec![SESSION_HELLO, 2 /* version */, timing, delay, shape.to_byte()];
        p.extend_from_slice(rom_sha);
        self.queue_frame(FRAME_SESSION, &p);
    }

    pub fn send_session_accept(&mut self, rom_sha: &[u8; 32]) {
        let mut p = vec![SESSION_ACCEPT, 1];
        p.extend_from_slice(rom_sha);
        self.queue_frame(FRAME_SESSION, &p);
    }

    /// One frame's held-button mask for our slot.
    pub fn send_session_input(&mut self, frame: u32, slot: u8, buttons: u16) {
        let f = frame.to_be_bytes();
        let b = buttons.to_be_bytes();
        self.queue_frame(
            FRAME_SESSION,
            &[SESSION_INPUT, f[0], f[1], f[2], f[3], slot, b[0], b[1]],
        );
    }

    pub fn send_session_crc(&mut self, frame: u32, crc: u32) {
        let f = frame.to_be_bytes();
        let c = crc.to_be_bytes();
        self.queue_frame(
            FRAME_SESSION,
            &[SESSION_CRC, f[0], f[1], f[2], f[3], c[0], c[1], c[2], c[3]],
        );
    }

    pub fn send_session_end(&mut self, reason: &str) {
        let mut p = vec![SESSION_END];
        p.extend_from_slice(reason.as_bytes());
        self.queue_frame(FRAME_SESSION, &p);
    }

    pub fn send_session_ping(&mut self, token: u32) {
        let t = token.to_be_bytes();
        self.queue_frame(FRAME_SESSION, &[SESSION_PING, t[0], t[1], t[2], t[3]]);
    }

    pub fn send_session_pong(&mut self, token: u32) {
        let t = token.to_be_bytes();
        self.queue_frame(FRAME_SESSION, &[SESSION_PONG, t[0], t[1], t[2], t[3]]);
    }

    /// Console-scoped chat to the linked peer (relayed verbatim like input).
    pub fn send_session_chat(&mut self, text: &str) {
        let text = sanitize(text);
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let mut p = vec![SESSION_CHAT];
        p.extend_from_slice(&text.as_bytes()[..text.len().min(180)]);
        self.queue_frame(FRAME_SESSION, &p);
    }

    /// Next decoded session message from the peer, if any.
    pub fn take_session(&mut self) -> Option<SessionMsg> {
        self.session_in.pop_front()
    }

    /// Discard any buffered session messages — called at the start of a new
    /// lockstep session so a stale End from a prior session can't leak in.
    pub fn reset_session_buffer(&mut self) {
        self.session_in.clear();
    }

    /// Bytes queued but not yet flushed to the socket — a congestion signal.
    pub fn out_backlog(&self) -> usize {
        self.outbuf.len()
    }

    fn send_control(&mut self, parts: &[&str]) {
        let line = parts.join("\t");
        self.queue_frame(FRAME_CONTROL, line.as_bytes());
    }

    fn queue_frame(&mut self, t: u8, payload: &[u8]) {
        if payload.len() > MAX_FRAME {
            return;
        }
        let len = payload.len() as u16;
        self.outbuf.push_back(t);
        self.outbuf.push_back((len >> 8) as u8);
        self.outbuf.push_back(len as u8);
        self.outbuf.extend(payload.iter().copied());
    }

    /// Flush queued output, read incoming, and process complete frames. Call
    /// once per loop iteration. Marks the client dead on socket close/error.
    pub fn pump(&mut self) {
        self.pump_io();
        // A dead transport can never deliver LINK_END — force the unplug
        // locally so an open link doesn't wait forever on a server that's gone.
        if !self.alive && !matches!(self.link, LinkState::Idle) {
            self.link = LinkState::Idle;
            self.session_in.clear();
            self.events.push_back(Event::LinkEnded { reason: "connection lost".into() });
        }
    }

    fn pump_io(&mut self) {
        while !self.outbuf.is_empty() {
            let chunk: Vec<u8> = self.outbuf.iter().copied().collect();
            match self.sock.write(&chunk) {
                Ok(0) => {
                    self.alive = false;
                    return;
                }
                Ok(n) => {
                    for _ in 0..n {
                        self.outbuf.pop_front();
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.alive = false;
                    return;
                }
            }
        }
        let mut buf = [0u8; 2048];
        loop {
            match self.sock.read(&mut buf) {
                Ok(0) => {
                    self.alive = false;
                    break;
                }
                Ok(n) => self.inbuf.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.alive = false;
                    break;
                }
            }
        }
        while let Some((t, payload)) = take_frame(&mut self.inbuf) {
            match t {
                FRAME_CONTROL => {
                    let line = String::from_utf8_lossy(&payload).into_owned();
                    self.dispatch(&line);
                }
                FRAME_SESSION => {
                    if let Some(msg) = parse_session(&payload) {
                        self.session_in.push_back(msg);
                    }
                }
                _ => {} // FRAME_SERIAL from a lameboy peer: not ours, drop
            }
        }
    }

    fn dispatch(&mut self, line: &str) {
        let t: Vec<&str> = line.split('\t').collect();
        let arg = |i: usize| t.get(i).copied().unwrap_or("").to_string();
        match t.first().copied().unwrap_or("") {
            "WELCOME" => self.my_id = arg(1),
            "ROSTER_CLEAR" => self.roster.clear(),
            "ROSTER_ADD" => self.roster.push(RosterEntry {
                id: arg(1),
                status: arg(2),
                game: arg(3),
                system: arg(4),
                port: arg(5),
                peer: arg(6),
                slot: arg(7),
            }),
            "CHALLENGED" => {
                let from = arg(1);
                self.incoming.retain(|c| c.from != from);
                self.incoming.push(Incoming { from: from.clone(), game: arg(2) });
                self.events.push_back(Event::ChallengeReceived { from, game: arg(2) });
            }
            "CHALLENGE_CANCELED" => {
                let from = arg(1);
                self.incoming.retain(|c| c.from != from);
                self.events.push_back(Event::ChallengeCanceled);
            }
            "REJECTED" => {
                if self.outgoing.as_deref() == Some(arg(1).as_str()) {
                    self.outgoing = None;
                }
                self.events.push_back(Event::ChallengeRejected { by: arg(1) });
            }
            "LINK_START" => {
                // Never yank an OPEN link into a new countdown (see lameboy).
                if !matches!(self.link, LinkState::Open { .. }) {
                    // The server's role token is authoritative (a JOIN has no
                    // outgoing challenge to infer the pad from); fall back to
                    // challenge parity against servers predating it.
                    self.link_initiator = match arg(3).as_str() {
                        "p1" => true,
                        "p2" => false,
                        _ => self.outgoing.as_deref() == Some(arg(1).as_str()),
                    };
                    self.outgoing = None;
                    self.incoming.clear();
                    self.link = LinkState::Starting { peer: arg(1), game: arg(2) };
                    self.events.push_back(Event::LinkStarting { peer: arg(1), game: arg(2) });
                }
            }
            "LINK_OPEN" => {
                let game = match &self.link {
                    LinkState::Starting { game, .. } => game.clone(),
                    _ => String::new(),
                };
                self.link = LinkState::Open { peer: arg(1), game: game.clone() };
                self.events.push_back(Event::LinkOpen { peer: arg(1), game });
            }
            "LINK_END" => {
                self.link = LinkState::Idle;
                self.session_in.clear();
                self.events.push_back(Event::LinkEnded { reason: arg(1) });
            }
            "CHAT_MSG" | "CHAT_HIST" => {
                self.chat.push_back(ChatMsg { from: arg(1), text: arg(2) });
                if self.chat.len() > CHAT_KEEP {
                    self.chat.pop_front();
                }
                // History replay fills the transcript silently; only live
                // lines toast.
                if t[0] == "CHAT_MSG" {
                    self.events.push_back(Event::Chat { from: arg(1), text: arg(2) });
                }
            }
            "ERROR" => self.events.push_back(Event::Error { msg: arg(1) }),
            _ => {} // unknown — ignore
        }
    }
}

/// Decode a lockstep-session payload (see SESSION_* kinds). Malformed frames
/// return None and are dropped.
fn parse_session(p: &[u8]) -> Option<SessionMsg> {
    match *p.first()? {
        SESSION_HELLO => {
            if p.len() < 5 + 32 {
                return None;
            }
            let mut sha = [0u8; 32];
            sha.copy_from_slice(&p[5..37]);
            Some(SessionMsg::Hello {
                rom_sha: sha,
                timing: p[2],
                delay: p[3],
                shape: SessionShape::from_byte(p[4]),
            })
        }
        SESSION_ACCEPT => {
            if p.len() < 2 + 32 {
                return None;
            }
            let mut sha = [0u8; 32];
            sha.copy_from_slice(&p[2..34]);
            Some(SessionMsg::Accept { rom_sha: sha })
        }
        SESSION_INPUT => {
            if p.len() < 8 {
                return None;
            }
            Some(SessionMsg::Input {
                frame: u32::from_be_bytes([p[1], p[2], p[3], p[4]]),
                slot: p[5],
                buttons: u16::from_be_bytes([p[6], p[7]]),
            })
        }
        SESSION_CRC => {
            if p.len() < 9 {
                return None;
            }
            Some(SessionMsg::Crc {
                frame: u32::from_be_bytes([p[1], p[2], p[3], p[4]]),
                crc: u32::from_be_bytes([p[5], p[6], p[7], p[8]]),
            })
        }
        SESSION_END => Some(SessionMsg::End {
            reason: String::from_utf8_lossy(&p[1..]).into_owned(),
        }),
        SESSION_PING => Some(SessionMsg::Ping {
            token: u32::from_be_bytes([*p.get(1)?, *p.get(2)?, *p.get(3)?, *p.get(4)?]),
        }),
        SESSION_PONG => Some(SessionMsg::Pong {
            token: u32::from_be_bytes([*p.get(1)?, *p.get(2)?, *p.get(3)?, *p.get(4)?]),
        }),
        SESSION_CHAT => Some(SessionMsg::Chat {
            text: String::from_utf8_lossy(&p[1..]).into_owned(),
        }),
        _ => None,
    }
}

fn take_frame(buf: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
    if buf.len() < 3 {
        return None;
    }
    let len = ((buf[1] as usize) << 8) | buf[2] as usize;
    if len > MAX_FRAME {
        buf.clear(); // defensive: desynced stream
        return None;
    }
    if buf.len() < 3 + len {
        return None;
    }
    let t = buf[0];
    let payload = buf[3..3 + len].to_vec();
    buf.drain(0..3 + len);
    Some((t, payload))
}

/// Strip TAB/newline from a user field so it can't break the line protocol.
fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_codec_round_trips() {
        let sha = [7u8; 32];
        let mut hello = vec![SESSION_HELLO, 2, 0, 4, 1];
        hello.extend_from_slice(&sha);
        assert_eq!(
            parse_session(&hello),
            Some(SessionMsg::Hello {
                rom_sha: sha,
                timing: 0,
                delay: 4,
                shape: SessionShape::GearToGear,
            })
        );

        let mut acc = vec![SESSION_ACCEPT, 1];
        acc.extend_from_slice(&sha);
        assert_eq!(parse_session(&acc), Some(SessionMsg::Accept { rom_sha: sha }));

        assert_eq!(
            parse_session(&[SESSION_INPUT, 0, 0, 1, 0, 1, 0, 0b101]),
            Some(SessionMsg::Input { frame: 256, slot: 1, buttons: 5 })
        );
        assert_eq!(
            parse_session(&[SESSION_CRC, 0, 0, 0, 60, 0xDE, 0xAD, 0xBE, 0xEF]),
            Some(SessionMsg::Crc { frame: 60, crc: 0xDEADBEEF })
        );
        let mut end = vec![SESSION_END];
        end.extend_from_slice(b"desync at 120");
        assert_eq!(parse_session(&end), Some(SessionMsg::End { reason: "desync at 120".into() }));
        assert_eq!(parse_session(&[SESSION_PING, 0, 0, 0, 9]), Some(SessionMsg::Ping { token: 9 }));
        let mut chat = vec![SESSION_CHAT];
        chat.extend_from_slice(b"gg wp");
        assert_eq!(parse_session(&chat), Some(SessionMsg::Chat { text: "gg wp".into() }));
        // Truncated -> dropped, not a panic.
        assert_eq!(parse_session(&[SESSION_HELLO, 1, 0]), None);
        assert_eq!(parse_session(&[]), None);
    }

    #[test]
    fn take_frame_handles_partial_and_multiple() {
        let mut buf = Vec::new();
        assert!(take_frame(&mut buf).is_none());
        let payload = b"ROSTER_ADD\tDragon\tmenu\t";
        let len = payload.len() as u16;
        let mut f = vec![FRAME_CONTROL, (len >> 8) as u8, len as u8];
        f.extend_from_slice(payload);
        buf.extend_from_slice(&f[..4]);
        assert!(take_frame(&mut buf).is_none()); // incomplete
        buf.extend_from_slice(&f[4..]);
        let (t, p) = take_frame(&mut buf).unwrap();
        assert_eq!(t, FRAME_CONTROL);
        assert!(p.starts_with(b"ROSTER_ADD"));
        assert!(buf.is_empty());
    }

    #[test]
    fn oversized_len_resets_buffer() {
        let mut buf = vec![FRAME_CONTROL, 0xFF, 0xFF, 1, 2, 3];
        assert!(take_frame(&mut buf).is_none());
        assert!(buf.is_empty());
    }
}
