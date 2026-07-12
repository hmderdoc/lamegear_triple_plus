//! Full-screen startup splash from an 80x32 CP437 `.bin` graphic
//! (`lamegear_splash.bin`, char/attr pairs + SAUCE trailer, which we ignore).
//!
//! Shown once at door start, dismissed by any key or a 10s timeout. Placement:
//!   - Terminal >= the graphic: centered, and every surrounding cell is filled
//!     BLUE (the graphic's own border color) so it blends with no black frame.
//!   - Terminal shorter than the graphic (the common 24/25-row case): the top
//!     rows are truncated and the bottom rows are shown (they carry the wordmark).
//!
//! Bytes are written raw (already CP437); the whole session runs with autowrap
//! off (see main), so painting the bottom-right cell can't scroll the screen.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::color::{self, ColorDepth, ColorSetting};
use crate::keys::Input;
use crate::term::Term;

/// Graphic dimensions (the SAUCE says DataType 5, FileType 40 => 80 wide; the
/// 5120-byte body is 80x32 char/attr cells).
const GW: usize = 80;
const GH: usize = 32;
const BODY: usize = GW * GH * 2;

/// The 16-color VGA text palette in **attribute-index order** (what a `.bin`'s
/// attribute nibbles index): 0-7 normal, 8-15 bright. Note this differs from the
/// ANSI SGR order used elsewhere (here 1=blue, 4=red); `cell_sgr` re-maps it to
/// the caller's depth via the RGB values.
const VGA16: [[u8; 3]; 16] = [
    [0, 0, 0], [0, 0, 170], [0, 170, 0], [0, 170, 170],
    [170, 0, 0], [170, 0, 170], [170, 85, 0], [170, 170, 170],
    [85, 85, 85], [85, 85, 255], [85, 255, 85], [85, 255, 255],
    [255, 85, 85], [255, 85, 255], [255, 255, 85], [255, 255, 255],
];

/// The Sega blue the graphic's border uses (VGA 1), for the surrounding fill.
const BLUE: [u8; 3] = [0, 0, 170];

/// Load `lamegear_splash.bin` (working dir, then the binary's dir). Returns the
/// raw file bytes if it exists and is at least one full 80x32 body; None to skip
/// the splash entirely.
fn load_splash() -> Option<Vec<u8>> {
    let mut paths = vec![PathBuf::from("lamegear_splash.bin")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join("lamegear_splash.bin"));
        }
    }
    for p in paths {
        if let Ok(data) = std::fs::read(&p) {
            if data.len() >= BODY {
                return Some(data);
            }
        }
    }
    None
}

/// Map terminal row `r` (0-based) to a graphic row, or None if it's a
/// surround/fill row. Tall enough terminals center the graphic; shorter ones
/// bottom-align it (truncating the top).
fn graphic_row(r: usize, h: usize) -> Option<usize> {
    if h >= GH {
        let top = (h - GH) / 2;
        (r >= top && r < top + GH).then(|| r - top)
    } else {
        Some((GH - h) + r) // r in 0..h -> bottom h rows of the graphic
    }
}

/// Map terminal column `c` to a graphic column, or None for a fill column. Wider
/// terminals center the graphic; narrower ones show its middle columns.
fn graphic_col(c: usize, w: usize) -> Option<usize> {
    if w >= GW {
        let left = (w - GW) / 2;
        (c >= left && c < left + GW).then(|| c - left)
    } else {
        Some((GW - w) / 2 + c) // c in 0..w
    }
}

/// Paint the whole `w`x`h` screen: the graphic where it lands, blue elsewhere.
/// SGR is deduped run-to-run to keep the byte count down on a slow link.
fn render_splash(term: &mut dyn Term, data: &[u8], depth: ColorDepth, w: u16, h: u16) -> io::Result<()> {
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 {
        return Ok(());
    }
    let fill = color::cell_sgr(depth, BLUE[0], BLUE[1], BLUE[2], BLUE[0], BLUE[1], BLUE[2]);
    let mut buf: Vec<u8> = Vec::with_capacity(w * h * 4);
    let mut last = String::new();
    for r in 0..h {
        buf.extend_from_slice(format!("\x1b[{};1H", r + 1).as_bytes());
        let gr = graphic_row(r, h);
        for c in 0..w {
            let (ch, sgr) = match gr.and_then(|gr| graphic_col(c, w).map(|gc| (gr, gc))) {
                Some((gr, gc)) => {
                    let o = (gr * GW + gc) * 2;
                    let mut ch = data[o];
                    let attr = data[o + 1];
                    if ch == 0 {
                        ch = b' ';
                    }
                    let fg = VGA16[(attr & 0x0F) as usize];
                    let bg = VGA16[((attr >> 4) & 0x07) as usize];
                    (ch, color::cell_sgr(depth, fg[0], fg[1], fg[2], bg[0], bg[1], bg[2]))
                }
                None => (b' ', fill.clone()),
            };
            if sgr != last {
                buf.push(0x1b);
                buf.push(b'[');
                buf.extend_from_slice(sgr.as_bytes());
                buf.push(b'm');
                last = sgr;
            }
            buf.push(ch);
        }
    }
    buf.extend_from_slice(b"\x1b[0m");
    term.write_all(&buf)?;
    term.flush()
}

/// Show the startup splash. Probes the terminal for size/keyboard/color first (so
/// the splash is placed and colored right, and detection is done before the menu),
/// then waits for any key or 10 seconds, redrawing on a resize. Returns whether a
/// splash was actually shown (false = no `.bin`, caller falls back to its intro).
pub fn show_splash(term: &mut dyn Term, input: &mut Input, color: ColorSetting) -> io::Result<bool> {
    let Some(data) = load_splash() else {
        return Ok(false);
    };

    // One probe burst for size + keyboard caps + color; drain replies for a beat.
    let _ = crate::send_size_probe(term, true);
    let mut size = (80u16, 24u16);
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(15));
        let _ = input.poll(term)?;
        if let Some((r, c)) = input.take_cursor() {
            if c > 0 && r > 0 {
                size = (c, r);
            }
        }
    }
    let depth = color.resolve(input.color_probe());
    render_splash(term, &data, depth, size.0, size.1)?;

    // Dismiss on any key or after 10s; repaint if the terminal resizes meanwhile.
    let end = Instant::now() + Duration::from_secs(10);
    let mut probe_timer = Instant::now();
    while Instant::now() < end {
        if !input.poll(term)?.is_empty() {
            break; // any key dismisses
        }
        if let Some((r, c)) = input.take_cursor() {
            let ns = (c, r);
            if ns.0 > 0 && ns.1 > 0 && ns != size {
                size = ns;
                render_splash(term, &data, depth, size.0, size.1)?;
            }
        }
        if probe_timer.elapsed() >= Duration::from_millis(1000) {
            probe_timer = Instant::now();
            let _ = crate::send_size_probe(term, !input.caps_resolved());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_placement_centers_or_bottom_aligns() {
        // Tall screen: centered (48 rows -> 8 above, 8 below the 32-row graphic).
        assert_eq!(graphic_row(0, 48), None);
        assert_eq!(graphic_row(7, 48), None);
        assert_eq!(graphic_row(8, 48), Some(0));
        assert_eq!(graphic_row(39, 48), Some(31));
        assert_eq!(graphic_row(40, 48), None);
        // Exact fit.
        assert_eq!(graphic_row(0, 32), Some(0));
        assert_eq!(graphic_row(31, 32), Some(31));
        // Short screen (24 rows): top 8 rows dropped, bottom 24 shown.
        assert_eq!(graphic_row(0, 24), Some(8));
        assert_eq!(graphic_row(23, 24), Some(31));
    }

    #[test]
    fn horizontal_placement_centers_or_clips() {
        // Wide screen: centered (100 cols -> 10 pad each side of the 80-wide art).
        assert_eq!(graphic_col(9, 100), None);
        assert_eq!(graphic_col(10, 100), Some(0));
        assert_eq!(graphic_col(89, 100), Some(79));
        assert_eq!(graphic_col(90, 100), None);
        // Exact width.
        assert_eq!(graphic_col(0, 80), Some(0));
        assert_eq!(graphic_col(79, 80), Some(79));
    }
}
