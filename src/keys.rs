//! Decode a raw telnet/ANSI input byte stream into key events.
//!
//! Replaces crossterm's console event reader: over an inherited door socket
//! there is no console to read events from, so we parse the bytes ourselves.
//! Handles telnet IAC negotiation (stripped), arrow keys as `ESC [` / `ESC O`
//! sequences, a lone `ESC` as the Esc key (resolved on the next idle poll), and
//! CR / CRLF / CR-NUL as Enter.
//!
//! There are no key-up events over a socket; callers treat every key as a press
//! and rely on the existing button-release timeout.

use crate::color::ColorDepth;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    PageUp,
    PageDown,
}

// Telnet
const IAC: u8 = 255;
const SE: u8 = 240;
const SB: u8 = 250;
const WILL: u8 = 251;
const WONT: u8 = 252;
const DO: u8 = 253;
const DONT: u8 = 254;

#[derive(Clone, Copy, PartialEq)]
enum State {
    Ground,
    Esc,    // saw ESC, awaiting continuation (or idle -> Key::Esc)
    Csi,    // saw ESC [ , accumulating to a final byte
    Ss3,    // saw ESC O , awaiting one byte
    Dcs,    // saw ESC P , accumulating a device-control string (DECRQSS reply)
    DcsEsc, // inside DCS, saw ESC , awaiting the `\` of the ST terminator
    Iac,    // saw 0xFF
    IacOpt, // saw IAC + WILL/WONT/DO/DONT, awaiting option byte
    IacSub, // inside SB ... , awaiting IAC
    IacSubIac, // inside SB, saw IAC, awaiting SE
}

/// Longest DCS body we buffer before giving up (a malformed stream with no ST
/// terminator can't be allowed to grow unbounded).
const MAX_DCS_LEN: usize = 64;

/// Which keyboard-input protocol the connected terminal supports, resolved from
/// the startup capability probes (folded into the size-probe handshake). Selects
/// how key edges are read; `Legacy` is the universal fallback (translated keys +
/// the button-release timeout).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyboardMode {
    /// SyncTERM/CTerm physical key reports (`CSI = 1 h`, evdev `K`/`k` edges).
    CtermPhysical,
    /// Kitty keyboard protocol (`CSI > flags u`, CSI-u events with press/release).
    Kitty,
    /// No enhanced protocol: translated keys only.
    Legacy,
}

/// A physical key edge from a CTerm physical-key report (`CSI = Pk;… K|k`):
/// the evdev key code and whether it went down (`pressed`) or up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyEdge {
    pub code: u16,
    pub pressed: bool,
}

pub struct KeyDecoder {
    state: State,
    cr_seen: bool,             // swallow the LF/NUL that may follow a CR
    csi: String,               // accumulated CSI parameter bytes
    dcs: String,               // accumulated DCS body (DECRQSS color readback)
    cursor: Option<(u16, u16)>, // last cursor-position report (row, col)
    audio_drain: Option<u8>,   // channel from a `CSI =7;ch;0 n` drain report
    // Keyboard capability flags, set as the probe replies arrive.
    phys_keys: bool,           // CTDA (`CSI < ... c`) advertised value 8
    sixel: bool,               // CTDA or Primary DA advertised value 4
    kitty_keys: bool,          // kitty query reply (`CSI ? flags u`)
    da1_seen: bool,            // Primary DA reply (`CSI ? ... c`) — the probe barrier
    cell_pixels: Option<(u16, u16)>, // reported character cell (height, width)
    text_area_px: Option<(u16, u16)>, // reported text area (height, width) in pixels
    gfx_geometry: Option<(u16, u16)>, // XTSMGRAPHICS sixel geometry (height, width) px
    // Color depth resolved from the DECRQSS probe reply, if the terminal answered:
    // Some(C256) = it quantized our 24-bit set (downgrade), Some(True) = kept 24-bit.
    // A stable capability once seen — peeked, not consumed.
    color_probe: Option<ColorDepth>,
    key_edges: Vec<KeyEdge>,   // physical key press/release edges awaiting the caller
    kitty_active: bool,        // kitty mode enabled: route CSI-u/arrow events to edges
    kitty_form_seen: bool,     // a kitty event-type arrived without kitty_active
                               // (a proxy switched the terminal upstream): releases
                               // are proven to come, so arrows can go full-edge
}

impl Default for KeyDecoder {
    fn default() -> Self {
        Self {
            state: State::Ground,
            cr_seen: false,
            csi: String::new(),
            dcs: String::new(),
            cursor: None,
            audio_drain: None,
            phys_keys: false,
            sixel: false,
            kitty_keys: false,
            da1_seen: false,
            cell_pixels: None,
            text_area_px: None,
            gfx_geometry: None,
            color_probe: None,
            key_edges: Vec::new(),
            kitty_active: false,
            kitty_form_seen: false,
        }
    }
}

impl KeyDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode a chunk of input bytes into zero or more keys.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        let mut out = Vec::new();
        for &b in bytes {
            self.step(b, &mut out);
        }
        out
    }

    /// Call when a poll returned no bytes: resolves a lone trailing ESC into the
    /// Esc key (it wasn't the start of an escape sequence after all). Also drops a
    /// dangling DCS whose ST terminator never arrived so we can't wedge there
    /// (a real DECRQSS reply arrives whole, so this only fires on a malformed one).
    pub fn idle(&mut self) -> Option<Key> {
        match self.state {
            State::Esc => {
                self.state = State::Ground;
                Some(Key::Esc)
            }
            State::Dcs | State::DcsEsc => {
                self.dcs.clear();
                self.state = State::Ground;
                None
            }
            _ => None,
        }
    }

    /// The color depth resolved from the DECRQSS probe reply, if the terminal
    /// answered. Sticky (a stable capability): safe to read repeatedly.
    pub fn color_probe(&self) -> Option<ColorDepth> {
        self.color_probe
    }

    /// Whether CTerm or Primary DA explicitly advertised SIXEL graphics.
    pub fn sixel_supported(&self) -> bool {
        self.sixel
    }

    /// Character-cell pixel dimensions (height, width), when reported by CTerm
    /// (`CSI = 3 n`) or an xterm-family terminal (`CSI 16 t`).
    pub fn cell_pixels(&self) -> Option<(u16, u16)> {
        self.cell_pixels
    }

    /// Text-area pixel dimensions (height, width) from `CSI 14 t` — the
    /// ground truth for the sixel canvas. Font-size × grid arithmetic can
    /// disagree with reality after a resize (mode/font switches, partial
    /// grants, hidpi); this is the terminal's own answer.
    pub fn text_area_pixels(&self) -> Option<(u16, u16)> {
        self.text_area_px
    }

    /// XTSMGRAPHICS sixel geometry (height, width) in pixels, from
    /// `CSI ? 2 ; 1 ; 0 S`. CTerm answers with the EMULATED SCREEN size —
    /// its equivalent of the `CSI 14 t` it doesn't speak. Callers must
    /// sanity-check against the character grid: xterm-family replies with
    /// the max ALLOWED graphic size here, which can be far larger than the
    /// window.
    pub fn gfx_geometry(&self) -> Option<(u16, u16)> {
        self.gfx_geometry
    }

    /// Take the most recent cursor-position report (`ESC [ row ; col R`), if the
    /// terminal answered our size probe since the last call.
    pub fn take_cursor(&mut self) -> Option<(u16, u16)> {
        self.cursor.take()
    }

    /// Take the most recent audio drain report (`CSI = 7 ; ch ; 0 n`): the
    /// channel's FIFO emptied since the last call. Returns the channel number.
    pub fn take_audio_drain(&mut self) -> Option<u8> {
        self.audio_drain.take()
    }

    /// The keyboard protocol resolved from the capability probes. CTerm physical
    /// keys win over kitty if a terminal somehow advertised both.
    pub fn keyboard_mode(&self) -> KeyboardMode {
        if self.phys_keys {
            KeyboardMode::CtermPhysical
        } else if self.kitty_keys {
            KeyboardMode::Kitty
        } else {
            KeyboardMode::Legacy
        }
    }

    /// True once the Primary DA reply (the probe barrier) has arrived, so the
    /// capability probes need not be re-sent with further size probes.
    pub fn caps_resolved(&self) -> bool {
        self.da1_seen
    }

    /// Parse the evdev codes from a physical-key report body (`= Pk;Pk…`) into
    /// edges. Bodies without the `=` prefix aren't physical reports — ignore them.
    fn push_key_edges(&mut self, pressed: bool) {
        if let Some(rest) = self.csi.strip_prefix('=') {
            for p in rest.split(';') {
                if let Ok(code) = p.parse::<u16>() {
                    self.key_edges.push(KeyEdge { code, pressed });
                }
            }
        }
    }

    /// Drain the physical key edges decoded since the last call (edge modes).
    pub fn take_key_edges(&mut self) -> Vec<KeyEdge> {
        std::mem::take(&mut self.key_edges)
    }

    /// Enable/disable kitty event decoding (set when the door pushes the kitty
    /// keyboard flags). While on, CSI-u and arrow events become key edges instead
    /// of translated keys.
    pub fn set_kitty_active(&mut self, on: bool) {
        self.kitty_active = on;
        if !on {
            // Session teardown: the terminal was restored (kitty popped), so
            // colon-form evidence from the finished game no longer holds. The
            // menu needs bare arrows back on the translated path.
            self.kitty_form_seen = false;
        }
    }

    /// Push an edge from a kitty CSI-u key event (`CSI codepoint ; mods:event u`),
    /// normalising the Unicode keysym to the same evdev code space as CTerm.
    fn push_kitty_u(&mut self) {
        let cp = self
            .csi
            .split(';')
            .next()
            .and_then(|f| f.split(':').next())
            .and_then(|s| s.parse::<u32>().ok());
        if let Some(code) = cp.and_then(kitty_cp_to_evdev) {
            self.key_edges.push(KeyEdge { code, pressed: kitty_event(&self.csi) != 3 });
        }
    }

    /// Push an edge from a kitty arrow event (final byte `A`..`D`).
    fn push_kitty_arrow(&mut self, final_byte: u8) {
        self.key_edges.push(KeyEdge {
            code: arrow_evdev(final_byte),
            pressed: kitty_event(&self.csi) != 3,
        });
    }

    /// A DCS body completed (ST or BEL). Classify it as a color-probe reply and
    /// return to ground. The body never surfaces as keystrokes.
    fn finish_dcs(&mut self) {
        if let Some(depth) = parse_color_readback(&self.dcs) {
            self.color_probe = Some(depth);
        }
        self.dcs.clear();
        self.state = State::Ground;
    }

    fn step(&mut self, b: u8, out: &mut Vec<Key>) {
        match self.state {
            State::Iac => {
                self.state = match b {
                    WILL | WONT | DO | DONT => State::IacOpt,
                    SB => State::IacSub,
                    IAC => State::Ground, // escaped 0xFF data byte; ignore as input
                    _ => State::Ground,   // standalone command (NOP, etc.)
                };
            }
            State::IacOpt => self.state = State::Ground,
            State::IacSub => {
                if b == IAC {
                    self.state = State::IacSubIac;
                }
            }
            State::IacSubIac => {
                self.state = if b == SE { State::Ground } else { State::IacSub };
            }
            State::Esc => match b {
                b'[' => {
                    self.csi.clear();
                    self.state = State::Csi;
                }
                b'O' => self.state = State::Ss3,
                b'P' => {
                    // DCS introducer: a DECRQSS reply to our color probe. Buffer
                    // the body; it never surfaces as keys.
                    self.dcs.clear();
                    self.state = State::Dcs;
                }
                0x1b => out.push(Key::Esc), // previous ESC was lone; stay in Esc for this one
                _ => {
                    out.push(Key::Esc); // previous ESC was lone
                    self.state = State::Ground;
                    self.ground(b, out);
                }
            },
            State::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    match b {
                        b'A' | b'B' | b'C' | b'D' => {
                            // Arrows go FULL-EDGE (press/repeat hold the down-set,
                            // release clears it) whenever real releases are
                            // guaranteed: we negotiated kitty ourselves, or a
                            // kitty event-type form (`1;mods:event X`) proves a
                            // proxy switched the terminal upstream (iTerm2 behind
                            // BBSproxy). Bare presses MUST count as edges then:
                            // iTerm2 sends plain `CSI C` for an unmodified press
                            // even in kitty mode, and the 150ms translated timeout
                            // is far shorter than macOS's ~500ms initial repeat
                            // delay — translating them would drop every hold for
                            // ~350ms (the "no momentum" jump bug). Auto-repeat
                            // also stops when another key is chorded, so only
                            // edge semantics keep a held direction alive through
                            // a jump. Without kitty evidence, arrows stay on the
                            // translated (timeout) path — a terminal that never
                            // sends releases must not get stuck keys.
                            if self.csi.contains(':') {
                                self.kitty_form_seen = true;
                            }
                            if self.kitty_active || self.kitty_form_seen {
                                self.push_kitty_arrow(b);
                            } else {
                                out.push(match b {
                                    b'A' => Key::Up,
                                    b'B' => Key::Down,
                                    b'C' => Key::Right,
                                    _ => Key::Left,
                                });
                            }
                        }
                        b'Z' => out.push(Key::BackTab),
                        // SyncTERM/CTerm transmit Page Up as `CSI V` and Page Down
                        // as `CSI U` (cterm.c keytab: PPAGE=\033[V, NPAGE=\033[U).
                        // xterm-family terminals send `CSI 5~`/`6~` instead (below).
                        b'V' => out.push(Key::PageUp),
                        b'U' => out.push(Key::PageDown),
                        b'R' => self.cursor = parse_cursor(&self.csi), // size probe reply
                        b'n' => {
                            // Device-status report; `=7;ch;0` is an audio drain.
                            if let Some(ch) = parse_audio_drain(&self.csi) {
                                self.audio_drain = Some(ch);
                            }
                            if let Some(size) = parse_cterm_cell_size(&self.csi) {
                                self.cell_pixels = Some(size);
                            }
                        }
                        b'c' => {
                            // Device Attributes replies (folded into the size probe).
                            // `CSI < ...;8;... c` = CTerm CTDA (physical keys avail);
                            // `CSI ? ... c` = Primary DA, our probe barrier.
                            if let Some(rest) = self.csi.strip_prefix('<') {
                                for feature in rest.split(';') {
                                    match feature {
                                        "4" => self.sixel = true,
                                        "8" => self.phys_keys = true,
                                        _ => {}
                                    }
                                }
                            } else if self.csi.starts_with('?') {
                                if self.csi[1..].split(';').any(|p| p == "4") {
                                    self.sixel = true;
                                }
                                self.da1_seen = true;
                            }
                        }
                        b't' => {
                            if let Some(size) = parse_xterm_cell_size(&self.csi) {
                                self.cell_pixels = Some(size);
                            }
                            if let Some(size) = parse_xterm_text_area(&self.csi) {
                                self.text_area_px = Some(size);
                            }
                        }
                        b'u' => {
                            if self.csi.starts_with('?') {
                                // Kitty keyboard query reply: `CSI ? <flags> u`.
                                self.kitty_keys = true;
                            } else if self.kitty_active {
                                // Kitty key event: `CSI codepoint ; mods:event u`.
                                self.push_kitty_u();
                            }
                        }
                        b'S' => {
                            // XTSMGRAPHICS reply: `CSI ? 2 ; 0 ; W ; H S`
                            // (item 2 = sixel geometry, status 0 = success).
                            if let Some(rest) = self.csi.strip_prefix("?2;0;") {
                                let mut f = rest.split(';');
                                if let (Some(w), Some(h)) = (
                                    f.next().and_then(|v| v.parse::<u16>().ok()),
                                    f.next().and_then(|v| v.parse::<u16>().ok()),
                                ) {
                                    if w >= 64 && h >= 64 {
                                        self.gfx_geometry = Some((h, w));
                                    }
                                }
                            }
                        }
                        b'K' => self.push_key_edges(true),  // `CSI = Pk;… K` press
                        b'k' => self.push_key_edges(false), // `CSI = Pk;… k` release
                        b'~' => {
                            // VT edit-keypad: `CSI 5 ~` = Page Up, `CSI 6 ~` = Page
                            // Down. Modifier forms (`5;2~`) keep the same first field.
                            match self.csi.split(';').next() {
                                Some("5") => out.push(Key::PageUp),
                                Some("6") => out.push(Key::PageDown),
                                _ => {} // Home/End/Ins/Del (1/4/2/3~): ignored
                            }
                        }
                        _ => {} // other final bytes: ignored
                    }
                    self.state = State::Ground;
                } else {
                    // parameter / intermediate byte (digits, ';') — accumulate
                    self.csi.push(b as char);
                }
            }
            State::Ss3 => {
                match b {
                    b'A' => out.push(Key::Up),
                    b'B' => out.push(Key::Down),
                    b'C' => out.push(Key::Right),
                    b'D' => out.push(Key::Left),
                    _ => {}
                }
                self.state = State::Ground;
            }
            State::Dcs => {
                if b == 0x1b {
                    self.state = State::DcsEsc; // maybe the ST terminator `ESC \`
                } else if b == 0x07 {
                    self.finish_dcs(); // some terminals end with BEL
                } else if self.dcs.len() < MAX_DCS_LEN {
                    self.dcs.push(b as char);
                } else {
                    // Runaway (no terminator): drop it rather than grow forever.
                    self.dcs.clear();
                    self.state = State::Ground;
                }
            }
            State::DcsEsc => {
                // In a DCS, `ESC \` is the string terminator. Anything else means
                // that ESC wasn't a terminator; finish what we have and reprocess
                // the byte from ground so a following key isn't swallowed.
                if b == b'\\' {
                    self.finish_dcs();
                } else {
                    self.finish_dcs();
                    self.step(b, out);
                }
            }
            State::Ground => self.ground(b, out),
        }
    }

    fn ground(&mut self, b: u8, out: &mut Vec<Key>) {
        match b {
            IAC => self.state = State::Iac,
            0x1b => self.state = State::Esc,
            b'\r' => {
                out.push(Key::Enter);
                self.cr_seen = true;
                return;
            }
            b'\n' | 0x00 => {
                if self.cr_seen {
                    // swallow the LF / NUL that completes a CR-terminated line
                } else if b == b'\n' {
                    out.push(Key::Enter);
                }
            }
            b'\t' => out.push(Key::Tab),
            0x08 | 0x7f => out.push(Key::Backspace),
            0x20..=0x7e => out.push(Key::Char(b as char)),
            _ => {} // other control bytes: ignored
        }
        self.cr_seen = false;
    }
}

/// Classify a DECRQSS SGR reply body (the text between `ESC P` and the ST).
/// Our color probe set FG to an odd 24-bit value; the reply echoes `38;2;...`
/// when the terminal preserved 24-bit (truecolor) or `38;5;N` when it quantized
/// to 256 (some terminals use colon subparams, e.g. `38:2:...`). Only a
/// DECRQSS-shaped reply (contains `$r`) is trusted; anything else -> None, which
/// leaves the caller at its truecolor default (conservative, never over-downgrades).
fn parse_color_readback(body: &str) -> Option<ColorDepth> {
    if !body.contains("$r") {
        return None;
    }
    if body.contains("38;5") || body.contains("38:5") {
        Some(ColorDepth::C256)
    } else if body.contains("38;2") || body.contains("38:2") {
        Some(ColorDepth::True)
    } else {
        None
    }
}

/// Parse a CSI cursor-position report body ("row;col") into (row, col).
fn parse_cursor(params: &str) -> Option<(u16, u16)> {
    let mut it = params.split(';');
    let row = it.next()?.parse().ok()?;
    let col = it.next()?.parse().ok()?;
    Some((row, col))
}

fn valid_cell_size(height: u16, width: u16) -> Option<(u16, u16)> {
    (height > 0 && width > 0 && height <= 256 && width <= 256).then_some((height, width))
}

/// CTerm State/Mode Report: `CSI = 3 ; height ; width n`.
fn parse_cterm_cell_size(params: &str) -> Option<(u16, u16)> {
    let mut fields = params.split(';');
    if fields.next()? != "=3" {
        return None;
    }
    let height = fields.next()?.parse().ok()?;
    let width = fields.next()?.parse().ok()?;
    valid_cell_size(height, width)
}

/// xterm window-op response to `CSI 16 t`: `CSI 6 ; height ; width t`.
fn parse_xterm_cell_size(params: &str) -> Option<(u16, u16)> {
    let mut fields = params.split(';');
    if fields.next()? != "6" {
        return None;
    }
    let height = fields.next()?.parse().ok()?;
    let width = fields.next()?.parse().ok()?;
    valid_cell_size(height, width)
}

/// xterm window-op response to `CSI 14 t`: `CSI 4 ; height ; width t` — the
/// text area in pixels.
fn parse_xterm_text_area(params: &str) -> Option<(u16, u16)> {
    let mut fields = params.split(';');
    if fields.next()? != "4" {
        return None;
    }
    let height: u16 = fields.next()?.parse().ok()?;
    let width: u16 = fields.next()?.parse().ok()?;
    (height >= 64 && width >= 64).then_some((height, width))
}

/// Kitty event-type from a CSI body: the colon-suffix of the 2nd `;`-field
/// (`key ; mods:event ; text`). 1=press, 2=repeat, 3=release; default 1.
fn kitty_event(csi: &str) -> u8 {
    csi.split(';')
        .nth(1)
        .and_then(|mods| mods.split_once(':'))
        .and_then(|(_, ev)| ev.parse().ok())
        .unwrap_or(1)
}

/// The evdev code for an arrow-key CSI final byte (`A`..`D`), matching what
/// CTerm physical-key reports send for the same keys.
fn arrow_evdev(final_byte: u8) -> u16 {
    match final_byte {
        b'A' => 103, // up
        b'B' => 108, // down
        b'C' => 106, // right
        _ => 105,    // left (D)
    }
}

/// Map a kitty Unicode keysym to the evdev code space used by the edge decoder,
/// for every key the door reads in-game: the full pad set (Z/X/A/S/C/V,
/// Enter, Space — see input.rs), the Esc/Q quit keys, and the 5/8 save-state
/// keys. Kitty reports the unshifted (lowercase) codepoint, but we accept
/// upper too. A key missing here is DEAD in kitty mode (the event never
/// falls back to the translated path) — this table must grow with input.rs.
fn kitty_cp_to_evdev(cp: u32) -> Option<u16> {
    Some(match cp {
        122 | 90 => 44,  // z/Z -> button 1 (NES A, Genesis B, SNES B, GBA A)
        120 | 88 => 45,  // x/X -> button 2 (NES B, Genesis C, SNES A, GBA B)
        97 | 65 => 30,   // a/A -> button 3 (Genesis A, SNES Y)
        115 | 83 => 31,  // s/S -> button 4 (SNES X)
        99 | 67 => 46,   // c/C -> L shoulder (SNES, GBA)
        118 | 86 => 47,  // v/V -> R shoulder (SNES, GBA)
        13 => 28,        // Enter (Start / SMS pause)
        32 => 57,        // Space (Select)
        27 => 1,         // Esc (quit)
        113 | 81 => 16,  // q/Q (quit)
        112 | 80 => 25,  // p/P (unused here; kept for lameboy parity)
        53 => 6,         // 5 (save state)
        56 => 9,         // 8 (load state)
        _ => return None,
    })
}

/// Parse an audio status report body. The terminal answers `CSI = 7 [ ; id ;
/// state ]… n` (state 0 = stopped, 1 = running); the `Update;C=` one-shot fires
/// `=7;ch;0`. Returns the first channel reported stopped, if any.
fn parse_audio_drain(params: &str) -> Option<u8> {
    let rest = params.strip_prefix("=7")?;
    let mut it = rest.split(';').filter(|s| !s.is_empty());
    while let (Some(id), Some(state)) = (it.next(), it.next()) {
        if state == "0" {
            return id.parse().ok();
        }
    }
    None
}

/// Reads bytes from a `Term` and decodes them into keys — the input side of the
/// door, replacing crossterm's event reader. `poll` is non-blocking (drain what
/// arrived this frame); `wait` blocks until a key (used by the menu).
pub struct Input {
    decoder: KeyDecoder,
    buf: [u8; 2048],
    // Opt-in diagnostic: when set, every non-empty poll chunk is appended to this
    // file as `<ms>  <escaped bytes>` so a terminal's real key-report format (kitty
    // CSI-u press/release, CTerm evdev edges, bare arrows) can be inspected.
    keylog: Option<(std::fs::File, std::time::Instant)>,
}

impl Default for Input {
    fn default() -> Self {
        Self { decoder: KeyDecoder::new(), buf: [0u8; 2048], keylog: None }
    }
}

impl Input {
    pub fn new() -> Self {
        Self::default()
    }

    /// Non-blocking: decode whatever input is available right now. Resolves a
    /// pending lone-ESC into Esc when nothing else arrived. Propagates a closed
    /// connection as an error.
    pub fn poll(&mut self, term: &mut dyn crate::term::Term) -> std::io::Result<Vec<Key>> {
        let n = term.read_available(&mut self.buf)?;
        if n == 0 {
            return Ok(self.decoder.idle().into_iter().collect());
        }
        if let Some((f, start)) = self.keylog.as_mut() {
            use std::io::Write as _;
            let ms = start.elapsed().as_millis();
            let mut line = format!("{ms:>8}  ");
            for &b in &self.buf[..n] {
                match b {
                    0x1b => line.push_str("<ESC>"),
                    b'\r' => line.push_str("\\r"),
                    b'\n' => line.push_str("\\n"),
                    0x20..=0x7e => line.push(b as char),
                    _ => line.push_str(&format!("\\x{b:02x}")),
                }
            }
            line.push('\n');
            let _ = f.write_all(line.as_bytes());
        }
        Ok(self.decoder.feed(&self.buf[..n]))
    }

    /// Enable an opt-in raw-input keylog (diagnostic; `--keylog <path>`). Every
    /// non-empty `poll` chunk is appended with an elapsed-ms prefix and ESC/control
    /// bytes rendered visibly. No-op if the file can't be opened.
    pub fn enable_keylog(&mut self, path: &str) {
        if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            self.keylog = Some((f, std::time::Instant::now()));
        }
    }

    /// Block until a key arrives, or until the terminal answers a size probe —
    /// so the menu can re-layout to the real screen height without waiting for a
    /// keystroke. Mirrors `wait`'s lone-ESC debounce.
    pub fn wait_event(&mut self, term: &mut dyn crate::term::Term) -> std::io::Result<MenuEvent> {
        self.wait_event_timeout(term, std::time::Duration::from_secs(1))
    }

    /// Variant used by interactive menus that need a short quiet-period tick
    /// (for example, to debounce an expensive artwork repaint).
    pub fn wait_event_timeout(
        &mut self,
        term: &mut dyn crate::term::Term,
        timeout: std::time::Duration,
    ) -> std::io::Result<MenuEvent> {
        let mut empties = 0u32;
        let empty_limit = (timeout.as_millis() / 15).clamp(2, u32::MAX as u128) as u32;
        loop {
            if let Some((r, c)) = self.decoder.take_cursor() {
                return Ok(MenuEvent::Resize(r, c));
            }
            let n = term.read_available(&mut self.buf)?;
            if n > 0 {
                let keys = self.decoder.feed(&self.buf[..n]);
                if let Some(k) = keys.into_iter().next() {
                    return Ok(MenuEvent::Key(k));
                }
                empties = 0;
            } else {
                empties += 1;
                if empties >= 2 {
                    if let Some(k) = self.decoder.idle() {
                        return Ok(MenuEvent::Key(k));
                    }
                }
                if empties >= empty_limit {
                    return Ok(MenuEvent::Idle);
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
    }

    pub fn take_cursor(&mut self) -> Option<(u16, u16)> {
        self.decoder.take_cursor()
    }

    /// Take the most recent audio drain report, if the terminal sent one.
    pub fn take_audio_drain(&mut self) -> Option<u8> {
        self.decoder.take_audio_drain()
    }

    /// The keyboard protocol resolved from the startup capability probes.
    pub fn keyboard_mode(&self) -> KeyboardMode {
        self.decoder.keyboard_mode()
    }

    /// True once detection is complete (Primary DA barrier seen): stop folding
    /// the capability queries into subsequent size probes.
    pub fn caps_resolved(&self) -> bool {
        self.decoder.caps_resolved()
    }

    /// Color depth resolved from the DECRQSS probe reply, if the terminal answered
    /// (sticky; None means it stayed silent -> caller keeps its truecolor default).
    pub fn color_probe(&self) -> Option<ColorDepth> {
        self.decoder.color_probe()
    }

    pub fn sixel_supported(&self) -> bool {
        self.decoder.sixel_supported()
    }

    pub fn text_area_pixels(&self) -> Option<(u16, u16)> {
        self.decoder.text_area_pixels()
    }

    pub fn gfx_geometry(&self) -> Option<(u16, u16)> {
        self.decoder.gfx_geometry()
    }

    /// Best-available pixel-area truth for the given character grid:
    /// `CSI 14 t` when the terminal speaks it, else the XTSMGRAPHICS sixel
    /// geometry IF it is plausibly the screen (4..=32 px/col, 6..=64 px/row)
    /// rather than xterm's "maximum allowed graphic" ceiling.
    pub fn pixel_area(&self, cols: u16, rows: u16) -> Option<(u16, u16)> {
        self.decoder.text_area_pixels().or_else(|| {
            self.decoder.gfx_geometry().filter(|&(h, w)| {
                let (cols, rows) = (cols.max(1) as u32, rows.max(1) as u32);
                let (pw, ph) = (w as u32 / cols, h as u32 / rows);
                (4..=32).contains(&pw) && (6..=64).contains(&ph)
            })
        })
    }

    pub fn cell_pixels(&self) -> Option<(u16, u16)> {
        self.decoder.cell_pixels()
    }

    /// Drain physical key edges decoded since the last call (edge modes).
    pub fn take_key_edges(&mut self) -> Vec<KeyEdge> {
        self.decoder.take_key_edges()
    }

    /// Enable/disable kitty event decoding (set when the door pushes the kitty
    /// keyboard flags).
    pub fn set_kitty_active(&mut self, on: bool) {
        self.decoder.set_kitty_active(on);
    }
}

/// What `Input::wait_event` returned: a keypress, the terminal's (rows, cols)
/// from a size-probe reply, or an idle tick (~1s of no input).
pub enum MenuEvent {
    Key(Key),
    Resize(u16, u16),
    Idle,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<Key> {
        KeyDecoder::new().feed(bytes)
    }

    #[test]
    fn cursor_position_report_is_captured_not_keyed() {
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1b[24;80R"), []); // no key emitted
        assert_eq!(d.take_cursor(), Some((24, 80)));
        assert_eq!(d.take_cursor(), None);
    }

    #[test]
    fn page_up_down_decode_from_both_terminal_families() {
        let mut d = KeyDecoder::new();
        // SyncTERM / CTerm: PPAGE = CSI V, NPAGE = CSI U (the ones that were
        // silently dropped before). This is what a Mac SyncTERM actually sends.
        assert_eq!(d.feed(b"\x1b[V"), [Key::PageUp]);
        assert_eq!(d.feed(b"\x1b[U"), [Key::PageDown]);
        // xterm-family (iTerm via the shim, fTelnet, PuTTY): CSI 5~ / 6~.
        assert_eq!(d.feed(b"\x1b[5~"), [Key::PageUp]);
        assert_eq!(d.feed(b"\x1b[6~"), [Key::PageDown]);
        // Modifier forms keep the same first field (e.g. Shift+PageUp).
        assert_eq!(d.feed(b"\x1b[5;2~"), [Key::PageUp]);
        // Other edit-keypad keys (Home/Ins/Del/End = 1/2/3/4~) stay ignored.
        assert_eq!(d.feed(b"\x1b[1~\x1b[3~"), []);
    }

    #[test]
    fn cursor_report_survives_interleaved_capability_replies() {
        // A real SyncTERM answers the folded probe burst as one chunk: CTDA
        // (CSI<...c), the CPR (CSI row;colR), then Primary DA (CSI?...c). The CPR
        // must still be captured and no probe reply may leak as a keystroke.
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1b[<0;8c\x1b[24;80R\x1b[?62c"), []); // nothing keyed
        assert_eq!(d.take_cursor(), Some((24, 80)), "CPR captured amid caps");
        assert_eq!(d.take_cursor(), None);
        // The capability replies in the same burst still resolved the protocol.
        assert_eq!(d.keyboard_mode(), KeyboardMode::CtermPhysical);
        assert!(d.caps_resolved(), "DA1 barrier seen");
        // And no stray key edges were produced by the c/R/c sequence.
        assert!(d.take_key_edges().is_empty());

        // Order-independence: CPR last, after both caps replies.
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1b[<0;8c\x1b[?62;1;6c\x1b[24;80R"), []);
        assert_eq!(d.take_cursor(), Some((24, 80)));
    }

    #[test]
    fn cterm_single_key_press_then_release_decodes_to_evdev_44_edges() {
        // A terminal sending proper CTerm physical key reports for one key (Z):
        // press `CSI = 44 K`, then release `CSI = 44 k`. Each drains as exactly
        // one edge with the right evdev code and direction, and no Key output.
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1b[=44K"), []); // press, no translated key
        assert_eq!(d.take_key_edges(), vec![KeyEdge { code: 44, pressed: true }]);
        assert_eq!(d.feed(b"\x1b[=44k"), []); // release, no translated key
        assert_eq!(d.take_key_edges(), vec![KeyEdge { code: 44, pressed: false }]);
    }

    #[test]
    fn physical_key_reports_decode_to_edges_not_keys() {
        let mut d = KeyDecoder::new();
        // Two keys pressed together, then one released — no Key output.
        assert_eq!(d.feed(b"\x1b[=44;45K"), []);
        assert_eq!(d.feed(b"\x1b[=44k"), []);
        let edges = d.take_key_edges();
        assert_eq!(edges.len(), 3);
        assert_eq!(edges[0], KeyEdge { code: 44, pressed: true });
        assert_eq!(edges[1], KeyEdge { code: 45, pressed: true });
        assert_eq!(edges[2], KeyEdge { code: 44, pressed: false });
        assert!(d.take_key_edges().is_empty(), "edges drained");
        // An audio drain report (also `=`-prefixed) is not a key edge.
        d.feed(b"\x1b[=7;2;0n");
        assert!(d.take_key_edges().is_empty());
        assert_eq!(d.take_audio_drain(), Some(2));
    }

    #[test]
    fn iterm_kitty_session_flow_resolves_and_decodes() {
        // Mock a realistic iTerm-over-the-wire session: the probe replies it
        // sends back, then (after the door enables kitty) movement+jump events.
        let mut d = KeyDecoder::new();
        // iTerm ignores the CTerm CTDA query, answers the kitty query, then DA1.
        d.feed(b"\x1b[?0u");           // kitty supported
        d.feed(b"\x1b[?62;4;6;22c");   // Primary DA (barrier); note the 22 contains no bare "8"
        assert_eq!(d.keyboard_mode(), KeyboardMode::Kitty, "iTerm should resolve to Kitty");
        assert!(d.caps_resolved());
        // Door enables kitty. Letters arrive as CSI-u -> edges, and arrows are
        // FULL edges including the bare initial press (iTerm2 sends plain
        // `CSI C` for an unmodified press even in kitty mode; kitty guarantees
        // the matching release, so a press-only edge cannot stick).
        d.set_kitty_active(true);
        assert_eq!(d.feed(b"\x1b[122u"), []);       // z press -> edge
        assert_eq!(d.feed(b"\x1b[122;1:3u"), []);   // z release -> edge
        assert_eq!(d.feed(b"\x1b[C"), []);          // bare arrow press -> edge
        assert_eq!(d.feed(b"\x1b[1;1:3C"), []);     // kitty arrow release -> edge
        assert_eq!(d.take_key_edges(), vec![
            KeyEdge { code: 44, pressed: true },    // z down (A)
            KeyEdge { code: 44, pressed: false },   // z up
            KeyEdge { code: 106, pressed: true },   // right down
            KeyEdge { code: 106, pressed: false },  // right up
        ]);
    }

    #[test]
    fn kitty_events_decode_to_evdev_edges_when_active() {
        let mut d = KeyDecoder::new();
        // A kitty-form arrow before activation proves kitty upstream (a proxy
        // switched the terminal): it is an edge, not a translated key.
        assert_eq!(d.feed(b"\x1b[1;1:1A"), []);
        assert_eq!(
            d.take_key_edges(),
            vec![KeyEdge { code: 103, pressed: true }]
        );
        d.set_kitty_active(true);
        // z press, z release, Up press, Up release, Space press.
        assert_eq!(d.feed(b"\x1b[122;1:1u"), []);
        assert_eq!(d.feed(b"\x1b[122;1:3u"), []);
        assert_eq!(d.feed(b"\x1b[1;1:1A"), []);
        assert_eq!(d.feed(b"\x1b[1;1:3A"), []);
        assert_eq!(d.feed(b"\x1b[32u"), []); // no event field -> press
        let e = d.take_key_edges();
        assert_eq!(e, vec![
            KeyEdge { code: 44, pressed: true },   // z down
            KeyEdge { code: 44, pressed: false },  // z up
            KeyEdge { code: 103, pressed: true },  // up down
            KeyEdge { code: 103, pressed: false }, // up up
            KeyEdge { code: 57, pressed: true },   // space down
        ]);
    }

    /// Every in-game key must be in the kitty map: an unmapped key is
    /// silently dropped in kitty mode (no translated fallback). This is the
    /// regression that made SNES Y/X and the shoulders dead in iTerm2-class
    /// terminals: the map was lameboy's key set, not this door's.
    #[test]
    fn kitty_map_covers_the_whole_pad() {
        let mut d = KeyDecoder::new();
        d.set_kitty_active(true);
        // a=Y, s=X, c=L, v=R (SNES labels), plus 5/8 save states.
        for (cp, code) in
            [(97u32, 30u16), (115, 31), (99, 46), (118, 47), (53, 6), (56, 9)]
        {
            assert_eq!(d.feed(format!("\x1b[{cp};1:1u").as_bytes()), []);
            assert_eq!(
                d.take_key_edges(),
                vec![KeyEdge { code, pressed: true }],
                "kitty codepoint {cp} must map to evdev {code}"
            );
        }
        // Uppercase forms too (kitty may report shifted keysyms).
        assert_eq!(d.feed(b"\x1b[83;1:1u"), []);
        assert_eq!(d.take_key_edges(), vec![KeyEdge { code: 31, pressed: true }]);
        // And every button evdev_to_button knows must be reachable from some
        // kitty codepoint, so the two tables can't drift apart again.
        for code in [44u16, 45, 30, 31, 46, 47, 57, 28, 103, 105, 106, 108] {
            assert!(
                crate::input::evdev_to_button(code).is_some(),
                "evdev {code} lost its button mapping"
            );
        }
    }

    #[test]
    fn kitty_form_arrows_without_kitty_mode_go_full_edge_once_proven() {
        // iTerm2 behind BBSproxy (real capture, 2026-07-04): the proxy switches
        // the terminal into kitty mode but the door is in the CTerm fork, so
        // kitty-form arrows arrive with kitty_active = false. The very first
        // bare press has no kitty evidence yet and stays translated; the first
        // colon-form event proves releases are coming, and from then on arrows
        // are FULL edges — including bare presses. Critically, a release is a
        // release edge, never a phantom press, and a held direction survives a
        // chord (macOS stops auto-repeat when another key is pressed — edges
        // don't care).
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1b[C"), [Key::Right]); // first-ever press: translated
        assert_eq!(d.feed(b"\x1b[1;1:2C"), []); // repeat: proves kitty -> edge
        assert_eq!(d.feed(b"\x1b[1;1:2C"), []);
        assert_eq!(d.feed(b"\x1b[1;1:3C"), []); // release: edge, no phantom press
        assert_eq!(d.feed(b"\x1b[C"), []); // next hold: bare press is an edge now
        assert_eq!(d.feed(b"\x1b[1;1:3C"), []);
        assert_eq!(
            d.take_key_edges(),
            vec![
                KeyEdge { code: 106, pressed: true },  // repeat -> down (idempotent)
                KeyEdge { code: 106, pressed: true },
                KeyEdge { code: 106, pressed: false }, // release -> up, immediately
                KeyEdge { code: 106, pressed: true },  // second hold: instant down
                KeyEdge { code: 106, pressed: false },
            ]
        );
        // Session teardown restores translated arrows for the menu.
        d.set_kitty_active(false);
        assert_eq!(d.feed(b"\x1b[C"), [Key::Right]);
    }

    #[test]
    fn caps_probe_replies_resolve_keyboard_mode_not_keyed() {
        // SyncTERM: CTDA advertises 8, then the Primary DA barrier.
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1b[<0;5;6;7;8c"), []);
        assert_eq!(d.keyboard_mode(), KeyboardMode::CtermPhysical);
        assert!(!d.caps_resolved(), "not resolved until the DA1 barrier");
        assert_eq!(d.feed(b"\x1b[?62;1;6c"), []);
        assert!(d.caps_resolved());

        // Kitty terminal: query reply, then DA1; no CTDA-8.
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1b[?0u\x1b[?62c"), []);
        assert_eq!(d.keyboard_mode(), KeyboardMode::Kitty);
        assert!(d.caps_resolved());

        // Dumb terminal: only Primary DA -> legacy fallback.
        let mut d = KeyDecoder::new();
        d.feed(b"\x1b[?62;1;6c");
        assert_eq!(d.keyboard_mode(), KeyboardMode::Legacy);
        assert!(d.caps_resolved());

        // CTDA present but without 8 -> not physical.
        let mut d = KeyDecoder::new();
        d.feed(b"\x1b[<0;5;6;7c\x1b[?62c");
        assert_eq!(d.keyboard_mode(), KeyboardMode::Legacy);
    }

    #[test]
    fn sixel_and_cell_geometry_are_detected_without_key_leaks() {
        let mut d = KeyDecoder::new();
        // SyncTERM: CTDA 4 means pixel operations (SIXEL/PPM); CTSMRR 3
        // reports the current font's height and width in pixels.
        assert_eq!(d.feed(b"\x1b[<0;4;8c\x1b[=3;16;13n"), []);
        assert!(d.sixel_supported());
        assert_eq!(d.cell_pixels(), Some((16, 13)));
        assert!(d.take_key_edges().is_empty());

        // xterm-style Primary DA 4 and window-op cell-size report are accepted.
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1b[?62;4;6c\x1b[6;18;9t"), []);
        assert!(d.sixel_supported());
        assert_eq!(d.cell_pixels(), Some((18, 9)));

        // A terminal that does not advertise feature 4 stays on ANSI fallback.
        let mut d = KeyDecoder::new();
        d.feed(b"\x1b[<0;6;8c\x1b[?62;1;6c");
        assert!(!d.sixel_supported());
    }

    #[test]
    fn decrqss_color_reply_resolves_depth_without_keying() {
        // Terminal quantized our 24-bit probe to 256: ESC P 1 $ r 0;38;5;16m ESC \
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1bP1$r0;38;5;16m\x1b\\"), []); // no keys
        assert_eq!(d.color_probe(), Some(ColorDepth::C256));
        assert!(d.take_key_edges().is_empty());

        // Terminal preserved 24-bit -> truecolor confirmed (colon subparams too).
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1bP1$r0;38:2::1:2:3m\x1b\\"), []);
        assert_eq!(d.color_probe(), Some(ColorDepth::True));

        // Silent / non-DECRQSS -> never resolves (caller stays at its default).
        let mut d = KeyDecoder::new();
        assert_eq!(d.color_probe(), None);
        d.feed(b"\x1b[24;80R"); // a stray CPR is not a color reply
        assert_eq!(d.color_probe(), None);
    }

    #[test]
    fn dcs_interleaved_with_probe_replies_keeps_them_all() {
        // A realistic burst: CTDA, the color DECRQSS reply, CPR, then Primary DA.
        // Every capability must resolve and nothing may leak as a keystroke.
        let mut d = KeyDecoder::new();
        assert_eq!(
            d.feed(b"\x1b[<0;8c\x1bP1$r0;38;5;9m\x1b\\\x1b[24;80R\x1b[?62c"),
            []
        );
        assert_eq!(d.color_probe(), Some(ColorDepth::C256));
        assert_eq!(d.take_cursor(), Some((24, 80)));
        assert_eq!(d.keyboard_mode(), KeyboardMode::CtermPhysical);
        assert!(d.caps_resolved());
    }

    #[test]
    fn malformed_dcs_without_terminator_is_dropped_on_idle() {
        // A bare `ESC P` with no ST must not wedge the decoder; idle clears it and
        // a subsequent key decodes normally.
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1bPsome garbage"), []);
        assert_eq!(d.idle(), None); // dangling DCS dropped
        assert_eq!(d.feed(b"z"), [Key::Char('z')]);
    }

    #[test]
    fn audio_drain_report_is_captured_not_keyed() {
        let mut d = KeyDecoder::new();
        // Update one-shot for channel 2 going idle: ESC [ = 7 ; 2 ; 0 n
        assert_eq!(d.feed(b"\x1b[=7;2;0n"), []); // no key emitted
        assert_eq!(d.take_audio_drain(), Some(2));
        assert_eq!(d.take_audio_drain(), None);
        // A "running" report (state 1) is not a drain.
        assert_eq!(d.feed(b"\x1b[=7;2;1n"), []);
        assert_eq!(d.take_audio_drain(), None);
    }

    #[test]
    fn arrows_csi_and_ss3() {
        assert_eq!(keys(b"\x1b[A\x1b[B\x1b[C\x1b[D"), [Key::Up, Key::Down, Key::Right, Key::Left]);
        assert_eq!(keys(b"\x1bOA\x1bOD"), [Key::Up, Key::Left]);
        assert_eq!(keys(b"\x1b[Z"), [Key::BackTab]);
    }

    #[test]
    fn lone_esc_resolves_on_idle() {
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1b"), []);
        assert_eq!(d.idle(), Some(Key::Esc));
        assert_eq!(d.idle(), None);
    }

    #[test]
    fn esc_then_char_emits_both() {
        assert_eq!(keys(b"\x1bx"), [Key::Esc, Key::Char('x')]);
    }

    #[test]
    fn enter_handles_cr_crlf_and_crnul() {
        assert_eq!(keys(b"\r"), [Key::Enter]);
        assert_eq!(keys(b"\r\n"), [Key::Enter]);
        assert_eq!(keys(b"\r\x00"), [Key::Enter]);
        assert_eq!(keys(b"\n"), [Key::Enter]);
        assert_eq!(keys(b"a\rb"), [Key::Char('a'), Key::Enter, Key::Char('b')]);
    }

    #[test]
    fn letters_space_tab_backspace() {
        assert_eq!(
            keys(b"hi z\t\x7f\x08"),
            [Key::Char('h'), Key::Char('i'), Key::Char(' '), Key::Char('z'),
             Key::Tab, Key::Backspace, Key::Backspace]
        );
    }

    #[test]
    fn telnet_iac_is_stripped() {
        // IAC WILL ECHO, then 'A'
        assert_eq!(keys(b"\xff\xfb\x01A"), [Key::Char('A')]);
        // IAC SB NAWS ... IAC SE, then 'Z'
        assert_eq!(keys(b"\xff\xfa\x1f\x00\x50\x00\x18\xff\xf0Z"), [Key::Char('Z')]);
    }

    #[test]
    fn sequences_split_across_feeds() {
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1b["), []);
        assert_eq!(d.feed(b"A"), [Key::Up]);
        // ESC split from its '[' across feeds, still becomes an arrow
        assert_eq!(d.feed(b"\x1b"), []);
        assert_eq!(d.feed(b"[D"), [Key::Left]);
    }
}
