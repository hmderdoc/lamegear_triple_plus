//! Wire protocol shared by the link server and the door's multiplayer client.
//!
//! One TCP connection carries both control traffic (presence, challenges, link
//! coordination) and, once two callers are linked, the raw Game Boy serial
//! bytes. Every message is a length-framed packet so the two never collide:
//!
//!   frame = [type: u8][len: u16 big-endian][payload: len bytes]
//!
//! `type = 1` (control): payload is a UTF-8 line of TAB-separated tokens, the
//! first being the verb (HELLO, ROSTER_ADD, CHALLENGE, …). `type = 2` (serial):
//! payload is raw serial bytes the relay cross-forwards to the linked peer.
//!
//! Control verbs are documented in `main.rs` (server side) and the door client.
//! Handles/tags are sanitized to contain no TAB or newline so tokenizing is safe.

use std::io::{self, Read, Write};

pub const FRAME_CONTROL: u8 = 1;
pub const FRAME_SERIAL: u8 = 2;
/// Opaque lockstep-session payload (input, video, setup, end — sub-typed by
/// the door; the server relays it verbatim to the session peer like serial).
pub const FRAME_SESSION: u8 = 3;
pub const MAX_FRAME: usize = 8192;

#[derive(Debug, Clone)]
pub enum Frame {
    Control(String),
    Serial(Vec<u8>),
    Session(Vec<u8>),
}

impl Frame {
    /// Build a control frame from a verb + args (joined by TAB).
    pub fn control(parts: &[&str]) -> Frame {
        Frame::Control(parts.join("\t"))
    }
}

/// Parse a control payload into (verb, args). Empty trailing tokens are kept so
/// positional optional fields (e.g. an empty game name) round-trip.
pub fn tokens(line: &str) -> Vec<&str> {
    line.split('\t').collect()
}

/// Replace TAB/newline in a user-supplied field so it can't break framing.
pub fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c }).collect()
}

pub fn write_frame(w: &mut impl Write, frame: &Frame) -> io::Result<()> {
    let (t, payload): (u8, &[u8]) = match frame {
        Frame::Control(s) => (FRAME_CONTROL, s.as_bytes()),
        Frame::Serial(b) => (FRAME_SERIAL, b),
        Frame::Session(b) => (FRAME_SESSION, b),
    };
    if payload.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let len = payload.len() as u16;
    w.write_all(&[t, (len >> 8) as u8, len as u8])?;
    w.write_all(payload)?;
    w.flush()
}

pub fn read_frame(r: &mut impl Read) -> io::Result<Frame> {
    let mut hdr = [0u8; 3];
    r.read_exact(&mut hdr)?;
    let t = hdr[0];
    let len = ((hdr[1] as usize) << 8) | hdr[2] as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    match t {
        FRAME_CONTROL => Ok(Frame::Control(String::from_utf8_lossy(&payload).into_owned())),
        FRAME_SERIAL => Ok(Frame::Serial(payload)),
        FRAME_SESSION => Ok(Frame::Session(payload)),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "bad frame type")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        for f in [
            Frame::control(&["HELLO", "Dragon", "FUTURELD"]),
            Frame::Serial(vec![0x01, 0xFF, 0x00, 0x99]),
            Frame::Control(String::new()),
        ] {
            let mut buf = Vec::new();
            write_frame(&mut buf, &f).unwrap();
            let got = read_frame(&mut &buf[..]).unwrap();
            match (f, got) {
                (Frame::Control(a), Frame::Control(b)) => assert_eq!(a, b),
                (Frame::Serial(a), Frame::Serial(b)) => assert_eq!(a, b),
                _ => panic!("frame type mismatch"),
            }
        }
    }

    #[test]
    fn sanitize_strips_delimiters() {
        assert_eq!(sanitize("a\tb\nc"), "a b c");
    }

    #[test]
    fn tokens_keep_empty_trailing() {
        assert_eq!(tokens("CHALLENGE\tDragon@X\t"), vec!["CHALLENGE", "Dragon@X", ""]);
    }
}
