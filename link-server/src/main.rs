//! Game Boy link server: presence + challenges + serial relay for the lameboy
//! BBS door, so callers on any BBS can find each other and link their games.
//!
//! One TCP connection per door, held for the whole session. Callers announce
//! presence (HELLO/STATUS), see a live roster, and send person-to-person
//! challenges that persist until cancelled/rejected/accepted (matchmaking by
//! *person*, not by game — with two callers and a thousand games, queueing by
//! game never pairs anyone). On accept, both sides run a countdown; when both
//! report READY the relay opens and serial bytes cross-forward.
//!
//! Protocol (all control frames are TAB-separated; see proto.rs for framing):
//!   Door -> server:
//!     HELLO   <handle> <bbstag>
//!     STATUS  <menu|game|linking> <game> <system?> <port?>
//!               system: the door's system slug (gg/sms/md/...) so lobbies can
//!               group players by console; port: "open" = anyone may JOIN as
//!               player 2 without a challenge, anything else = closed.
//!     CHALLENGE <target_id> <game?>
//!     JOIN    <target_id>           (sit down at an OPEN console: target must
//!                                    be in-game with port=open; links without
//!                                    a challenge round-trip, target = P1)
//!     CHAT    <text>                (global chat: broadcast to every caller,
//!                                    echoed back to the sender as delivery
//!                                    confirmation; kept in a 50-line history
//!                                    replayed to new connections)
//!     CANCEL  <target_id>
//!     ACCEPT  <from_id>
//!     REJECT  <from_id>
//!     READY                         (countdown finished, ready to link)
//!     ABORT                         (cancel the pending link)
//!     BYE
//!   Server -> door:
//!     WELCOME <your_id>
//!     ROSTER_CLEAR
//!     ROSTER_ADD <id> <status> <game> <system> <port> <peer> <slot>
//!               peer/slot: the session peer's id and this user's player slot
//!               ("0"/"1") when linked, empty otherwise. Old clients that
//!               only read the first tokens keep working.
//!     CHALLENGED <from_id> <from_handle> <from_bbstag> <game>
//!     CHALLENGE_CANCELED <from_id>
//!     REJECTED <by_id>
//!     LINK_START <peer_id> <game> <role?>   role "p1"/"p2": which pad this
//!               side holds (p1 = session initiator / handshake driver).
//!               Clients predating the token fall back to challenge parity.
//!     LINK_OPEN  <peer_id>
//!     LINK_END   <reason>
//!     CHAT_MSG   <from_id> <text>   (live global chat line)
//!     CHAT_HIST  <from_id> <text>   (history replay on connect: same shape,
//!                                    but clients shouldn't toast it)
//!     ERROR      <msg>

mod proto;

use proto::{read_frame, sanitize, tokens, write_frame, Frame};
use std::collections::HashMap;
use std::env;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

type UserId = String; // "handle@bbstag"

struct User {
    status: String, // menu | game | linking
    game: String,
    system: String, // system slug (gg/sms/md/...) while in-game, else ""
    port: String,   // "open" = joinable as P2 without a challenge
    tx: Sender<Frame>,
    session: Option<UserId>, // active serial-linked peer
    slot: Option<u8>,        // pad held in the open session (0 = P1)
}

struct Challenge {
    from: UserId,
    to: UserId,
    game: String,
}

struct PendingLink {
    a: UserId,
    b: UserId,
    a_ready: bool,
    b_ready: bool,
}

/// Global chat history kept for replay to new connections.
const CHAT_HISTORY: usize = 50;
/// Chat lines longer than this are truncated (matches the doors' input cap).
const CHAT_MAX: usize = 180;

#[derive(Default)]
struct Hub {
    users: HashMap<UserId, User>,
    challenges: Vec<Challenge>,
    pending: Vec<PendingLink>,
    chat: Vec<(UserId, String)>, // (from, text), oldest first
}

impl Hub {
    fn send(&self, id: &str, frame: Frame) {
        if let Some(u) = self.users.get(id) {
            let _ = u.tx.send(frame);
        }
    }

    fn broadcast_roster(&self) {
        // Small rosters; re-send the whole list on any change. Each client
        // rebuilds from CLEAR + one ADD per user. The id is the display name.
        for (_, target) in self.users.iter() {
            let _ = target.tx.send(Frame::control(&["ROSTER_CLEAR"]));
            for (id, u) in self.users.iter() {
                let peer = u.session.as_deref().unwrap_or("");
                let slot = u.slot.map(|s| s.to_string()).unwrap_or_default();
                let _ = target.tx.send(Frame::control(&[
                    "ROSTER_ADD",
                    id,
                    &u.status,
                    &u.game,
                    &u.system,
                    &u.port,
                    peer,
                    &slot,
                ]));
            }
        }
    }

    /// A free display id for a handle: the handle itself, or `handle#2`, `#3`…
    /// on the rare collision, so the lobby shows a plain name that's still unique.
    fn free_id(&self, handle: &str) -> UserId {
        if !self.users.contains_key(handle) {
            return handle.to_string();
        }
        (2..).map(|n| format!("{handle}#{n}")).find(|c| !self.users.contains_key(c)).unwrap()
    }

    /// Register a new connection under a unique id derived from its handle;
    /// returns the assigned id for the connection loop to use.
    fn register(&mut self, handle: &str, tx: Sender<Frame>) -> UserId {
        let id = self.free_id(handle);
        self.users.insert(
            id.clone(),
            User {
                status: "menu".into(),
                game: String::new(),
                system: String::new(),
                port: String::new(),
                tx,
                session: None,
                slot: None,
            },
        );
        self.send(&id, Frame::control(&["WELCOME", &id]));
        // Replay the room's recent conversation so a new caller has context.
        for (from, text) in &self.chat {
            self.send(&id, Frame::control(&["CHAT_HIST", from, text]));
        }
        self.broadcast_roster();
        id
    }

    /// Global chat: append to history, broadcast to everyone (sender included
    /// — the echo doubles as delivery confirmation).
    fn chat(&mut self, from: &str, text: &str) {
        let mut text = sanitize(text).trim().to_string();
        if text.is_empty() {
            return;
        }
        text.truncate(CHAT_MAX);
        self.chat.push((from.to_string(), text.clone()));
        if self.chat.len() > CHAT_HISTORY {
            self.chat.remove(0);
        }
        for (_, u) in self.users.iter() {
            let _ = u.tx.send(Frame::control(&["CHAT_MSG", from, &text]));
        }
    }

    fn set_status(&mut self, id: &str, state: &str, game: &str, system: &str, port: &str) {
        if let Some(u) = self.users.get_mut(id) {
            u.status = sanitize(state);
            u.game = sanitize(game);
            u.system = sanitize(system);
            u.port = sanitize(port);
        }
        self.broadcast_roster();
    }

    fn challenge(&mut self, from: &str, to: &str, game: &str) {
        if from == to || !self.users.contains_key(to) {
            self.send(from, Frame::control(&["ERROR", "no such player"]));
            return;
        }
        // Replace any prior challenge from->to, so re-challenging just refreshes.
        self.challenges.retain(|c| !(c.from == from && c.to == to));
        self.challenges.push(Challenge { from: from.into(), to: to.into(), game: game.into() });
        self.send(to, Frame::control(&["CHALLENGED", from, game]));
    }

    fn cancel(&mut self, from: &str, to: &str) {
        let had = self.remove_challenge(from, to);
        if had {
            self.send(to, Frame::control(&["CHALLENGE_CANCELED", from]));
        }
    }

    fn reject(&mut self, me: &str, from: &str) {
        if self.remove_challenge(from, me) {
            self.send(from, Frame::control(&["REJECTED", me]));
        }
    }

    fn accept(&mut self, me: &str, from: &str) {
        let game = match self.challenges.iter().find(|c| c.from == from && c.to == me) {
            Some(c) => c.game.clone(),
            None => {
                self.send(me, Frame::control(&["ERROR", "challenge expired"]));
                return;
            }
        };
        // Neither party may already be in (or setting up) a link — accepting
        // would cross-wire sessions (ready() overwrites session unconditionally,
        // so a third party's open link would start leaking serial bytes here).
        let busy = |id: &str| {
            self.users
                .get(id)
                .is_some_and(|u| u.session.is_some())
                || self.pending.iter().any(|p| p.a == id || p.b == id)
        };
        if busy(me) || busy(from) {
            self.send(me, Frame::control(&["ERROR", "busy"]));
            return;
        }
        // The challenger drove the pairing: they run the handshake as P1.
        self.start_link(from, me, &game);
    }

    /// Sit down at an open console: no challenge round-trip. `target` must be
    /// mid-game with their controller port open; they stay P1, the joiner
    /// takes pad 2, and both doors power-cycle into the linked session.
    fn join(&mut self, joiner: &str, target: &str) {
        if joiner == target || !self.users.contains_key(target) {
            self.send(joiner, Frame::control(&["ERROR", "no such player"]));
            return;
        }
        let busy = |id: &str| {
            self.users.get(id).is_some_and(|u| u.session.is_some())
                || self.pending.iter().any(|p| p.a == id || p.b == id)
        };
        if busy(joiner) || busy(target) {
            self.send(joiner, Frame::control(&["ERROR", "busy"]));
            return;
        }
        let (game, ok) = match self.users.get(target) {
            Some(u) => (u.game.clone(), u.status == "game" && u.port == "open"),
            None => (String::new(), false),
        };
        if !ok || game.is_empty() {
            self.send(joiner, Frame::control(&["ERROR", "console not joinable"]));
            return;
        }
        // The sitting player keeps pad 1 and drives the handshake.
        self.start_link(target, joiner, &game);
    }

    /// Common tail of accept()/join(): clear stray challenges, mark both
    /// linking, and send LINK_START with explicit pad roles (p1 = initiator).
    fn start_link(&mut self, p1: &str, p2: &str, game: &str) {
        // Clear every challenge touching either of the two — they're now busy;
        // tell the sidelined challengers so their pending state clears.
        let dropped: Vec<(UserId, UserId)> = self
            .challenges
            .iter()
            .filter(|c| c.from == p1 || c.to == p1 || c.from == p2 || c.to == p2)
            .map(|c| (c.from.clone(), c.to.clone()))
            .collect();
        self.challenges
            .retain(|c| c.from != p1 && c.to != p1 && c.from != p2 && c.to != p2);
        for (cf, ct) in dropped {
            // Notify the *target* of any dropped challenge that it's gone (the
            // linked pair are handled by LINK_START below).
            if ct != p1 && ct != p2 {
                self.send(&ct, Frame::control(&["CHALLENGE_CANCELED", &cf]));
            }
        }

        for id in [p1, p2] {
            if let Some(u) = self.users.get_mut(id) {
                u.status = "linking".into();
            }
        }
        self.pending.push(PendingLink {
            a: p1.into(),
            b: p2.into(),
            a_ready: false,
            b_ready: false,
        });
        self.send(p1, Frame::control(&["LINK_START", p2, game, "p1"]));
        self.send(p2, Frame::control(&["LINK_START", p1, game, "p2"]));
        self.broadcast_roster();
    }

    fn ready(&mut self, id: &str) {
        let mut open: Option<usize> = None;
        for (i, p) in self.pending.iter_mut().enumerate() {
            if p.a == id {
                p.a_ready = true;
            } else if p.b == id {
                p.b_ready = true;
            } else {
                continue;
            }
            if p.a_ready && p.b_ready {
                open = Some(i);
            }
            break;
        }
        if let Some(i) = open {
            let p = self.pending.remove(i);
            if let Some(u) = self.users.get_mut(&p.a) {
                u.session = Some(p.b.clone());
                u.slot = Some(0);
            }
            if let Some(u) = self.users.get_mut(&p.b) {
                u.session = Some(p.a.clone());
                u.slot = Some(1);
            }
            self.send(&p.a, Frame::control(&["LINK_OPEN", &p.b]));
            self.send(&p.b, Frame::control(&["LINK_OPEN", &p.a]));
            self.broadcast_roster();
        }
    }

    fn abort(&mut self, id: &str, reason: &str) {
        // A pending (accepted-but-not-open) link: cancel it.
        if let Some(i) = self.pending.iter().position(|p| p.a == id || p.b == id) {
            let p = self.pending.remove(i);
            for who in [&p.a, &p.b] {
                if let Some(u) = self.users.get_mut(who) {
                    u.status = "menu".into();
                    u.game.clear();
                }
                self.send(who, Frame::control(&["LINK_END", reason]));
            }
            self.broadcast_roster();
            return;
        }
        // An OPEN session: unplug the cable for both sides. Without this a
        // player quitting a linked game stayed in-session forever (and their
        // door, still seeing the link Open, relaunched the game in a loop).
        let session = self.users.get(id).and_then(|u| u.session.clone());
        if let Some(peer) = session {
            for who in [id, peer.as_str()] {
                if let Some(u) = self.users.get_mut(who) {
                    u.session = None;
                    u.slot = None;
                    u.status = "menu".into();
                    u.game.clear();
                }
                self.send(who, Frame::control(&["LINK_END", reason]));
            }
            self.broadcast_roster();
        }
    }

    /// Lockstep-session traffic (input/video/setup): same forwarding rule as
    /// serial — verbatim to the session peer, dropped when no session exists.
    fn relay_session(&self, from: &str, bytes: Vec<u8>) {
        if let Some(peer) = self.users.get(from).and_then(|u| u.session.clone()) {
            self.send(&peer, Frame::Session(bytes));
        }
    }

    fn relay_serial(&self, from: &str, bytes: Vec<u8>) {
        if let Some(peer) = self.users.get(from).and_then(|u| u.session.clone()) {
            self.send(&peer, Frame::Serial(bytes));
        }
    }

    fn remove_user(&mut self, id: &str, reason: &str) {
        // End an active serial session (clear both ends so the abort() below
        // doesn't see a half-open session and re-send LINK_END).
        let session = self.users.get(id).and_then(|u| u.session.clone());
        if let Some(peer) = session {
            if let Some(u) = self.users.get_mut(id) {
                u.session = None;
                u.slot = None;
            }
            if let Some(u) = self.users.get_mut(&peer) {
                u.session = None;
                u.slot = None;
                u.status = "menu".into();
                u.game.clear();
            }
            self.send(&peer, Frame::control(&["LINK_END", reason]));
        }
        // End any pending (accepted-but-not-open) link.
        self.abort(id, reason);
        // Drop challenges to/from the leaver; notify the still-present side.
        let notify: Vec<(UserId, UserId)> = self
            .challenges
            .iter()
            .filter(|c| c.from == id || c.to == id)
            .map(|c| (c.from.clone(), c.to.clone()))
            .collect();
        self.challenges.retain(|c| c.from != id && c.to != id);
        for (cf, ct) in notify {
            if cf == id {
                // an outgoing challenge from the leaver: tell the target it's gone
                self.send(&ct, Frame::control(&["CHALLENGE_CANCELED", &cf]));
            }
            // incoming challenges to the leaver: the challenger just sees them
            // leave the roster; nothing else to send.
        }
        self.users.remove(id);
        self.broadcast_roster();
    }

    fn remove_challenge(&mut self, from: &str, to: &str) -> bool {
        let before = self.challenges.len();
        self.challenges.retain(|c| !(c.from == from && c.to == to));
        self.challenges.len() != before
    }

    fn handle_control(&mut self, id: &str, line: &str) {
        let t = tokens(line);
        let arg = |i: usize| t.get(i).copied().unwrap_or("");
        match t.first().copied().unwrap_or("") {
            "STATUS" => self.set_status(id, arg(1), arg(2), arg(3), arg(4)),
            "CHALLENGE" => self.challenge(id, arg(1), arg(2)),
            "JOIN" => self.join(id, arg(1)),
            "CHAT" => self.chat(id, arg(1)),
            "CANCEL" => self.cancel(id, arg(1)),
            "ACCEPT" => self.accept(id, arg(1)),
            "REJECT" => self.reject(id, arg(1)),
            "READY" => self.ready(id),
            "ABORT" => self.abort(id, "aborted"),
            "BYE" => {} // handled by the read loop closing
            other => self.send(id, Frame::control(&["ERROR", &format!("unknown verb {other}")])),
        }
    }
}

fn handle_conn(stream: TcpStream, hub: Arc<Mutex<Hub>>) {
    let _ = stream.set_nodelay(true);
    let mut reader = match stream.try_clone() {
        Ok(r) => r,
        Err(_) => return,
    };
    let (tx, rx) = mpsc::channel::<Frame>();

    // Writer thread: drains outbound frames to the socket until all senders drop.
    let writer = thread::spawn(move || {
        let mut w = stream;
        for frame in rx {
            if write_frame(&mut w, &frame).is_err() {
                break;
            }
        }
        let _ = w.shutdown(Shutdown::Both);
    });

    // First frame must be HELLO.
    let id = match read_frame(&mut reader) {
        Ok(Frame::Control(line)) => {
            let t = tokens(&line);
            if t.first().copied() != Some("HELLO") || t.get(1).map_or(true, |h| h.is_empty()) {
                let _ = tx.send(Frame::control(&["ERROR", "expected HELLO"]));
                drop(tx);
                let _ = writer.join();
                return;
            }
            let handle = sanitize(t[1]);
            hub.lock().unwrap().register(&handle, tx.clone())
        }
        _ => {
            drop(tx);
            let _ = writer.join();
            return;
        }
    };

    // Main read loop.
    loop {
        match read_frame(&mut reader) {
            Ok(Frame::Control(line)) => {
                if tokens(&line).first().copied() == Some("BYE") {
                    break;
                }
                hub.lock().unwrap().handle_control(&id, &line);
            }
            Ok(Frame::Serial(bytes)) => hub.lock().unwrap().relay_serial(&id, bytes),
            Ok(Frame::Session(bytes)) => hub.lock().unwrap().relay_session(&id, bytes),
            Err(_) => break, // disconnect / EOF
        }
    }

    hub.lock().unwrap().remove_user(&id, "peer left");
    drop(tx); // ends the writer thread
    let _ = reader.shutdown(Shutdown::Both);
    let _ = writer.join();
}

fn main() {
    let port: u16 = env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(9999);
    let listener = TcpListener::bind(("0.0.0.0", port)).unwrap_or_else(|e| {
        eprintln!("[link-server] bind :{port} failed: {e}");
        std::process::exit(1);
    });
    eprintln!("[link-server] listening on :{port}");
    let hub = Arc::new(Mutex::new(Hub::default()));
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let hub = Arc::clone(&hub);
                thread::spawn(move || handle_conn(s, hub));
            }
            Err(e) => eprintln!("[link-server] accept error: {e}"),
        }
    }
}
