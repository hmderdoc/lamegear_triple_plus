//! Delay-based lockstep for input-mirroring netplay (spec §4).
//!
//! Both peers boot the same ROM fresh and simulate every frame identically;
//! only held-button masks cross the network. Frame F executes only when both
//! slots' inputs for F are known (the emulation clock is the network clock —
//! contract §4.3.1). Local input sampled at frame F is scheduled for frame
//! F + D, where D = ceil(rtt/frame) + 1 was negotiated at session start, so
//! under steady latency neither side ever stalls.
//!
//! Every `CRC_INTERVAL` frames each side CRC32s its full serialized state and
//! exchanges it; any mismatch aborts loudly with the frame number and both
//! CRCs (contract §4.3.9 — never diverge silently).
//!
//! This module is pure state (no sockets, no clocks): main.rs wires it to
//! `Multiplayer` and the emulator, and the tests drive two instances against
//! each other in-process.

use std::collections::HashMap;

pub const CRC_INTERVAL: u32 = 60;
/// Refuse to run further than this many frames past the newest peer input.
/// (With delay-based lockstep the natural bound is D; this is a hard cap so a
/// bug can't let one side run away.)
const MAX_LEAD: u32 = 32;

/// Button-mask bit layout (mirrors input::button_index order).
pub fn mask_from_held(held: &[bool; crate::input::BUTTON_COUNT]) -> u16 {
    let mut m = 0u16;
    for (i, &h) in held.iter().enumerate() {
        if h {
            m |= 1 << i;
        }
    }
    m
}

/// Fold both slots' masks into door inputs: slot 0 = player 1 pad, slot 1 =
/// player 2 pad. Per-machine fan-out (which pad bit is which console button)
/// happens in emu.rs, identically on both peers.
pub fn masks_to_door_inputs(p1: u16, p2: u16) -> crate::emu::DoorInputs {
    let pad = |m: u16| {
        let mut held = [false; crate::input::BUTTON_COUNT];
        for (i, h) in held.iter_mut().enumerate() {
            *h = m & (1 << i) != 0;
        }
        held
    };
    crate::emu::DoorInputs { p1: pad(p1), p2: pad(p2) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputMsg {
    pub frame: u32,
    pub slot: u8,
    pub buttons: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Desync {
    pub frame: u32,
    pub ours: u32,
    pub theirs: u32,
}

pub struct Lockstep {
    pub local_slot: u8,
    pub delay: u32,
    /// Next frame to execute.
    frame: u32,
    /// buttons-by-frame for each slot. Pruned as frames execute.
    inputs: [HashMap<u32, u16>; 2],
    /// State CRCs at CRC_INTERVAL boundaries, ours and the peer's.
    our_crcs: HashMap<u32, u32>,
    peer_crcs: HashMap<u32, u32>,
    /// Newest local frame scheduled (sampling cursor).
    next_local: u32,
}

impl Lockstep {
    pub fn new(local_slot: u8, delay: u8) -> Lockstep {
        let delay = (delay as u32).clamp(1, MAX_LEAD - 1);
        let mut ls = Lockstep {
            local_slot,
            delay,
            frame: 0,
            inputs: [HashMap::new(), HashMap::new()],
            our_crcs: HashMap::new(),
            peer_crcs: HashMap::new(),
            next_local: delay,
        };
        // Contract: the first D frames run on empty input for BOTH slots —
        // each side prefill s both by convention, so nothing needs to cross
        // the wire before frame 0 can execute.
        for f in 0..delay {
            ls.inputs[0].insert(f, 0);
            ls.inputs[1].insert(f, 0);
        }
        ls
    }

    /// Compute the input delay from a measured round-trip: D = ceil(rtt/frame)
    /// + 1 (spec §4.3), clamped to something playable.
    pub fn delay_for_rtt(rtt_ms: f64, frame_ms: f64) -> u8 {
        ((rtt_ms / frame_ms).ceil() as u32 + 1).clamp(1, 12) as u8
    }

    pub fn current_frame(&self) -> u32 {
        self.frame
    }

    /// Record a peer (or replayed) input mask.
    pub fn push_input(&mut self, msg: InputMsg) {
        if msg.slot > 1 {
            return;
        }
        // First write wins: inputs for an already-executed or already-known
        // frame must not change (that would be a silent desync).
        self.inputs[msg.slot as usize].entry(msg.frame).or_insert(msg.buttons);
    }

    /// True when frame `current_frame()` has inputs for both slots.
    pub fn ready(&self) -> bool {
        self.inputs[0].contains_key(&self.frame) && self.inputs[1].contains_key(&self.frame)
    }

    /// Attempt to execute one frame: samples `local_mask` for frame
    /// `next_local` (returning the InputMsg to transmit), and if the current
    /// frame is fully known returns the two masks to feed the core.
    ///
    /// Returns (to_send, step): `to_send` must be transmitted even when the
    /// step stalls — that's what un-stalls the peer.
    pub fn tick(&mut self, local_mask: u16) -> (Option<InputMsg>, Option<(u16, u16)>) {
        // Schedule local input only while we're allowed to lead. Sampling is
        // tied to execution (one local sample per executed frame), except the
        // very first window where next_local == delay > frame.
        let mut to_send = None;
        if self.next_local < self.frame + self.delay + 1
            && !self.inputs[self.local_slot as usize].contains_key(&self.next_local)
        {
            let msg = InputMsg { frame: self.next_local, slot: self.local_slot, buttons: local_mask };
            self.inputs[self.local_slot as usize].insert(self.next_local, local_mask);
            self.next_local += 1;
            to_send = Some(msg);
        }

        if !self.ready() {
            return (to_send, None);
        }
        let p0 = self.inputs[0][&self.frame];
        let p1 = self.inputs[1][&self.frame];
        // Prune far-past entries so the maps stay tiny.
        if self.frame >= MAX_LEAD {
            let old = self.frame - MAX_LEAD;
            self.inputs[0].remove(&old);
            self.inputs[1].remove(&old);
        }
        self.frame += 1;
        (to_send, Some((p0, p1)))
    }

    /// Whether a state CRC is due after executing frame `f` (call with
    /// current_frame() - 1 right after a step).
    pub fn crc_due(f: u32) -> bool {
        f % CRC_INTERVAL == 0
    }

    /// Record our own state CRC for frame `f`; compare if the peer's arrived.
    pub fn note_our_crc(&mut self, f: u32, crc: u32) -> Option<Desync> {
        self.our_crcs.insert(f, crc);
        self.compare(f)
    }

    /// Record the peer's state CRC; compare if ours is known.
    pub fn note_peer_crc(&mut self, f: u32, crc: u32) -> Option<Desync> {
        self.peer_crcs.insert(f, crc);
        self.compare(f)
    }

    fn compare(&mut self, f: u32) -> Option<Desync> {
        let (ours, theirs) = (self.our_crcs.get(&f).copied()?, self.peer_crcs.get(&f).copied()?);
        // Matched (or reported) frames can be dropped, plus anything older.
        self.our_crcs.retain(|&k, _| k > f);
        self.peer_crcs.retain(|&k, _| k > f);
        if ours != theirs {
            return Some(Desync { frame: f, ours, theirs });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two peers exchanging messages "instantly": both must execute the exact
    /// same (p0, p1) sequence, one frame at a time, with no stalls.
    #[test]
    fn zero_latency_session_stays_in_step() {
        let mut a = Lockstep::new(0, 3);
        let mut b = Lockstep::new(1, 3);
        let mut a_frames = Vec::new();
        let mut b_frames = Vec::new();
        for i in 0u32..200 {
            let a_mask = (i * 7 % 97) as u16;
            let b_mask = (i * 13 % 89) as u16;
            let (a_msg, a_step) = a.tick(a_mask);
            let (b_msg, b_step) = b.tick(b_mask);
            if let Some(m) = a_msg {
                b.push_input(m);
            }
            if let Some(m) = b_msg {
                a.push_input(m);
            }
            if let Some(s) = a_step {
                a_frames.push(s);
            }
            if let Some(s) = b_step {
                b_frames.push(s);
            }
        }
        assert!(a_frames.len() >= 190, "should almost never stall at zero latency");
        let n = a_frames.len().min(b_frames.len());
        assert_eq!(a_frames[..n], b_frames[..n], "both peers must see identical input streams");
    }

    /// Messages delivered with a delivery lag shorter than D: still no stalls
    /// once past the priming window.
    #[test]
    fn latency_under_delay_never_stalls_after_priming() {
        let delay = 5u8;
        let lag = 3usize; // frames of network delay, < D
        let mut a = Lockstep::new(0, delay);
        let mut b = Lockstep::new(1, delay);
        let mut a_inbox: std::collections::VecDeque<(usize, InputMsg)> = Default::default();
        let mut b_inbox: std::collections::VecDeque<(usize, InputMsg)> = Default::default();
        let mut a_steps = 0;
        let mut stalls_after_priming = 0;
        for i in 0usize..300 {
            while a_inbox.front().is_some_and(|(due, _)| *due <= i) {
                let (_, m) = a_inbox.pop_front().unwrap();
                a.push_input(m);
            }
            while b_inbox.front().is_some_and(|(due, _)| *due <= i) {
                let (_, m) = b_inbox.pop_front().unwrap();
                b.push_input(m);
            }
            let (a_msg, a_step) = a.tick(i as u16);
            let (b_msg, b_step) = b.tick((i * 3) as u16);
            if let Some(m) = a_msg {
                b_inbox.push_back((i + lag, m));
            }
            if let Some(m) = b_msg {
                a_inbox.push_back((i + lag, m));
            }
            if a_step.is_some() {
                a_steps += 1;
            } else if i > delay as usize + lag {
                stalls_after_priming += 1;
            }
            let _ = b_step;
        }
        assert_eq!(stalls_after_priming, 0, "lag < D must not stall");
        assert!(a_steps > 280);
    }

    /// When the peer's inputs stop arriving, execution stops (never runs
    /// ahead on guessed input) and resumes exactly where it left off.
    #[test]
    fn missing_peer_input_stalls_then_resumes() {
        let mut a = Lockstep::new(0, 2);
        // Peer never sends: after the 2 primed frames, a must stall.
        let mut steps = 0;
        let mut pending = Vec::new();
        for i in 0..10 {
            let (msg, step) = a.tick(i as u16);
            if let Some(m) = msg {
                pending.push(m);
            }
            if step.is_some() {
                steps += 1;
            }
        }
        assert_eq!(steps, 2, "only the primed window executes without the peer");
        // Deliver the peer's inputs late; execution resumes deterministically.
        for f in 2..8u32 {
            a.push_input(InputMsg { frame: f, slot: 1, buttons: 0xF });
        }
        let mut resumed = Vec::new();
        for i in 10..16 {
            let (_, step) = a.tick(i as u16);
            if let Some(s) = step {
                resumed.push(s);
            }
        }
        assert_eq!(resumed.len(), 6);
        assert!(resumed.iter().all(|&(_, p1)| p1 == 0xF));
    }

    #[test]
    fn crc_mismatch_is_reported_with_both_values() {
        let mut a = Lockstep::new(0, 2);
        assert_eq!(a.note_our_crc(60, 0xAAAA), None); // peer's not in yet
        let d = a.note_peer_crc(60, 0xBBBB).expect("mismatch must be reported");
        assert_eq!(d, Desync { frame: 60, ours: 0xAAAA, theirs: 0xBBBB });
        // Matching CRCs are silent, in either arrival order.
        let mut b = Lockstep::new(0, 2);
        assert_eq!(b.note_peer_crc(120, 7), None);
        assert_eq!(b.note_our_crc(120, 7), None);
    }

    #[test]
    fn first_write_wins_on_duplicate_input() {
        let mut a = Lockstep::new(0, 1);
        a.push_input(InputMsg { frame: 5, slot: 1, buttons: 1 });
        a.push_input(InputMsg { frame: 5, slot: 1, buttons: 2 }); // retransmit/dup
        assert_eq!(a.inputs[1][&5], 1);
    }

    #[test]
    fn masks_round_trip_to_door_inputs() {
        let mut held = [false; crate::input::BUTTON_COUNT];
        held[0] = true; // up
        held[4] = true; // button1
        held[8] = true; // select
        let m = mask_from_held(&held);
        let inputs = masks_to_door_inputs(m, 0);
        assert!(inputs.p1[0] && inputs.p1[4] && inputs.p1[8]);
        assert!(!inputs.p2[0]);
        let inputs2 = masks_to_door_inputs(0, m);
        assert!(inputs2.p2[0] && inputs2.p2[4] && inputs2.p2[8]);
    }
}
