//! Output color-depth model and RGB quantization.
//!
//! The door renders from a full-RGB framebuffer but the caller's terminal may not
//! do 24-bit color. `ColorDepth` picks how we encode each cell:
//!
//!   - **Truecolor**: 24-bit `38;2;r;g;b` — the menu's native output.
//!   - **256**: the xterm 6x6x6 cube / gray ramp (`38;5;N`); half the bytes of
//!     truecolor and the game's default tier.
//!   - **16**: the classic ANSI palette (`3N`/`4N`) with ordered dithering, for
//!     terminals with no extended color at all.
//!
//! The quantizers are ported from the spectre door's `color.go`. One divergence:
//! spectre's palette table is in CGA/VGA-attribute order (blue=1, red=4) yet it
//! emits the index straight as an SGR number, which swaps red/blue and cyan/yellow.
//! Here the palette is in **ANSI SGR order** so `index == 3<index>m` is correct.

use std::fmt;

/// Resolved output color depth (what the renderer actually emits).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorDepth {
    True,
    C256,
    C16,
}

/// The user-facing color setting: an explicit depth, or `Auto` (auto-probe with a
/// truecolor default, downgraded to 256 only on explicit evidence — see
/// `resolve`). 16 is never auto-selected; the DECRQSS probe can't detect it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorSetting {
    Auto,
    True,
    C256,
    C16,
}

impl ColorSetting {
    /// ini/config spelling.
    pub fn slug(self) -> &'static str {
        match self {
            ColorSetting::Auto => "auto",
            ColorSetting::True => "truecolor",
            ColorSetting::C256 => "256",
            ColorSetting::C16 => "16",
        }
    }

    /// Short label for the settings menu.
    pub fn label(self) -> &'static str {
        match self {
            ColorSetting::Auto => "Auto",
            ColorSetting::True => "Truecolor",
            ColorSetting::C256 => "256",
            ColorSetting::C16 => "16",
        }
    }

    pub fn parse(s: &str) -> Option<ColorSetting> {
        match s.trim().to_lowercase().as_str() {
            "auto" | "detect" => Some(ColorSetting::Auto),
            "truecolor" | "true" | "rgb" | "24bit" => Some(ColorSetting::True),
            "256" | "256color" | "xterm" => Some(ColorSetting::C256),
            "16" | "16color" | "classic" | "ansi" => Some(ColorSetting::C16),
            _ => None,
        }
    }

    /// Cycle order for the Left/Right menu control.
    pub fn next(self) -> ColorSetting {
        match self {
            ColorSetting::Auto => ColorSetting::True,
            ColorSetting::True => ColorSetting::C256,
            ColorSetting::C256 => ColorSetting::C16,
            ColorSetting::C16 => ColorSetting::Auto,
        }
    }

    pub fn prev(self) -> ColorSetting {
        match self {
            ColorSetting::Auto => ColorSetting::C16,
            ColorSetting::True => ColorSetting::Auto,
            ColorSetting::C256 => ColorSetting::True,
            ColorSetting::C16 => ColorSetting::C256,
        }
    }

    /// Resolve to a concrete depth. `probe` is the auto-detection result, if any:
    /// `Some(C256)` = the terminal quantized our 24-bit probe (downgrade),
    /// `Some(True)` = it kept 24-bit, `None` = silent (stay truecolor).
    pub fn resolve(self, probe: Option<ColorDepth>) -> ColorDepth {
        match self {
            ColorSetting::Auto => probe.unwrap_or(ColorDepth::True),
            ColorSetting::True => ColorDepth::True,
            ColorSetting::C256 => ColorDepth::C256,
            ColorSetting::C16 => ColorDepth::C16,
        }
    }
}

impl fmt::Display for ColorSetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

/// The classic 16-color ANSI palette in **SGR index order** (index N renders as
/// `ESC[3Nm`): the standard VGA text-mode colors. 0-7 normal, 8-15 bright.
pub const ANSI16: [[u8; 3]; 16] = [
    [0, 0, 0],       // 0 black
    [170, 0, 0],     // 1 red
    [0, 170, 0],     // 2 green
    [170, 85, 0],    // 3 yellow/brown
    [0, 0, 170],     // 4 blue
    [170, 0, 170],   // 5 magenta
    [0, 170, 170],   // 6 cyan
    [170, 170, 170], // 7 light gray (white)
    [85, 85, 85],    // 8 dark gray (bright black)
    [255, 85, 85],   // 9 bright red
    [85, 255, 85],   // 10 bright green
    [255, 255, 85],  // 11 bright yellow
    [85, 85, 255],   // 12 bright blue
    [255, 85, 255],  // 13 bright magenta
    [85, 255, 255],  // 14 bright cyan
    [255, 255, 255], // 15 white
];

/// Luma-weighted squared RGB distance (green counts most).
#[inline]
fn cdist(r: i32, g: i32, b: i32, p: [u8; 3]) -> i32 {
    let dr = r - p[0] as i32;
    let dg = g - p[1] as i32;
    let db = b - p[2] as i32;
    2 * dr * dr + 4 * dg * dg + 3 * db * db
}

/// Classic 4x4 ordered-dither threshold matrix.
const BAYER4: [[i32; 4]; 4] = [
    [0, 8, 2, 10],
    [12, 4, 14, 6],
    [3, 11, 1, 9],
    [15, 7, 13, 5],
];

#[inline]
fn clamp_u8(v: i32) -> i32 {
    v.clamp(0, 255)
}

/// Map RGB to the xterm 256-color palette: the 6x6x6 cube or the 24-step gray
/// ramp, whichever lands closer.
pub fn xterm256(r: u8, g: u8, b: u8) -> u8 {
    #[inline]
    fn ci(v: u8) -> i32 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            (v as i32 - 35) / 40
        }
    }
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let (cr, cg, cb) = (ci(r), ci(g), ci(b));
    let cube_d = cdist(
        r as i32,
        g as i32,
        b as i32,
        [STEPS[cr as usize], STEPS[cg as usize], STEPS[cb as usize]],
    );
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let gi = ((avg - 3) / 10).clamp(0, 23);
    let gv = (8 + 10 * gi) as u8;
    if cdist(r as i32, g as i32, b as i32, [gv, gv, gv]) < cube_d {
        return (232 + gi) as u8;
    }
    (16 + 36 * cr + 6 * cg + cb) as u8
}

/// Map RGB to the 16-color ANSI palette with a position-fixed ordered-dither
/// bias, so flat gradients blend between the two nearest palette colors instead
/// of banding. The bias depends only on screen position: a static scene still
/// deltas to nothing frame-over-frame.
pub fn nearest16(r: u8, g: u8, b: u8, x: usize, y: usize) -> u8 {
    // Amplitude +/-30 (spectre uses +/-45). The Game Boy's four shades sit at
    // 0/85/170/255 — exactly on ANSI black/dark-gray/light-gray/white, each 42.5
    // from the boundary with its neighbor. Keeping the bias under 42.5 means those
    // flats never speckle (clean B&W), while continuous GBC color still blends.
    let d = (BAYER4[y & 3][x & 3] * 2 - 15) * 2;
    let rr = clamp_u8(r as i32 + d);
    let gg = clamp_u8(g as i32 + d);
    let bb = clamp_u8(b as i32 + d);
    let mut best = 0usize;
    let mut best_d = i32::MAX;
    for (i, p) in ANSI16.iter().enumerate() {
        let dd = cdist(rr, gg, bb, *p);
        if dd < best_d {
            best = i;
            best_d = dd;
        }
    }
    best as u8
}

/// How much bright color `8+i` loses when snapped to its dark twin `i` (used when
/// both halves of a cell want a bright background).
#[inline]
fn twin_penalty(i: u8) -> i32 {
    let c = ANSI16[(8 + i) as usize];
    cdist(c[0] as i32, c[1] as i32, c[2] as i32, ANSI16[i as usize])
}

/// CP437 half-block glyphs used by `pack_cell16`.
pub const FULL_BLOCK: u8 = 0xDB; // █
pub const UPPER_HALF: u8 = 0xDF; // ▀
pub const LOWER_HALF: u8 = 0xDC; // ▄
pub const SPACE: u8 = 0x20;

/// Resolve a half-block cell (top/bottom palette indexes) into classic-ANSI
/// fg/bg/glyph. Classic backgrounds only span the 8 dark colors (no iCE-color
/// assumption), so a bright bottom half flips the glyph to the lower-half block —
/// the bright color rides in the foreground; if both halves are bright, the one
/// that loses least snaps to its dark twin.
pub fn pack_cell16(mut t16: u8, mut b16: u8) -> (u8, u8, u8) {
    if t16 == b16 {
        if t16 < 8 {
            return (7, t16, SPACE); // solid dark cell: a space on that background
        }
        return (t16, 0, FULL_BLOCK); // solid bright cell: full block
    }
    if t16 >= 8 && b16 >= 8 {
        if twin_penalty(b16 - 8) <= twin_penalty(t16 - 8) {
            b16 -= 8;
        } else {
            t16 -= 8;
        }
        if t16 == b16 {
            // twins collided (e.g. yellow+white both near gray)
            return (7, t16, SPACE);
        }
    }
    if b16 >= 8 {
        return (b16, t16, LOWER_HALF); // fg paints the bottom
    }
    (t16, b16, UPPER_HALF) // fg paints the top
}

/// Build the classic SGR params (no `ESC[` / `m`) for a `pack_cell16` result. bg
/// is always one of the 8 dark colors there, so plain `4N` suffices; a bright fg
/// uses the bold attribute.
pub fn sgr16(fg: u8, bg: u8) -> String {
    if fg < 8 {
        format!("0;3{};4{}", fg, bg)
    } else {
        format!("1;3{};4{}", fg - 8, bg)
    }
}

/// Full SGR params (no `ESC[`/`m`) painting a foreground RGB over a background
/// RGB at the given depth — used to render a CP437 cell (the splash `.bin`). The
/// 16-color form masks the background to the 8 normal colors (bright bg is
/// unreliable on classic terminals) and uses the bold bit for bright foregrounds.
pub fn cell_sgr(depth: ColorDepth, fr: u8, fg: u8, fb: u8, br: u8, bg: u8, bb: u8) -> String {
    match depth {
        ColorDepth::True => format!("38;2;{};{};{};48;2;{};{};{}", fr, fg, fb, br, bg, bb),
        ColorDepth::C256 => format!("38;5;{};48;5;{}", xterm256(fr, fg, fb), xterm256(br, bg, bb)),
        // Exact-palette VGA art maps back to its ANSI index with no dither needed.
        ColorDepth::C16 => sgr16(nearest16(fr, fg, fb, 0, 0), nearest16(br, bg, bb, 0, 0) & 7),
    }
}

/// SGR params for a single foreground RGB in the given depth (no `ESC[`/`m`).
/// Used by overlays (FPS counter) that draw on the default background. The
/// 16-color form resets attributes to manage bold, so callers must be on a
/// default background.
pub fn fg_sgr(depth: ColorDepth, r: u8, g: u8, b: u8) -> String {
    match depth {
        ColorDepth::True => format!("38;2;{};{};{}", r, g, b),
        ColorDepth::C256 => format!("38;5;{}", xterm256(r, g, b)),
        ColorDepth::C16 => {
            // aixterm codes: 30-37 normal, 90-97 bright. Explicit bright (not the
            // bold `1;3N` form) so a terminal that doesn't brighten bold still
            // shows bright white/etc. as bright, not dim.
            let i = nearest16(r, g, b, 0, 0);
            if i < 8 {
                format!("3{}", i)
            } else {
                format!("9{}", i - 8)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_parse_roundtrip() {
        for s in [
            ColorSetting::Auto,
            ColorSetting::True,
            ColorSetting::C256,
            ColorSetting::C16,
        ] {
            assert_eq!(ColorSetting::parse(s.slug()), Some(s));
        }
        assert_eq!(ColorSetting::parse("RGB"), Some(ColorSetting::True));
        assert_eq!(ColorSetting::parse("classic"), Some(ColorSetting::C16));
        assert_eq!(ColorSetting::parse("nope"), None);
    }

    #[test]
    fn auto_resolves_conservatively() {
        // Silent terminal stays truecolor; explicit 256 downgrades; explicit
        // pins are honored verbatim.
        assert_eq!(ColorSetting::Auto.resolve(None), ColorDepth::True);
        assert_eq!(ColorSetting::Auto.resolve(Some(ColorDepth::C256)), ColorDepth::C256);
        assert_eq!(ColorSetting::Auto.resolve(Some(ColorDepth::True)), ColorDepth::True);
        assert_eq!(ColorSetting::C16.resolve(Some(ColorDepth::True)), ColorDepth::C16);
    }

    #[test]
    fn xterm256_known_points() {
        assert_eq!(xterm256(0, 0, 0), 16); // cube black
        assert_eq!(xterm256(255, 255, 255), 231); // cube white
        // Pure bright green lands in the cube's green corner.
        assert_eq!(xterm256(0, 255, 0), 46);
    }

    #[test]
    fn nearest16_primaries_map_to_ansi_indexes() {
        // The index IS the SGR number, so this also proves ANSI (not CGA) order:
        // red must be 1 (CGA would give 4) and blue 4 (CGA would give 1). At (0,0)
        // the dither bias is negative, so full primaries land on the dark variants.
        assert_eq!(nearest16(0, 255, 0, 0, 0), 2); // green
        assert_eq!(nearest16(255, 0, 0, 0, 0), 1); // red — ANSI 1, not CGA 4
        assert_eq!(nearest16(0, 0, 255, 0, 0), 4); // blue — ANSI 4, not CGA 1
        assert_eq!(nearest16(0, 0, 0, 0, 0), 0);
        // White at a positive-dither cell resolves to bright white (15).
        assert_eq!(nearest16(255, 255, 255, 1, 0), 15);
    }

    #[test]
    fn dmg_grays_map_cleanly_without_dither_noise() {
        // The four DMG shades must land on solid ANSI grays at EVERY screen
        // position (no dither speckle), so B&W games look clean in 16-color.
        let grays = [
            ((0u8, 0u8, 0u8), 0u8),
            ((85, 85, 85), 8),
            ((170, 170, 170), 7),
            ((255, 255, 255), 15),
        ];
        for ((r, g, b), want) in grays {
            for y in 0..4 {
                for x in 0..4 {
                    assert_eq!(
                        nearest16(r, g, b, x, y),
                        want,
                        "gray {:?} speckled at ({},{})",
                        (r, g, b),
                        x,
                        y
                    );
                }
            }
        }
    }

    #[test]
    fn pack_cell16_solid_and_split() {
        // Solid dark: space on that bg. Solid bright: full block.
        assert_eq!(pack_cell16(0, 0), (7, 0, SPACE));
        assert_eq!(pack_cell16(15, 15), (15, 0, FULL_BLOCK));
        // Dark top over dark bottom: upper-half, fg=top bg=bottom.
        assert_eq!(pack_cell16(2, 4), (2, 4, UPPER_HALF));
        // Bright bottom over dark top: lower-half, fg carries the bright bottom.
        assert_eq!(pack_cell16(0, 10), (10, 0, LOWER_HALF));
    }

    #[test]
    fn sgr16_bold_bit_for_bright() {
        assert_eq!(sgr16(2, 0), "0;32;40");
        assert_eq!(sgr16(10, 0), "1;32;40");
    }

    #[test]
    fn cell_sgr_per_depth() {
        // The splash surround is magenta (VGA 5 = 170,0,170).
        assert_eq!(
            cell_sgr(ColorDepth::True, 170, 0, 170, 170, 0, 170),
            "38;2;170;0;170;48;2;170;0;170"
        );
        assert!(cell_sgr(ColorDepth::C256, 170, 0, 170, 170, 0, 170).starts_with("38;5;"));
        // 16-color: magenta -> ANSI 5, so fg 35 on bg 45 (both normal).
        assert_eq!(cell_sgr(ColorDepth::C16, 170, 0, 170, 170, 0, 170), "0;35;45");
    }
}
