//! Local menu artwork: ROM-name lookup, ANSI fallback, and SIXEL encoding.
//!
//! Nothing in this module accesses the network. Sysops populate `art/` with the
//! companion `tools/fetch_game_art.py` script. Art is keyed by the ROM's own
//! system: `.gg` looks in `art/gg`, `.sms` in `art/sms`, `.sg` in `art/sg`
//! (with `art/` itself as a flat fallback).

use image::{imageops::FilterType, ImageReader, RgbaImage};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::color::{self, ColorDepth};

const SIXEL_MAX_SIDE: u32 = 384;
const SIXEL_COLORS: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq)]
struct RenderKey {
    x: u16,
    y: u16,
    cols: u16,
    rows: u16,
    depth: ColorDepth,
    sixel: bool,
    cell_pixels: Option<(u16, u16)>, // (height, width)
    bg: [u8; 3],                     // letterbox / transparency backdrop
}

/// One-image decode/render cache. Arrowing to another game replaces it; redraws
/// of the same menu state reuse both the decoded pixels and encoded terminal data.
pub struct ArtRenderer {
    root: PathBuf,
    cached_rom: Option<PathBuf>,
    cached_image: Option<RgbaImage>,
    render_key: Option<RenderKey>,
    render_bytes: Vec<u8>,
}

impl ArtRenderer {
    pub fn new() -> Self {
        Self {
            root: default_art_root(),
            cached_rom: None,
            cached_image: None,
            render_key: None,
            render_bytes: Vec::new(),
        }
    }

    #[cfg(test)]
    fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            cached_rom: None,
            cached_image: None,
            render_key: None,
            render_bytes: Vec::new(),
        }
    }

    /// Render local art into an already-cleared preview viewport. Returns false
    /// when no matching/decodable image exists so the caller can draw a placeholder.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        out: &mut impl Write,
        rom: &Path,
        x: u16,
        y: u16,
        cols: u16,
        rows: u16,
        depth: ColorDepth,
        sixel: bool,
        cell_pixels: Option<(u16, u16)>,
        bg: [u8; 3],
    ) -> io::Result<bool> {
        self.load_rom(rom);
        let Some(image) = self.cached_image.as_ref() else {
            return Ok(false);
        };
        let key = RenderKey {
            x,
            y,
            cols,
            rows,
            depth,
            sixel,
            cell_pixels,
            bg,
        };
        if self.render_key != Some(key) {
            self.render_bytes = if sixel {
                render_sixel(image, x, y, cols, rows, cell_pixels, bg)
            } else {
                // Aspect-fit within the viewport; center horizontally but
                // TOP-align (matching the SIXEL path) so the art sits under
                // the banner instead of drifting down a tall pane. The
                // caller's fill (the panel color) is the letterbox below.
                let (iw, ih) = (image.width() as f64, image.height() as f64);
                let (cw, ch) = (cols as f64, rows as f64 * 2.0);
                let scale = (cw / iw).min(ch / ih);
                let fit_cols = ((iw * scale).round() as u16).clamp(1, cols);
                let fit_rows = (((ih * scale) / 2.0).round() as u16).clamp(1, rows);
                let ox = x + cols.saturating_sub(fit_cols) / 2;
                render_ansi(image, ox, y, fit_cols, fit_rows, depth, bg)
            };
            self.render_key = Some(key);
        }
        out.write_all(&self.render_bytes)?;
        Ok(true)
    }

    /// Render this ROM's art to fill the whole screen, aspect-preserved
    /// (letterboxed — NOT stretched), as CP437 half-blocks. `out` must be a
    /// CP437-translating writer (the `▀` glyphs become byte 0xDF). Returns
    /// false when there's no decodable art so the caller can skip the splash.
    /// The caller clears the screen (to black) first; this only paints the
    /// centered image, leaving the letterbox bars as the cleared background.
    pub fn draw_fullscreen(
        &mut self,
        out: &mut impl Write,
        rom: &Path,
        term_cols: u16,
        term_rows: u16,
        depth: ColorDepth,
    ) -> io::Result<bool> {
        self.load_rom(rom);
        let Some(image) = self.cached_image.as_ref() else {
            return Ok(false);
        };
        if term_cols == 0 || term_rows == 0 {
            return Ok(false);
        }
        // The half-block canvas is `term_cols` wide by `term_rows*2` tall in
        // roughly-square pixels (each cell paints two stacked half-pixels).
        // Scale the image into that box keeping its aspect ratio.
        let (iw, ih) = (image.width() as f64, image.height() as f64);
        let (cw, ch) = (term_cols as f64, term_rows as f64 * 2.0);
        let scale = (cw / iw).min(ch / ih);
        let fit_cols = ((iw * scale).round() as u16).clamp(1, term_cols);
        let fit_rows = (((ih * scale) / 2.0).round() as u16).clamp(1, term_rows);
        let x = term_cols.saturating_sub(fit_cols) / 2;
        let y = term_rows.saturating_sub(fit_rows) / 2;
        // Full-screen art letterboxes against the black-cleared screen.
        let bytes = render_ansi(image, x, y, fit_cols, fit_rows, depth, [0, 0, 0]);
        out.write_all(&bytes)?;
        Ok(true)
    }

    /// Whether a local art file exists for `rom` (for the sysop art-coverage
    /// check). Does not decode or cache — just a filename lookup.
    pub fn has_local_art(&self, rom: &Path) -> bool {
        find_art_path(&self.root, rom).is_some()
    }

    /// Whether `rom` has decodable art (decodes and caches it, so a subsequent
    /// `draw` of the same rom is free). Lets the menu decide the panel layout
    /// before the deferred SIXEL emit happens.
    pub fn has_art(&mut self, rom: &Path) -> bool {
        self.load_rom(rom);
        self.cached_image.is_some()
    }

    fn load_rom(&mut self, rom: &Path) {
        if self.cached_rom.as_deref() == Some(rom) {
            return;
        }
        self.cached_rom = Some(rom.to_path_buf());
        self.render_key = None;
        self.render_bytes.clear();
        self.cached_image = find_art_path(&self.root, rom).and_then(|path| {
            ImageReader::open(path)
                .ok()?
                .with_guessed_format()
                .ok()?
                .decode()
                .ok()
                .map(|image| image.to_rgba8())
        });
    }
}

fn default_art_root() -> PathBuf {
    let cwd = PathBuf::from("art");
    if cwd.is_dir() {
        return cwd;
    }
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join("art")))
        .unwrap_or(cwd)
}

fn thumbnail_stem(title: &str) -> String {
    title
        .chars()
        .map(|c| {
            if matches!(c, '&' | '*' | '/' | ':' | '`' | '"' | '<' | '>' | '?' | '\\' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// Map a ROM extension to its per-system art subdirectory. Mirrors
/// `systems.rs` (`SystemDef::id`) and `tools/fetch_game_art.py`.
fn art_system(extension: &str) -> Option<&'static str> {
    match extension {
        "gg" => Some("gg"),
        "sms" => Some("sms"),
        "sg" => Some("sg"),
        "md" | "gen" | "smd" => Some("md"),
        "nes" => Some("nes"),
        "sfc" | "smc" => Some("sfc"),
        "gba" => Some("gba"),
        "pce" => Some("pce"),
        _ => None,
    }
}

fn find_art_path(root: &Path, rom: &Path) -> Option<PathBuf> {
    let title = thumbnail_stem(rom.file_stem()?.to_str()?);
    let system = art_system(&rom.extension()?.to_str()?.to_ascii_lowercase())?;
    // The flat root is only a fallback when no per-system directory exists at
    // all: once art/<system>/ is populated, a same-titled game on another
    // platform must NOT borrow this one's art via the flat directory.
    let sys_dir = root.join(system);
    let mut dirs = vec![sys_dir.clone()];
    if !sys_dir.is_dir() {
        dirs.push(root.to_path_buf());
    }
    for dir in dirs {
        for extension in ["png", "jpg", "jpeg"] {
            let candidate = dir.join(format!("{title}.{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resize `image` to `width`x`height` and alpha-composite it over the solid `bg`
/// color, so any transparency (and the surrounding letterbox, when the caller
/// leaves one) reads as `bg` rather than always black.
fn composite_on(image: &RgbaImage, width: u32, height: u32, bg: [u8; 3]) -> Vec<[u8; 3]> {
    let scaled = image::imageops::resize(image, width, height, FilterType::Triangle);
    scaled
        .pixels()
        .map(|pixel| {
            let a = pixel[3] as u16;
            let blend = |fg: u8, bg: u8| ((fg as u16 * a + bg as u16 * (255 - a)) / 255) as u8;
            [
                blend(pixel[0], bg[0]),
                blend(pixel[1], bg[1]),
                blend(pixel[2], bg[2]),
            ]
        })
        .collect()
}

fn render_ansi(
    image: &RgbaImage,
    x: u16,
    y: u16,
    cols: u16,
    rows: u16,
    depth: ColorDepth,
    bg: [u8; 3],
) -> Vec<u8> {
    if cols == 0 || rows == 0 {
        return Vec::new();
    }
    // One upper-half block represents two independently-colored vertical pixels.
    let pixels = composite_on(image, cols as u32, rows as u32 * 2, bg);
    let mut out = Vec::with_capacity(cols as usize * rows as usize * 20);
    let mut last_sgr = String::new();
    for row in 0..rows as usize {
        let _ = write!(out, "\x1b[{};{}H", y as usize + row + 1, x + 1);
        for col in 0..cols as usize {
            let top = pixels[(row * 2) * cols as usize + col];
            let bottom = pixels[(row * 2 + 1) * cols as usize + col];
            let sgr = color::cell_sgr(
                depth, top[0], top[1], top[2], bottom[0], bottom[1], bottom[2],
            );
            if sgr != last_sgr {
                let _ = write!(out, "\x1b[{sgr}m");
                last_sgr = sgr;
            }
            // UTF-8 here is intentional: the menu's Cp437Writer converts it to
            // byte 0xDF before the terminal sees it.
            out.extend_from_slice("▀".as_bytes());
        }
    }
    out.extend_from_slice(b"\x1b[0m");
    out
}

fn render_sixel(
    image: &RgbaImage,
    x: u16,
    y: u16,
    cols: u16,
    rows: u16,
    cell_pixels: Option<(u16, u16)>,
    bg: [u8; 3],
) -> Vec<u8> {
    if cols == 0 || rows == 0 {
        return Vec::new();
    }
    // CTerm can report this exactly (`CSI = 3 n`). The fallback matches the
    // classic BBS 8x16 cell; the cap keeps arrow-key browsing link-friendly.
    let (cell_h, cell_w) = cell_pixels.unwrap_or((16, 8));
    let available_w = cols as u32 * cell_w.max(1) as u32;
    let available_h = rows as u32 * cell_h.max(1) as u32;
    // Aspect-fit the art in the viewport (box art isn't square — SNES boxes
    // are landscape, Genesis portrait); the panel fill is the letterbox.
    let (iw, ih) = (image.width().max(1) as f64, image.height().max(1) as f64);
    let scale = (available_w as f64 / iw)
        .min(available_h as f64 / ih)
        .min(SIXEL_MAX_SIDE as f64 / iw.max(ih));
    let out_w = ((iw * scale).round() as u32).clamp(1, available_w.max(1));
    let out_h = ((ih * scale).round() as u32).clamp(1, available_h.max(1));
    // Center horizontally, but TOP-align vertically: the pane's height grows
    // with the terminal, and a centered image drifts down the sidebar —
    // anchored under the banner it stays put at any size.
    let col_offset = (available_w - out_w) / 2 / cell_w.max(1) as u32;
    let pixels = composite_on(image, out_w, out_h, bg);
    let mut out = Vec::new();
    let _ = write!(out, "\x1b[{};{}H", y as u32 + 1, x as u32 + col_offset + 1);
    out.extend_from_slice(&encode_sixel(&pixels, out_w, out_h, SIXEL_COLORS));
    out
}

fn coarse_bucket(rgb: [u8; 3]) -> usize {
    let r = rgb[0] as usize * 7 / 255;
    let g = rgb[1] as usize * 7 / 255;
    let b = rgb[2] as usize * 3 / 255;
    (r << 5) | (g << 2) | b
}

fn quantize(pixels: &[[u8; 3]], max_colors: usize) -> (Vec<[u8; 3]>, Vec<u8>) {
    let mut count = [0u32; 256];
    let mut sums = [[0u64; 3]; 256];
    for &rgb in pixels {
        let bucket = coarse_bucket(rgb);
        count[bucket] += 1;
        for (channel, &value) in rgb.iter().enumerate() {
            sums[bucket][channel] += value as u64;
        }
    }
    let mut buckets: Vec<usize> = (0..256).filter(|&bucket| count[bucket] > 0).collect();
    buckets.sort_unstable_by_key(|&bucket| std::cmp::Reverse(count[bucket]));
    buckets.truncate(max_colors.max(1));
    let palette: Vec<[u8; 3]> = buckets
        .iter()
        .map(|&bucket| {
            let n = count[bucket] as u64;
            [
                (sums[bucket][0] / n) as u8,
                (sums[bucket][1] / n) as u8,
                (sums[bucket][2] / n) as u8,
            ]
        })
        .collect();
    let indexed = pixels
        .iter()
        .map(|&rgb| {
            palette
                .iter()
                .enumerate()
                .min_by_key(|(_, color)| {
                    let dr = rgb[0] as i32 - color[0] as i32;
                    let dg = rgb[1] as i32 - color[1] as i32;
                    let db = rgb[2] as i32 - color[2] as i32;
                    dr * dr + dg * dg + db * db
                })
                .map(|(index, _)| index as u8)
                .unwrap_or(0)
        })
        .collect();
    (palette, indexed)
}

fn append_sixel_run(out: &mut Vec<u8>, data: &[u8]) {
    let mut start = 0;
    while start < data.len() {
        let mut end = start + 1;
        while end < data.len() && data[end] == data[start] {
            end += 1;
        }
        let count = end - start;
        if count >= 4 {
            let _ = write!(out, "!{count}");
            out.push(data[start]);
        } else {
            out.extend(std::iter::repeat_n(data[start], count));
        }
        start = end;
    }
}

fn encode_sixel(pixels: &[[u8; 3]], width: u32, height: u32, colors: usize) -> Vec<u8> {
    let (palette, indexed) = quantize(pixels, colors);
    encode_sixel_indexed(&palette, &indexed, width, height)
}

/// Sixel-encode an already-quantized image (palette + per-pixel indices).
/// The in-game renderer quantizes each frame itself (a bucket-map pass, much
/// cheaper than `quantize`'s nearest-palette search) and enters here.
pub(crate) fn encode_sixel_indexed(
    palette: &[[u8; 3]],
    indexed: &[u8],
    width: u32,
    height: u32,
) -> Vec<u8> {
    let mut out = Vec::new();
    // P1=7 declares 1:1 pixels in the DCS intro itself: CTerm maps P1=0 to
    // TWO-pixel-tall sixel rows, and while current CTerm lets the raster
    // attributes override that back to 1x1, older SyncTERM releases do not —
    // the image rendered double-height there (stretched + off-screen).
    // Modern terminals ignore P1 entirely. p2=1 leaves zero bits untouched;
    // the raster attributes stay as belt-and-suspenders for current CTerm.
    let _ = write!(out, "\x1bP7;1q\"1;1;{width};{height}");
    for (index, rgb) in palette.iter().enumerate() {
        let _ = write!(
            out,
            "#{index};2;{};{};{}",
            (rgb[0] as u16 * 100 + 127) / 255,
            (rgb[1] as u16 * 100 + 127) / 255,
            (rgb[2] as u16 * 100 + 127) / 255
        );
    }
    let width = width as usize;
    let height = height as usize;
    for band_y in (0..height).step_by(6) {
        let mut present = vec![false; palette.len()];
        for py in band_y..(band_y + 6).min(height) {
            for &index in &indexed[py * width..(py + 1) * width] {
                present[index as usize] = true;
            }
        }
        let colors_in_band: Vec<usize> = present
            .iter()
            .enumerate()
            .filter_map(|(index, &present)| present.then_some(index))
            .collect();
        for (pass, &color_index) in colors_in_band.iter().enumerate() {
            let _ = write!(out, "#{color_index}");
            let mut sixels = Vec::with_capacity(width);
            for px in 0..width {
                let mut bits = 0u8;
                for bit in 0..6 {
                    let py = band_y + bit;
                    if py < height && indexed[py * width + px] as usize == color_index {
                        bits |= 1 << bit;
                    }
                }
                sixels.push(bits + 0x3f);
            }
            while sixels.last() == Some(&b'?') {
                sixels.pop();
            }
            append_sixel_run(&mut out, &sixels);
            if pass + 1 < colors_in_band.len() {
                out.push(b'$');
            }
        }
        if band_y + 6 < height {
            out.push(b'-');
        }
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RgbaImage {
        RgbaImage::from_fn(8, 8, |x, y| {
            image::Rgba([x as u8 * 30, y as u8 * 30, 120, 255])
        })
    }

    #[test]
    fn libretro_invalid_filename_characters_are_substituted() {
        assert_eq!(thumbnail_stem("A&B: C/D?"), "A_B_ C_D_");
        assert_eq!(
            thumbnail_stem("Sonic the Hedgehog (USA, Europe)"),
            "Sonic the Hedgehog (USA, Europe)"
        );
    }

    #[test]
    fn extensions_map_to_per_system_art_dirs() {
        let root = Path::new("art");
        // No files exist, so these all miss — but the extension gate must accept
        // exactly the three Sega systems and reject everything else.
        for ext in ["gg", "sms", "sg"] {
            assert_eq!(art_system(ext), Some(ext));
        }
        assert_eq!(art_system("gb"), None);
        assert_eq!(art_system("bin"), None);
        assert!(find_art_path(root, Path::new("Sonic.zip")).is_none());
    }

    #[test]
    fn ansi_fallback_fills_exact_cell_viewport() {
        let bytes = render_ansi(&sample(), 10, 4, 3, 2, ColorDepth::C256, [0, 0, 0]);
        assert!(bytes.starts_with(b"\x1b[5;11H"));
        assert_eq!(
            bytes
                .windows("▀".len())
                .filter(|window| *window == "▀".as_bytes())
                .count(),
            6
        );
        assert!(bytes.ends_with(b"\x1b[0m"));
    }

    #[test]
    fn sixel_has_square_raster_and_string_terminator() {
        // 20x10 cells @ 8x16 px = 160x160 available; the square source fills
        // it exactly (offset 0).
        let bytes = render_sixel(&sample(), 2, 3, 20, 10, Some((16, 8)), [0, 0, 0]);
        assert!(bytes.starts_with(b"\x1b[4;3H\x1bP7;1q\"1;1;160;160"));
        assert!(bytes.ends_with(b"\x1b\\"));
    }

    #[test]
    fn sixel_preserves_non_square_aspect() {
        // A 2:1 landscape box (SNES-style) in a 160x160 viewport must render
        // 160x80 — not be squashed into a square.
        let wide = RgbaImage::from_fn(100, 50, |_, _| image::Rgba([10, 20, 30, 255]));
        let bytes = render_sixel(&wide, 0, 0, 20, 10, Some((16, 8)), [0, 0, 0]);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("\"1;1;160;80"), "raster was {text:.60}");
        // And a portrait (Genesis-style) box: 80x160.
        let tall = RgbaImage::from_fn(50, 100, |_, _| image::Rgba([10, 20, 30, 255]));
        let bytes = render_sixel(&tall, 0, 0, 20, 10, Some((16, 8)), [0, 0, 0]);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("\"1;1;80;160"), "raster was {text:.60}");
    }

    #[test]
    fn renderer_finds_platform_art_and_caches_the_encoded_result() {
        let root = std::env::temp_dir().join(format!("lamegear-art-test-{}", std::process::id()));
        let system = root.join("gg");
        std::fs::create_dir_all(&system).unwrap();
        sample().save(system.join("A_B_ C_D.png")).unwrap();

        let mut renderer = ArtRenderer::with_root(root.clone());
        let mut first = Vec::new();
        assert!(renderer
            .draw(
                &mut first,
                Path::new("A&B: C?D.gg"),
                1,
                2,
                8,
                4,
                ColorDepth::C256,
                false,
                None,
                [0, 0, 0],
            )
            .unwrap());
        let mut second = Vec::new();
        assert!(renderer
            .draw(
                &mut second,
                Path::new("A&B: C?D.gg"),
                1,
                2,
                8,
                4,
                ColorDepth::C256,
                false,
                None,
                [0, 0, 0],
            )
            .unwrap());
        assert_eq!(first, second);
        std::fs::remove_dir_all(root).unwrap();
    }
}
