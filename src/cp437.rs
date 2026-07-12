//! UTF-8 -> CP437 output adapter.
//!
//! BBS terminals receive door output as CP437: bytes
//! pass through unchanged to CP437 clients and are mapped up to Unicode for
//! UTF-8 clients. A door that emits raw UTF-8 therefore garbles on every client.
//!
//! The in-game renderer already emits the single CP437 half-block byte (0xDF),
//! but the menu draws with crossterm's `Print`, which only emits UTF-8. Rather
//! than rewrite all 45 print sites, the menu wraps its writer in `Cp437Writer`,
//! which converts UTF-8 text to CP437 on the fly. crossterm's cursor/colour
//! commands are pure ASCII and pass through untouched; only Unicode glyphs from
//! `Print(...)` are remapped. (Local patch -- see PATCH-NOTES.md)

use std::io::{self, Write};

/// Map a Unicode char to its closest CP437 byte. Covers exactly the glyphs the
/// menu uses; anything else falls back to '?'. ASCII (incl. ESC and the rest of
/// the C0 controls used by ANSI sequences) maps to itself.
fn unicode_to_cp437(c: char) -> u8 {
    match c {
        // Double-line box drawing
        '═' => 0xCD,
        '║' => 0xBA,
        '╔' => 0xC9,
        '╗' => 0xBB,
        '╚' => 0xC8,
        '╝' => 0xBC,
        '╠' => 0xCC,
        '╣' => 0xB9,
        '╦' => 0xCB,
        '╩' => 0xCA,
        // Mixed double/single box drawing (the SG-1000 doodle)
        '╓' => 0xD6,
        '╖' => 0xB7,
        '╙' => 0xD3,
        '╜' => 0xBD,
        // Single-line / blocks
        '─' => 0xC4,
        '│' => 0xB3,
        '┌' => 0xDA,
        '┐' => 0xBF,
        '└' => 0xC0,
        '┘' => 0xD9,
        '├' => 0xC3,
        '┤' => 0xB4,
        '■' | '▪' => 0xFE,
        '·' => 0xFA,
        '∙' => 0xF9,
        '▬' => 0x16,
        '█' => 0xDB,
        '░' => 0xB0,
        '▒' => 0xB1,
        '▓' => 0xB2,
        '▀' => 0xDF,
        '▄' => 0xDC,
        '▌' => 0xDD,
        '▐' => 0xDE,
        // Arrows / triangles
        '▲' => 0x1E,
        '▼' => 0x1F,
        '◄' => 0x11,
        '►' | '▶' => 0x10,
        '↑' => 0x18,
        '↓' => 0x19,
        // Punctuation with no exact CP437 equivalent
        '…' => b'.',
        '✓' => 0xFB,
        '—' => b'-',
        '≈' => 0xF7,
        c if (c as u32) < 0x80 => c as u8,
        _ => b'?',
    }
}

/// A `Write` that converts UTF-8 input to CP437 bytes before forwarding.
pub struct Cp437Writer<W: Write> {
    inner: W,
    /// Holds an incomplete trailing UTF-8 sequence between writes.
    pending: Vec<u8>,
}

impl<W: Write> Cp437Writer<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            pending: Vec::new(),
        }
    }
}

impl<W: Write> Write for Cp437Writer<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        let mut out = Vec::with_capacity(self.pending.len());
        let mut consumed = 0;
        while consumed < self.pending.len() {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(s) => {
                    for c in s.chars() {
                        out.push(unicode_to_cp437(c));
                    }
                    consumed = self.pending.len();
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        // SAFETY: bytes [consumed, consumed+valid) are valid UTF-8.
                        let s = unsafe {
                            std::str::from_utf8_unchecked(&self.pending[consumed..consumed + valid])
                        };
                        for c in s.chars() {
                            out.push(unicode_to_cp437(c));
                        }
                        consumed += valid;
                    }
                    match e.error_len() {
                        Some(len) => {
                            // Genuinely invalid bytes: emit a placeholder and skip.
                            out.push(b'?');
                            consumed += len;
                        }
                        None => break, // Incomplete trailing sequence; keep for next write.
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        self.inner.write_all(&out)?;
        // Report the full input as consumed: leftover bytes live in `pending`.
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(s: &str) -> Vec<u8> {
        let mut w = Cp437Writer::new(Vec::new());
        w.write_all(s.as_bytes()).unwrap();
        w.inner
    }

    #[test]
    fn ascii_passes_through() {
        assert_eq!(conv("Game Boy 123!"), b"Game Boy 123!");
    }

    #[test]
    fn ansi_escape_sequences_pass_through() {
        // A typical crossterm truecolor SGR + cursor move must survive verbatim.
        let s = "\x1b[38;2;130;180;255m\x1b[5;10HX\x1b[0m";
        assert_eq!(conv(s), s.as_bytes());
    }

    #[test]
    fn glyphs_map_to_cp437() {
        assert_eq!(conv("╔═╗"), vec![0xC9, 0xCD, 0xBB]);
        assert_eq!(conv("║"), vec![0xBA]);
        assert_eq!(conv("╚═╝"), vec![0xC8, 0xCD, 0xBC]);
        assert_eq!(conv("█▀▄▌▐"), vec![0xDB, 0xDF, 0xDC, 0xDD, 0xDE]);
        assert_eq!(conv("◄►▲▼"), vec![0x11, 0x10, 0x1E, 0x1F]);
        assert_eq!(conv("↑↓"), vec![0x18, 0x19]);
        assert_eq!(conv("▶"), vec![0x10]);
        assert_eq!(conv("✓"), vec![0xFB]);
        assert_eq!(conv("…"), vec![b'.']);
    }

    #[test]
    fn no_utf8_multibyte_leaks_for_logo_line() {
        let line = "  ████████╗███████╗██████╗ ███╗   ███╗██╗";
        let out = conv(line);
        // Every byte is either ASCII or one of the single CP437 bytes this logo
        // line uses (block 0xDB + double-line box drawing). Crucially, no 3-byte
        // UTF-8 sequence survived -- the output is shorter than the UTF-8 input.
        assert!(
            out.iter()
                .all(|&b| b < 0x80 || matches!(b, 0xDB | 0xC9 | 0xCD | 0xBB | 0xBA | 0xC8 | 0xBC)),
            "unexpected byte in {:?}",
            out
        );
        assert!(
            out.len() < line.len(),
            "UTF-8 glyphs were not collapsed to single bytes"
        );
    }

    #[test]
    fn multibyte_split_across_writes() {
        // Feed the 3 bytes of ╗ (0xE2 0x95 0x97) one at a time.
        let bytes = "╗".as_bytes().to_vec();
        let mut w = Cp437Writer::new(Vec::new());
        for b in &bytes {
            w.write_all(&[*b]).unwrap();
        }
        assert_eq!(w.inner, vec![0xBB]);
    }
}
