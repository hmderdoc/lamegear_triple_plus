//! CP437 half-block renderer, adapted from lameboy.
//!
//! One terminal column per source pixel, one row per two pixels, upper-half
//! block 0xDF with fg=top / bg=bottom. Delta-encoded: only cells that changed
//! since the last transmitted frame repaint.
//!
//! Fit rules, resolved in this order (spec 3.1):
//!   1. Native 1:1 when the picture fits.
//!   2. Clip: drop up to CLIP_MAX_X edge columns / CLIP_MAX_Y rows (the
//!      hardware-blanked left column and CRT overscan region) if that reaches
//!      1:1. Lossless in practice; decided per fit, not per frame.
//!   3. Scale: nearest-neighbor everything else (the default for small
//!      terminals, not a failure mode).

use crate::color::{self, ColorDepth};
use crate::framebuffer::FrameBuffer;
use crate::shade16;
use std::io::{self, Write};

/// ASCII character palette (ordered by brightness, dark to light)
const ASCII_CHARS: &[u8] = b" .'`^\",:;Il!i><~+_-?][}{1)(|\\/tfjrxnuvczXYUJCLQ0OZmwqpdbkhao*#MW&8%B@$";

/// Most a lossless clip may remove: SMS/GG games mask the left 8 pixels while
/// scrolling (VDP reg 0 bit 5); 8 rows top+bottom is CRT overscan convention.
const CLIP_MAX_X: usize = 8;
const CLIP_MAX_Y: usize = 16;

#[derive(Clone, Copy, PartialEq)]
pub enum RenderMode {
    Ascii,
    Block,
    /// True pixel graphics as sixel DCS streams. Only offered to terminals
    /// that advertise sixel in DA, where it is the default unless the caller
    /// saved another choice. Frames are de-duplicated: an unchanged
    /// picture transmits nothing, same as the block renderer's empty delta.
    Sixel,
}

/// In-game sixel palette size. Bigger than box art's 32: a live frame is the
/// whole point of the mode, and the RLE handles the extra passes fine.
const SIXEL_GAME_COLORS: usize = 64;
/// Upscale cap and absolute pixel cap for the sixel graphic — a maximized
/// hidpi terminal could otherwise ask a Genesis frame to be a megabyte of
/// sixel data 20 times a second. pub(crate): main.rs sizes the "screen:
/// best" resize request so the canvas lands exactly on these caps.
pub(crate) const SIXEL_GAME_MAX_SCALE: usize = 4;
pub(crate) const SIXEL_GAME_MAX_SIDE: usize = 1280;

/// What shape the sixel graphic should DISPLAY as. Consoles never had square
/// pixels: a 256x224 PCE frame filled a 4:3 television. Rendering the raw
/// framebuffer shape (SquarePx) is the distortion, not the authentic look.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SixelAspect {
    /// Raw framebuffer shape, one source pixel square (the old behavior).
    SquarePx,
    /// Target a fixed display ratio (width/height): 4/3 for TV consoles,
    /// 3/2 for GBA's native LCD (which equals its square-pixel shape).
    Display(f64),
    /// Target ratio R as seen THROUGH a display the terminal squeezes to
    /// 4:3 (CTerm-class aspect correction shows the whole pixel canvas at
    /// 4:3 no matter its shape): emitted ratio = canvas_ratio * R * 3/4.
    /// For R = 4/3 this fills the canvas exactly.
    CrtDisplay(f64),
    /// Fill the whole pixel canvas — the manual "wide fill" setting.
    FillCanvas,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FitKind {
    Native,
    Clipped,
    Scaled,
}

#[derive(Clone, Copy, PartialEq)]
struct Cell {
    ch: u8,
    fg: u8,
    bg: u8,
}

impl Cell {
    const SENTINEL: Cell = Cell { ch: 0, fg: 255, bg: 255 };
}

/// CP437 byte for the upper half block. Synchronet treats door output as
/// CP437; emitting raw UTF-8 here would garble on every client.
const HALF_BLOCK: u8 = 0xDF;

pub struct RenderConfig {
    pub mode: RenderMode,
    pub depth: ColorDepth,
    /// Terminal cell size in pixels, (height, width) — CTerm reports it via
    /// `CSI = 3 n`. Only the sixel mode needs it; the 8x16 fallback is the
    /// classic BBS font.
    pub cell_pixels: Option<(u16, u16)>,
}

pub struct Renderer {
    pub config: RenderConfig,
    output_buffer: Vec<u8>,
    last_fg: u8,
    last_bg: u8,
    needs_clear: bool,

    // Source (emulator picture) dimensions currently fitted for.
    src_w: usize,
    src_h: usize,
    // Terminal size as last reported.
    term_cols: u16,
    term_rows: u16,

    // Scaled output dimensions (terminal cells) + centering offsets.
    out_cols: u16,
    out_rows: u16,
    left_pad: u16,
    top_pad: u16,
    fit: FitKind,
    // Nearest-neighbor lookup tables (identity + offset in native/clip fits).
    col_map: Vec<usize>,
    row_map: Vec<usize>,
    // Source span [start, end) behind each output column / sub-row. One
    // pixel wide except when downscaling, where the 16-color matcher
    // averages the whole box instead of point-sampling it.
    col_span: Vec<(usize, usize)>,
    row_span: Vec<(usize, usize)>,
    shade_cache: ShadeCache,

    prev_cells: Vec<Cell>,
    force_repaint: bool,

    // Sixel mode: graphic size in pixels + the last transmitted frame's hash
    // (the de-duplication key — None forces the next frame out).
    sixel_px: (usize, usize),
    last_sixel_hash: Option<u64>,
    /// Whether the sixel path has done its one FULL screen clear. CTerm-class
    /// terminals keep the character buffer under the graphic and re-render
    /// stale cells over it (blink cycles, exposes) — entering a game must
    /// wipe the menu from the char buffer with a real \x1b[2J, not just the
    /// border. After that, cells under the image are spaces and geometry
    /// changes can use the flash-free border-only wipe.
    sixel_ground_cleared: bool,
    /// Text area (height, width) in PIXELS from `CSI 14 t` — ground truth
    /// for the sixel canvas when available (font-size × grid arithmetic can
    /// disagree with reality after a "screen: best" resize).
    text_area_px: Option<(u16, u16)>,
    /// Target display shape for the sixel graphic (see SixelAspect).
    sixel_aspect: SixelAspect,
}

impl Renderer {
    pub fn new(config: RenderConfig, src_w: usize, src_h: usize) -> Self {
        let mut r = Self {
            config,
            output_buffer: Vec::with_capacity(src_w * src_h * 8),
            last_fg: 255,
            last_bg: 255,
            needs_clear: true,
            src_w,
            src_h,
            term_cols: src_w as u16,
            term_rows: (src_h / 2 + 1) as u16,
            out_cols: 0,
            out_rows: 0,
            left_pad: 0,
            top_pad: 0,
            fit: FitKind::Native,
            col_map: Vec::new(),
            row_map: Vec::new(),
            col_span: Vec::new(),
            row_span: Vec::new(),
            shade_cache: ShadeCache::default(),
            prev_cells: Vec::new(),
            force_repaint: false,
            sixel_px: (0, 0),
            last_sixel_hash: None,
            sixel_ground_cleared: false,
            text_area_px: None,
            sixel_aspect: SixelAspect::SquarePx,
        };
        r.refit();
        r
    }

    fn ensure_cache(&mut self) {
        let needed = self.out_cols as usize * self.out_rows as usize;
        if self.prev_cells.len() != needed {
            self.prev_cells = vec![Cell::SENTINEL; needed];
        }
    }

    fn invalidate_cache(&mut self) {
        for c in self.prev_cells.iter_mut() {
            *c = Cell::SENTINEL;
        }
    }

    /// Terminal size changed (resize probe answered). The probe answers about
    /// once a second whether or not anything changed — only a REAL change may
    /// trigger the clear+refit, or every reply becomes a full-screen black
    /// flash on the caller's terminal.
    pub fn update_dimensions(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 || (cols == self.term_cols && rows == self.term_rows) {
            return;
        }
        self.term_cols = cols;
        self.term_rows = rows;
        self.refit();
        self.request_clear();
    }

    /// The emulator picture changed size mid-session (GG/SMS mode switch,
    /// 224-line PAL mode, ...). Refits without tearing down the terminal.
    pub fn set_source(&mut self, w: usize, h: usize) {
        if w == self.src_w && h == self.src_h || w == 0 || h == 0 {
            return;
        }
        self.src_w = w;
        self.src_h = h;
        self.refit();
        self.request_clear();
    }

    /// Sixel-capable terminals report their cell size late (the `CSI = 3 n`
    /// reply rides the same probe as everything else); refit when it lands.
    pub fn set_cell_pixels(&mut self, cp: Option<(u16, u16)>) {
        if cp.is_none() || cp == self.config.cell_pixels {
            return;
        }
        self.config.cell_pixels = cp;
        if self.config.mode == RenderMode::Sixel {
            self.refit();
            self.request_clear();
        }
    }

    /// Change the sixel graphic's target display shape (settings/auto).
    pub fn set_sixel_aspect(&mut self, aspect: SixelAspect) {
        if aspect == self.sixel_aspect {
            return;
        }
        self.sixel_aspect = aspect;
        if self.config.mode == RenderMode::Sixel {
            self.refit();
            self.request_clear();
        }
    }

    /// Text-area pixel report (`CSI 14 t`) landed or changed: the sixel
    /// canvas is re-derived from it (it outranks the font-size arithmetic).
    pub fn set_text_area_px(&mut self, px: Option<(u16, u16)>) {
        if px.is_none() || px == self.text_area_px {
            return;
        }
        self.text_area_px = px;
        if self.config.mode == RenderMode::Sixel {
            self.refit();
            self.request_clear();
        }
    }

    /// Resolve fit: native, then clip, then scale.
    fn refit(&mut self) {
        if self.config.mode == RenderMode::Sixel {
            return self.refit_sixel();
        }
        let cols = self.term_cols as usize;
        // Reserve 1 row for the status bar below the game.
        let usable_rows = (self.term_rows as usize).saturating_sub(1).max(1);
        let usable_subrows = usable_rows * 2;
        let (src_w, src_h) = (self.src_w, self.src_h);

        let (fit, vis_w, vis_h, clip_x, clip_y);
        if src_w <= cols && src_h <= usable_subrows {
            // Rule 1: native.
            (fit, vis_w, vis_h, clip_x, clip_y) = (FitKind::Native, src_w, src_h, 0, 0);
        } else {
            let need_x = src_w.saturating_sub(cols);
            let need_y = src_h.saturating_sub(usable_subrows);
            if need_x <= CLIP_MAX_X && need_y <= CLIP_MAX_Y {
                // Rule 2: clip. Columns come off the left (where the VDP
                // mask garbage lives); rows split top/bottom (overscan).
                (fit, vis_w, vis_h, clip_x, clip_y) =
                    (FitKind::Clipped, src_w - need_x, src_h - need_y, need_x, need_y / 2);
            } else {
                // Rule 3: scale (up or down), aspect preserved.
                let f = (cols as f64 / src_w as f64)
                    .min(usable_rows as f64 / (src_h as f64 / 2.0));
                let w = ((src_w as f64 * f).round() as usize).max(1);
                let h = (((src_h as f64 / 2.0) * f).round() as usize).max(1) * 2;
                (fit, vis_w, vis_h, clip_x, clip_y) = (FitKind::Scaled, w, h, 0, 0);
            }
        }

        let out_cols = vis_w as u16;
        let out_rows = (vis_h / 2).max(1) as u16;
        self.left_pad = (self.term_cols).saturating_sub(out_cols) / 2;
        self.top_pad = (usable_rows as u16).saturating_sub(out_rows) / 2;
        self.fit = fit;

        self.col_map = match fit {
            FitKind::Scaled => (0..vis_w).map(|j| j * src_w / vis_w).collect(),
            _ => (0..vis_w).map(|j| j + clip_x).collect(),
        };
        self.row_map = match fit {
            FitKind::Scaled => (0..vis_h).map(|s| s * src_h / vis_h).collect(),
            _ => (0..vis_h).map(|s| s + clip_y).collect(),
        };
        self.col_span = spans(&self.col_map, src_w, fit);
        self.row_span = spans(&self.row_map, src_h, fit);
        self.out_cols = out_cols;
        self.out_rows = out_rows;
    }

    /// Sixel fit works in PIXELS, not cells: aspect-preserving fill of the
    /// terminal's pixel canvas, exactly like the block renderer fills its
    /// cell grid — an integer-only scale left letterbox bars around every
    /// picture that didn't happen to divide evenly. The caps bound bandwidth;
    /// the status row stays reserved below the graphic.
    fn refit_sixel(&mut self) {
        let (cell_h, cell_w) = self.config.cell_pixels.unwrap_or((16, 8));
        let (mut cell_w, mut cell_h) = (cell_w.max(1) as usize, cell_h.max(1) as usize);
        // The text-area pixel report outranks font-size arithmetic: after a
        // "screen: best" resize the terminal may have switched fonts/modes
        // or granted a different size, and a canvas computed from a stale
        // cell size overflows the real pixel area — seen live as a cropped,
        // bottom-anchored (scrolled) picture. Integer floor = conservative.
        if let Some((area_h, area_w)) = self.text_area_px {
            cell_w = (area_w as usize / (self.term_cols as usize).max(1)).max(1);
            cell_h = (area_h as usize / (self.term_rows as usize).max(1)).max(1);
        }
        // TWO rows reserved, not one: the status row plus a spacer above it.
        // When the graphic ends flush against the status row, a terminal
        // with an off-by-one in its post-graphic cursor placement (pixel row
        // N*cell_h landing "in" row N+1) pushes the cursor past the bottom
        // margin and scrolls the whole screen EVERY frame — seen live as
        // status-bar ghosts marching up the letterbox. The spacer row
        // swallows that disagreement on any terminal.
        let usable_rows = (self.term_rows as usize).saturating_sub(2).max(1);
        let avail_w = self.term_cols as usize * cell_w;
        let avail_h = usable_rows * cell_h;
        let (src_w, src_h) = (self.src_w.max(1), self.src_h.max(1));

        // Target DISPLAY ratio (width/height of the emitted rectangle).
        // SquarePx keeps the framebuffer's own shape; Display(r) letterboxes
        // to the original hardware's screen shape; FillCanvas uses it all.
        let ratio = match self.sixel_aspect {
            SixelAspect::SquarePx => src_w as f64 / src_h as f64,
            SixelAspect::Display(r) => r,
            SixelAspect::CrtDisplay(r) => {
                // The terminal squeezes its FULL canvas (all rows, not just
                // our usable ones) to 4:3 — derive the pre-widening from it.
                let full_h = (self.term_rows.max(1) as usize * cell_h) as f64;
                (avail_w as f64 / full_h) * r * 0.75
            }
            SixelAspect::FillCanvas => avail_w as f64 / avail_h as f64,
        };
        // Maximize height, bounded by: the canvas (both axes), the per-axis
        // source-scale cap, and the absolute side cap.
        let scale_cap = SIXEL_GAME_MAX_SCALE as f64;
        let side_cap = SIXEL_GAME_MAX_SIDE as f64;
        let h = (avail_h as f64)
            .min(avail_w as f64 / ratio)
            .min(src_h as f64 * scale_cap)
            .min(src_w as f64 * scale_cap / ratio)
            .min(side_cap)
            .min(side_cap / ratio);
        let mut px_h = (h.round() as usize).clamp(1, avail_h);
        // Sixel data goes out in 6-pixel bands. If the last band would poke
        // past the canvas, shave the height to a band boundary (<=5px,
        // invisible) — a terminal that paints zero-bits as background would
        // otherwise wipe the top sliver of the status row every frame.
        if px_h.div_ceil(6) * 6 > avail_h {
            px_h = (px_h - px_h % 6).max(1);
        }
        let px_w = ((px_h as f64 * ratio).round() as usize).clamp(1, avail_w);
        self.fit =
            if px_w == src_w && px_h == src_h { FitKind::Native } else { FitKind::Scaled };
        // Nearest-neighbor pixel maps (col_map/row_map are per-PIXEL here,
        // per-cell in the block modes — only render_sixel reads this shape).
        self.col_map = (0..px_w).map(|x| x * src_w / px_w).collect();
        self.row_map = (0..px_h).map(|y| y * src_h / px_h).collect();
        self.sixel_px = (px_w, px_h);

        // The cell rectangle the graphic covers, for letterbox centering and
        // image_rect() consumers (status bar, overlays).
        self.out_cols = (px_w.div_ceil(cell_w) as u16).min(self.term_cols);
        self.out_rows = (px_h.div_ceil(cell_h) as u16).min(usable_rows as u16).max(1);
        self.left_pad = self.term_cols.saturating_sub(self.out_cols) / 2;
        self.top_pad = (usable_rows as u16).saturating_sub(self.out_rows) / 2;
    }

    pub fn fit_kind(&self) -> FitKind {
        self.fit
    }

    /// One-line geometry summary for the sixel-debug log: everything the fit
    /// decided and what it was told, for diagnosing a caller's terminal
    /// without being able to see their screen.
    pub fn debug_geometry(&self) -> String {
        format!(
            "aspect={:?} term={}x{} cell={:?} area={:?} src={}x{} sixel_px={}x{} rect=(l{} t{} {}x{}) fit={:?}",
            self.sixel_aspect,
            self.term_cols,
            self.term_rows,
            self.config.cell_pixels,
            self.text_area_px,
            self.src_w,
            self.src_h,
            self.sixel_px.0,
            self.sixel_px.1,
            self.left_pad,
            self.top_pad,
            self.out_cols,
            self.out_rows,
            self.fit,
        )
    }

    /// (left_pad, top_pad, out_cols, out_rows) of the game image, 0-based cells.
    pub fn image_rect(&self) -> (u16, u16, u16, u16) {
        (self.left_pad, self.top_pad, self.out_cols, self.out_rows)
    }

    /// Full terminal width, for bottom-row chrome (status bar, chat compose).
    pub fn term_cols(&self) -> u16 {
        self.term_cols
    }

    /// Terminal row for the status bar (0-based): always the BOTTOM row.
    /// A letterboxed image floats centered in the rows above; anchoring the
    /// bar to the screen edge keeps it where a status line belongs instead
    /// of hanging off the letterbox.
    pub fn fps_row(&self) -> u16 {
        self.term_rows.saturating_sub(1)
    }

    pub fn request_clear(&mut self) {
        self.needs_clear = true;
    }

    /// Repaint every cell next frame (no clear): periodic keyframe self-heal.
    pub fn request_repaint(&mut self) {
        self.force_repaint = true;
    }

    /// Full \x1b[2J on the next sixel frame, not just the border wipe: for
    /// after TEXT was drawn INSIDE the image region (chat overlay). On
    /// CTerm-class terminals that text stays in the character buffer under
    /// the re-sent graphic and ghosts back over the game on any cell
    /// re-render; only a real clear evicts it.
    pub fn request_ground_clear(&mut self) {
        self.needs_clear = true;
        self.sixel_ground_cleared = false;
    }

    pub fn render<W: Write + ?Sized>(&mut self, fb: &FrameBuffer, out: &mut W) -> io::Result<()> {
        // Track a mid-session picture-size change even if the caller forgot.
        self.set_source(fb.width, fb.height);
        match self.config.mode {
            RenderMode::Block => self.render_block(fb, out),
            RenderMode::Ascii => self.render_ascii(fb, out),
            RenderMode::Sixel => self.render_sixel(fb, out),
        }
    }

    #[inline]
    fn write_u8(&mut self, n: u8) {
        if n >= 100 {
            self.output_buffer.push(b'0' + n / 100);
            self.output_buffer.push(b'0' + (n / 10) % 10);
            self.output_buffer.push(b'0' + n % 10);
        } else if n >= 10 {
            self.output_buffer.push(b'0' + n / 10);
            self.output_buffer.push(b'0' + n % 10);
        } else {
            self.output_buffer.push(b'0' + n);
        }
    }

    #[inline]
    fn write_u16(&mut self, n: u16) {
        let mut started = false;
        for div in [10000u16, 1000, 100, 10] {
            let d = (n / div) % 10;
            if d != 0 || started {
                self.output_buffer.push(b'0' + d as u8);
                started = true;
            }
        }
        self.output_buffer.push(b'0' + (n % 10) as u8);
    }

    #[inline]
    fn move_to(&mut self, row: u16, col: u16) {
        self.output_buffer.push(b'\x1b');
        self.output_buffer.push(b'[');
        self.write_u16(row);
        self.output_buffer.push(b';');
        self.write_u16(col);
        self.output_buffer.push(b'H');
    }

    #[inline]
    fn set_fg_256(&mut self, color: u8) {
        if color != self.last_fg {
            self.output_buffer.extend_from_slice(b"\x1b[38;5;");
            self.write_u8(color);
            self.output_buffer.push(b'm');
            self.last_fg = color;
        }
    }

    #[inline]
    fn set_bg_256(&mut self, color: u8) {
        if color != self.last_bg {
            self.output_buffer.extend_from_slice(b"\x1b[48;5;");
            self.write_u8(color);
            self.output_buffer.push(b'm');
            self.last_bg = color;
        }
    }

    #[inline]
    fn set_pair_16(&mut self, fg: u8, bg: u8) {
        if fg != self.last_fg || bg != self.last_bg {
            // Same bytes as color::sgr16, without a String per change.
            let bold = if fg < 8 { b'0' } else { b'1' };
            self.output_buffer.extend_from_slice(&[
                0x1b,
                b'[',
                bold,
                b';',
                b'3',
                b'0' + (fg & 7),
                b';',
                b'4',
                b'0' + bg,
                b'm',
            ]);
            self.last_fg = fg;
            self.last_bg = bg;
        }
    }

    #[inline]
    fn brightness_to_ascii(brightness: u8) -> u8 {
        let index = (brightness as usize * (ASCII_CHARS.len() - 1)) / 255;
        ASCII_CHARS[index]
    }

    fn begin_frame(&mut self) {
        self.output_buffer.clear();
        self.ensure_cache();
        if self.needs_clear {
            self.output_buffer.extend_from_slice(b"\x1b[2J");
            self.invalidate_cache();
            self.needs_clear = false;
        } else if self.force_repaint {
            self.invalidate_cache();
        }
        self.force_repaint = false;
        self.last_fg = 255;
        self.last_bg = 255;
    }

    fn render_block<W: Write + ?Sized>(&mut self, fb: &FrameBuffer, out: &mut W) -> io::Result<()> {
        self.begin_frame();

        let out_cols = self.out_cols as usize;
        let out_rows = self.out_rows as usize;
        let is16 = self.config.depth == ColorDepth::C16;
        let shade = if is16 { Some(shade16::table()) } else { None };
        let last_sub = fb.height - 1;

        for i in 0..out_rows {
            let row = self.top_pad + i as u16 + 1; // 1-based
            let sy_top = self.row_map[(2 * i).min(last_sub)];
            let sy_bot = self.row_map.get(2 * i + 1).copied().unwrap_or(sy_top).min(last_sub);

            let mut drawing = false;

            for j in 0..out_cols {
                let sx = self.col_map[j];
                let top = fb.get_pixel(sx, sy_top);
                let bottom = fb.get_pixel(sx, sy_bot);

                let (ch, fg_color, bg_color) = if let Some(t) = shade {
                    if self.fit != FitKind::Scaled {
                        let key = (t.key(top.r, top.g, top.b), t.key(bottom.r, bottom.g, bottom.b));
                        self.shade_cache.get(t, key)
                    } else {
                        let (xs, ys_top) = (self.col_span[j], self.row_span[(2 * i).min(last_sub)]);
                        let ys_bot = self.row_span.get(2 * i + 1).copied().unwrap_or(ys_top);
                        // Quadrants: left/right only when the cell spans
                        // more than one source column.
                        let xm = (xs.0 + xs.1) / 2;
                        let split_x = xm > xs.0;
                        let (l, r) = if split_x { ((xs.0, xm), (xm, xs.1)) } else { (xs, xs) };
                        let q = |x, y| {
                            let mut reg = shade16::Region::default();
                            accumulate(t, fb, x, y, &mut reg);
                            reg
                        };
                        let (tl, bl) = (q(l, ys_top), q(l, ys_bot));
                        let (tr, br) =
                            if split_x { (q(r, ys_top), q(r, ys_bot)) } else { (tl, bl) };
                        let (top, bot) = (tl.merge(&tr), bl.merge(&br));
                        match (top.flat_key(), bot.flat_key()) {
                            // Flat halves match like single pixels: memoized.
                            (Some(kt), Some(kb)) => self.shade_cache.get(t, (kt, kb)),
                            _ => t.match_cell(&tl, &tr, &bl, &br, split_x),
                        }
                    }
                } else {
                    (
                        HALF_BLOCK,
                        color::xterm256(top.r, top.g, top.b),
                        color::xterm256(bottom.r, bottom.g, bottom.b),
                    )
                };

                let idx = i * out_cols + j;
                let cell = Cell { ch, fg: fg_color, bg: bg_color };
                if self.prev_cells[idx] == cell {
                    drawing = false;
                    continue;
                }

                if !drawing {
                    self.move_to(row, self.left_pad + 1 + j as u16);
                    drawing = true;
                }
                if is16 {
                    self.set_pair_16(fg_color, bg_color);
                } else {
                    self.set_fg_256(fg_color);
                    self.set_bg_256(bg_color);
                }
                self.output_buffer.push(ch);
                self.prev_cells[idx] = cell;
            }
        }

        self.output_buffer.extend_from_slice(b"\x1b[0m");
        out.write_all(&self.output_buffer)?;
        out.flush()?;
        Ok(())
    }

    fn render_ascii<W: Write + ?Sized>(&mut self, fb: &FrameBuffer, out: &mut W) -> io::Result<()> {
        self.begin_frame();

        let is16 = self.config.depth == ColorDepth::C16;
        if !is16 {
            self.output_buffer.extend_from_slice(b"\x1b[48;5;16m");
        }

        let out_cols = self.out_cols as usize;
        let out_rows = self.out_rows as usize;
        let last_sub = fb.height - 1;

        for i in 0..out_rows {
            let row = self.top_pad + i as u16 + 1;
            let sy_top = self.row_map[(2 * i).min(last_sub)];
            let sy_bot = self.row_map.get(2 * i + 1).copied().unwrap_or(sy_top).min(last_sub);

            let mut drawing = false;

            for j in 0..out_cols {
                let sx = self.col_map[j];
                let top = fb.get_pixel(sx, sy_top);
                let bottom = fb.get_pixel(sx, sy_bot);

                let avg_grey = ((top.to_grey() as u16 + bottom.to_grey() as u16) >> 1) as u8;
                let ascii_char = Self::brightness_to_ascii(avg_grey);

                let fg_r = ((top.r as u16 + bottom.r as u16) >> 1) as u8;
                let fg_g = ((top.g as u16 + bottom.g as u16) >> 1) as u8;
                let fg_b = ((top.b as u16 + bottom.b as u16) >> 1) as u8;

                let (fg_color, bg_id) = if is16 {
                    (color::nearest16(fg_r, fg_g, fg_b, j, i), 0)
                } else {
                    (color::xterm256(fg_r, fg_g, fg_b), 16)
                };

                let idx = i * out_cols + j;
                let cell = Cell { ch: ascii_char, fg: fg_color, bg: bg_id };
                if self.prev_cells[idx] == cell {
                    drawing = false;
                    continue;
                }

                if !drawing {
                    self.move_to(row, self.left_pad + 1 + j as u16);
                    drawing = true;
                }
                if is16 {
                    self.set_pair_16(fg_color, 0);
                } else {
                    self.set_fg_256(fg_color);
                }
                self.output_buffer.push(ascii_char);
                self.prev_cells[idx] = cell;
            }
        }

        self.output_buffer.extend_from_slice(b"\x1b[0m");
        out.write_all(&self.output_buffer)?;
        out.flush()?;
        Ok(())
    }

    /// One frame as one sixel DCS, de-duplicated: if the picture hasn't
    /// changed since the last transmitted frame, nothing is sent (this is
    /// what makes the mode viable on a BBS link — quiet screens are free,
    /// which is the block renderer's key property too).
    fn render_sixel<W: Write + ?Sized>(&mut self, fb: &FrameBuffer, out: &mut W) -> io::Result<()> {
        self.output_buffer.clear();
        if self.needs_clear {
            if self.sixel_ground_cleared {
                // Geometry change mid-game: cells under the image are
                // already spaces, so wiping only the letterbox border
                // avoids blanking the graphic region (a black flash while
                // the replacement DCS decodes).
                self.clear_sixel_border();
            } else {
                // First frame (or after in-image text): evict stale
                // characters everywhere, INCLUDING under the graphic —
                // CTerm-class terminals re-render buffered cells over
                // sixel pixels, so menu remnants would cover the game.
                self.output_buffer.extend_from_slice(b"\x1b[0m\x1b[2J");
                self.sixel_ground_cleared = true;
            }
            self.needs_clear = false;
            self.last_sixel_hash = None;
        }
        if self.force_repaint {
            self.force_repaint = false;
            self.last_sixel_hash = None;
        }

        let hash = frame_hash(fb);
        if self.last_sixel_hash == Some(hash) {
            if !self.output_buffer.is_empty() {
                out.write_all(&self.output_buffer)?;
                out.flush()?;
            }
            return Ok(());
        }
        self.last_sixel_hash = Some(hash);

        // Per-frame quantization, O(pixels): coarse 8x8x4 RGB buckets, the
        // top SIXEL_GAME_COLORS survive as palette entries (bucket means),
        // losing buckets map to their nearest survivor. Retro palettes are
        // small; this is exact for most frames.
        let mut count = [0u32; 256];
        let mut sums = [[0u32; 3]; 256];
        for p in &fb.pixels {
            let b = coarse_bucket(p.r, p.g, p.b);
            count[b] += 1;
            sums[b][0] += p.r as u32;
            sums[b][1] += p.g as u32;
            sums[b][2] += p.b as u32;
        }
        let mut order: Vec<usize> = (0..256).filter(|&b| count[b] > 0).collect();
        order.sort_unstable_by_key(|&b| std::cmp::Reverse(count[b]));
        order.truncate(SIXEL_GAME_COLORS);
        let palette: Vec<[u8; 3]> = order
            .iter()
            .map(|&b| {
                let n = count[b];
                [(sums[b][0] / n) as u8, (sums[b][1] / n) as u8, (sums[b][2] / n) as u8]
            })
            .collect();
        let mut map = [0u8; 256];
        for b in 0..256 {
            if count[b] == 0 {
                continue;
            }
            map[b] = match order.iter().position(|&o| o == b) {
                Some(i) => i as u8,
                None => {
                    let n = count[b];
                    let (r, g, bl) =
                        ((sums[b][0] / n) as i32, (sums[b][1] / n) as i32, (sums[b][2] / n) as i32);
                    palette
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, c)| {
                            let (dr, dg, db) =
                                (r - c[0] as i32, g - c[1] as i32, bl - c[2] as i32);
                            dr * dr + dg * dg + db * db
                        })
                        .map(|(i, _)| i as u8)
                        .unwrap_or(0)
                }
            };
        }

        // Index at OUTPUT resolution through the nearest-neighbor maps.
        let (px_w, px_h) = self.sixel_px;
        if px_w == 0 || px_h == 0 {
            return Ok(());
        }
        let mut indexed = vec![0u8; px_w * px_h];
        for y in 0..px_h {
            let src_row = &fb.pixels[self.row_map[y] * fb.width..];
            let dst = &mut indexed[y * px_w..(y + 1) * px_w];
            for (x, d) in dst.iter_mut().enumerate() {
                let p = src_row[self.col_map[x]];
                *d = map[coarse_bucket(p.r, p.g, p.b)];
            }
        }

        // Cursor to the letterbox origin, then the whole frame as one DCS.
        // The graphic never reaches past term_rows-1, so the post-graphic
        // cursor advance lands on the status row instead of scrolling.
        // NOTE: no DEC 2026 wrapper here — main.rs wraps the whole frame
        // TRANSACTION (graphic + status bar) in one synchronized update, so
        // the bar can't flash between the graphic paint and its own redraw.
        // 2026 doesn't nest; a wrapper here would end the sync early.
        self.move_to(self.top_pad + 1, self.left_pad + 1);
        let payload =
            crate::art::encode_sixel_indexed(&palette, &indexed, px_w as u32, px_h as u32);
        self.output_buffer.extend_from_slice(&payload);
        out.write_all(&self.output_buffer)?;
        out.flush()?;
        Ok(())
    }
}

impl Renderer {
    /// Clear-for-sixel: wipe only the LETTERBOX around the image rect, never
    /// the image region itself. A full `\x1b[2J` blanks the graphic too, and
    /// its replacement is a large DCS — the gap shows as a black flash on
    /// every video-mode switch (H32/H40, PCE 224/240-line flips) and resize.
    /// The next frame overpaints its own rectangle anyway; the status row is
    /// skipped because draw_status repaints it whole. EL variants only
    /// (`\x1b[1K`/`\x1b[K`/`\x1b[2K`) — universally supported.
    fn clear_sixel_border(&mut self) {
        self.output_buffer.extend_from_slice(b"\x1b[0m");
        let img_first = self.top_pad + 1; // 1-based first/last image rows
        let img_last = self.top_pad + self.out_rows;
        for row in 1..self.term_rows {
            if row >= img_first && row <= img_last {
                if self.left_pad > 0 {
                    self.move_to(row, self.left_pad);
                    self.output_buffer.extend_from_slice(b"\x1b[1K");
                }
                let right_start = self.left_pad + self.out_cols + 1;
                if right_start <= self.term_cols {
                    self.move_to(row, right_start);
                    self.output_buffer.extend_from_slice(b"\x1b[K");
                }
            } else {
                self.move_to(row, 1);
                self.output_buffer.extend_from_slice(b"\x1b[2K");
            }
        }
    }
}

/// Direct-mapped memo of the 16-color matcher's one-pixel-per-half answers.
/// A console frame uses few distinct colors, so nearly every cell of a
/// native-fit frame is a hit.
struct ShadeCache {
    slots: Vec<(u32, (u8, u8, u8))>,
}

impl Default for ShadeCache {
    fn default() -> Self {
        ShadeCache { slots: vec![(u32::MAX, (0, 0, 0)); 4096] }
    }
}

impl ShadeCache {
    #[inline]
    fn get(&mut self, t: &shade16::Shade16, (top, bot): (u16, u16)) -> (u8, u8, u8) {
        let key = (top as u32) << 16 | bot as u32;
        let slot = &mut self.slots[(key.wrapping_mul(0x9E37_79B1) >> 20) as usize];
        if slot.0 != key {
            *slot = (key, t.match_pair(top, bot));
        }
        slot.1
    }
}

/// Source span behind each output index of a nearest-neighbor map: the pixel
/// itself, or (downscaling) everything up to the next sample.
fn spans(map: &[usize], src: usize, fit: FitKind) -> Vec<(usize, usize)> {
    (0..map.len())
        .map(|k| {
            let start = map[k];
            let end = match fit {
                FitKind::Scaled => map.get(k + 1).copied().unwrap_or(src).max(start + 1),
                _ => start + 1,
            };
            (start, end.min(src).max(start + 1))
        })
        .collect()
}

/// Add every pixel of a source box to a half-cell's statistics.
fn accumulate(
    t: &shade16::Shade16,
    fb: &FrameBuffer,
    (x0, x1): (usize, usize),
    (y0, y1): (usize, usize),
    h: &mut shade16::Region,
) {
    let x1 = x1.min(fb.width);
    for y in y0..y1.min(fb.height) {
        let row = &fb.pixels[y * fb.width..(y + 1) * fb.width];
        for p in &row[x0.min(x1)..x1] {
            t.add(h, p.r, p.g, p.b);
        }
    }
}

/// Same 8x8x4 RGB bucketing as art.rs's box-art quantizer.
#[inline]
fn coarse_bucket(r: u8, g: u8, b: u8) -> usize {
    ((r as usize * 7 / 255) << 5) | ((g as usize * 7 / 255) << 2) | (b as usize * 3 / 255)
}

/// FNV-1a over the frame's pixels — the sixel de-duplication key.
fn frame_hash(fb: &FrameBuffer) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut mix = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    };
    mix((fb.width & 0xff) as u8);
    mix((fb.height & 0xff) as u8);
    for p in &fb.pixels {
        mix(p.r);
        mix(p.g);
        mix(p.b);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebuffer::{FrameBuffer, Rgb};

    fn count(hay: &[u8], needle: u8) -> usize {
        hay.iter().filter(|&&b| b == needle).count()
    }
    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn native_fit_when_terminal_is_big_enough() {
        // GG 160x144 -> needs 160 cols x 72 rows + 1 status row.
        let mut r = Renderer::new(
            RenderConfig { mode: RenderMode::Block, depth: ColorDepth::C256, cell_pixels: None },
            160,
            144,
        );
        r.update_dimensions(162, 74);
        assert_eq!(r.fit_kind(), FitKind::Native);
        let (_, _, w, h) = r.image_rect();
        assert_eq!((w, h), (160, 72));
    }

    #[test]
    fn sms_clips_one_column_on_synterm_cap() {
        // SMS 256x192 needs 256 cols; a 255-col terminal is 1 short -> clip.
        let mut r = Renderer::new(
            RenderConfig { mode: RenderMode::Block, depth: ColorDepth::C256, cell_pixels: None },
            256,
            192,
        );
        r.update_dimensions(255, 97);
        assert_eq!(r.fit_kind(), FitKind::Clipped);
        let (_, _, w, h) = r.image_rect();
        assert_eq!((w, h), (255, 96));
        // Clipped column comes off the left (VDP mask side).
        assert_eq!(r.col_map[0], 1);
    }

    #[test]
    fn small_terminal_scales() {
        let mut r = Renderer::new(
            RenderConfig { mode: RenderMode::Block, depth: ColorDepth::C256, cell_pixels: None },
            256,
            192,
        );
        r.update_dimensions(80, 24);
        assert_eq!(r.fit_kind(), FitKind::Scaled);
        let (_, _, w, h) = r.image_rect();
        assert!(w <= 80 && h <= 23);
        // Aspect preserved: 256x96 cell-space -> 8:3.
        assert!((w as f64 / h as f64 - 256.0 / 96.0).abs() < 0.2);
    }

    #[test]
    fn block_delta_skips_unchanged_cells() {
        let mut r = Renderer::new(
            RenderConfig { mode: RenderMode::Block, depth: ColorDepth::C256, cell_pixels: None },
            160,
            144,
        );
        r.update_dimensions(20, 11);
        let cells = {
            let (_, _, w, h) = r.image_rect();
            w as usize * h as usize
        };
        let fb = FrameBuffer::new(160, 144);

        let mut b1 = Vec::new();
        r.render(&fb, &mut b1).unwrap();
        assert!(contains(&b1, b"\x1b[2J"));
        assert_eq!(count(&b1, HALF_BLOCK), cells);

        let mut b2 = Vec::new();
        r.render(&fb, &mut b2).unwrap();
        assert_eq!(b2, b"\x1b[0m", "unchanged frame emits no cells");

        let mut fb2 = FrameBuffer::new(160, 144);
        fb2.pixels[0] = Rgb { r: 255, g: 255, b: 255 };
        let mut b3 = Vec::new();
        r.render(&fb2, &mut b3).unwrap();
        assert!(!contains(&b3, b"\x1b[2J"));
        assert_eq!(count(&b3, HALF_BLOCK), 1);
    }

    #[test]
    fn c16_shades_mid_tones_at_native_and_downscaled_fits() {
        // Mid gray between palette steps: shades, not flat blocks, both 1:1
        // and box-averaged; an unchanged frame still emits nothing.
        let mut fb = FrameBuffer::new(64, 48);
        fb.pixels.fill(Rgb { r: 128, g: 128, b: 128 });
        for (cols, rows) in [(80u16, 30u16), (20, 8)] {
            let mut r = Renderer::new(
                RenderConfig { mode: RenderMode::Block, depth: ColorDepth::C16, cell_pixels: None },
                64,
                48,
            );
            r.update_dimensions(cols, rows);
            let mut b1 = Vec::new();
            r.render(&fb, &mut b1).unwrap();
            assert!(b1.iter().any(|&c| (0xB0..=0xB2).contains(&c)), "{cols}x{rows}");
            let mut b2 = Vec::new();
            r.render(&fb, &mut b2).unwrap();
            assert_eq!(b2, b"\x1b[0m");
        }
    }

    fn sixel_renderer(cols: u16, rows: u16, src_w: usize, src_h: usize) -> Renderer {
        let mut r = Renderer::new(
            RenderConfig {
                mode: RenderMode::Sixel,
                depth: ColorDepth::C256,
                cell_pixels: Some((16, 8)),
            },
            src_w,
            src_h,
        );
        r.update_dimensions(cols, rows);
        r
    }

    #[test]
    fn sixel_scale_cap_bounds_a_big_terminal() {
        // GG 160x144 on a 100x40 cell terminal (800x624 px at 8x16): fits
        // 5x wide, 4.3x tall -> capped at SIXEL_GAME_MAX_SCALE = 4x.
        let r = sixel_renderer(100, 40, 160, 144);
        assert_eq!(r.sixel_px, (640, 576));
        assert_eq!(r.fit_kind(), FitKind::Scaled);
        // Cell rect: 640/8 x 576/16 = 80x36 cells, centered above the bar.
        let (left, top, w, h) = r.image_rect();
        assert_eq!((w, h), (80, 36));
        assert_eq!((left, top), (10, 1));
    }

    #[test]
    fn sixel_fills_a_classic_terminal_fractionally() {
        // Genesis 320x224 on 80x24 at 8x16: 22 usable rows (status + spacer
        // reserved) = a 640x352 px canvas; the height-limited 1.57x fill —
        // an integer-only scale would sit at 1:1 with letterbox bars around
        // 61% of the possible picture. 352 is not a 6px band boundary, so
        // the height shaves to 348: the final sixel band must never cross
        // toward the status row.
        let r = sixel_renderer(80, 24, 320, 224);
        let (px_w, px_h) = r.sixel_px;
        assert_eq!(px_h, 348, "band-aligned fill of the 352px canvas");
        assert_eq!(px_w, 497);
        assert_eq!(r.fit_kind(), FitKind::Scaled);
        // Aspect preserved (within the <=5px band shave).
        assert!((px_w as f64 / px_h as f64 - 320.0 / 224.0).abs() < 0.03);
    }

    #[test]
    fn sixel_fit_downscales_fractionally_when_small() {
        // Genesis 320x224 on a 30x20 terminal (240x304 px): scale < 1.
        let r = sixel_renderer(30, 20, 320, 224);
        let (px_w, px_h) = r.sixel_px;
        assert!(px_w < 320, "must be a downscale, got {px_w}");
        assert!(px_w <= 240 && px_h <= 304, "{px_w}x{px_h}");
        // Aspect preserved.
        assert!((px_w as f64 / px_h as f64 - 320.0 / 224.0).abs() < 0.05);
    }

    #[test]
    fn sixel_dedup_skips_unchanged_frames() {
        let mut r = sixel_renderer(100, 40, 160, 144);
        let fb = FrameBuffer::new(160, 144);

        let mut b1 = Vec::new();
        r.render(&fb, &mut b1).unwrap();
        // First frame: a real \x1b[2J — the menu must leave the character
        // buffer too, or CTerm-class terminals re-render it over the game.
        assert!(contains(&b1, b"\x1b[2J"), "game entry does a full ground clear");
        assert!(contains(&b1, b"\x1bP7;1q"), "first frame carries the DCS");
        assert!(b1.ends_with(b"\x1b\\"), "DCS is terminated");
        // No DEC 2026 here: main.rs owns the synchronized-update wrapper so
        // it can span graphic + status bar as ONE atomic present.
        assert!(!contains(&b1, b"\x1b[?2026h"), "renderer must not open sync itself");

        // Identical frame: NOTHING goes over the wire.
        let mut b2 = Vec::new();
        r.render(&fb, &mut b2).unwrap();
        assert!(b2.is_empty(), "unchanged frame transmits zero bytes, got {}", b2.len());

        // One changed pixel: the frame goes out again (full frame — sixel
        // has no cell delta, dedup is all-or-nothing).
        let mut fb2 = FrameBuffer::new(160, 144);
        fb2.pixels[0] = Rgb { r: 255, g: 0, b: 0 };
        let mut b3 = Vec::new();
        r.render(&fb2, &mut b3).unwrap();
        assert!(contains(&b3, b"\x1bP7;1q"));
        assert!(!contains(&b3, b"\x1b[2K"), "no border wipe on an ordinary frame");

        // request_repaint (keyframe): re-send despite an unchanged picture.
        let mut b4 = Vec::new();
        r.render(&fb2, &mut b4).unwrap();
        assert!(b4.is_empty());
        r.request_repaint();
        let mut b5 = Vec::new();
        r.render(&fb2, &mut b5).unwrap();
        assert!(contains(&b5, b"\x1bP7;1q"), "repaint forces a re-send");

        // Geometry change mid-game: flash-free BORDER wipe (EL variants),
        // never a second \x1b[2J — the graphic region must not blank while
        // the replacement DCS decodes.
        r.update_dimensions(90, 36);
        let mut b6 = Vec::new();
        r.render(&fb2, &mut b6).unwrap();
        assert!(!contains(&b6, b"\x1b[2J"), "no full clear on a mid-game refit");
        assert!(contains(&b6, b"\x1b[2K"), "letterbox rows wiped");
        assert!(contains(&b6, b"\x1b[1K"), "pillarbox left segments wiped");

        // Text was drawn inside the image region (chat overlay closed):
        // ground clear again, evicting it from the character buffer.
        r.request_ground_clear();
        let mut b7 = Vec::new();
        r.render(&fb2, &mut b7).unwrap();
        assert!(contains(&b7, b"\x1b[2J"), "ground clear after in-image text");
    }

    /// Not a correctness test: prints per-frame encode cost + wire size at
    /// candidate scale caps so the caps stay measurement-backed. Run with
    /// `cargo test --release bench_sixel -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_sixel_encode() {
        // Busy-frame stand-in: 8x8 colored tiles + LCG dither, far noisier
        // than a real game frame (worst-case palette count and RLE runs).
        let (src_w, src_h) = (320, 224);
        let mut fb = FrameBuffer::new(src_w, src_h);
        let mut lcg: u32 = 12345;
        for y in 0..src_h {
            for x in 0..src_w {
                lcg = lcg.wrapping_mul(1664525).wrapping_add(1013904223);
                let tile = ((x / 8 + y / 8) * 37) as u8;
                let n = (lcg >> 24) as u8 & 0x1f;
                fb.pixels[y * src_w + x] =
                    Rgb { r: tile.wrapping_mul(3) ^ n, g: tile.wrapping_mul(7), b: n << 3 };
            }
        }
        // (cols, rows, cell) canvases that land on ~2x/4x/6x/8x of Genesis.
        for (label, cols, rows) in
            [("2x 640x448", 80, 30), ("4x 1280x896", 160, 58), ("6x 1920x1344", 240, 86), ("8x 2560x1792", 320, 114)]
        {
            let mut r = Renderer::new(
                RenderConfig {
                    mode: RenderMode::Sixel,
                    depth: ColorDepth::C256,
                    cell_pixels: Some((16, 8)),
                },
                src_w,
                src_h,
            );
            r.update_dimensions(cols, rows);
            let mut sink = Vec::new();
            r.render(&fb, &mut sink).unwrap(); // warm (includes clear)
            let n = 10;
            let t = std::time::Instant::now();
            for i in 0..n {
                // Nudge one pixel so dedup doesn't skip the frame.
                fb.pixels[i].r ^= 1;
                sink.clear();
                r.render(&fb, &mut sink).unwrap();
            }
            let ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
            println!(
                "{label}: out {}x{} px, {:.1} ms/frame encode, {:.1} KB/frame wire ({:.1} Mbps @20fps)",
                r.sixel_px.0,
                r.sixel_px.1,
                ms,
                sink.len() as f64 / 1024.0,
                sink.len() as f64 * 20.0 * 8.0 / 1e6
            );
        }
    }

    #[test]
    fn sixel_text_area_report_outranks_font_math() {
        // 80x24 at reported font 8x16 claims a 640x368 canvas, but the
        // terminal's own `CSI 14 t` says the text area is only 640x240 px
        // (a post-resize font/mode switch): the canvas must follow the
        // report — effective cell 8x10, 22 usable rows = 640x220 — or the
        // graphic overflows the real screen (cropped + scrolled picture).
        let mut r = sixel_renderer(80, 24, 160, 144);
        r.set_text_area_px(Some((240, 640)));
        let (px_w, px_h) = r.sixel_px;
        assert_eq!(px_h, 216, "fills 220px band-aligned, not the fictional 352");
        assert_eq!(px_w, 240);
    }

    #[test]
    fn sixel_aspect_targets_the_tv_shape() {
        // The reported SyncTERM case: 132x59 grid of 8x8 cells = a 1056x472
        // ultra-wide canvas. A square-pixel PCE frame (1.14:1) renders as a
        // narrow column; the hardware put 4:3 on the TV. Display(4/3) at 57
        // usable rows (456px) -> 608x456 — the authentic shape.
        let mut r = Renderer::new(
            RenderConfig {
                mode: RenderMode::Sixel,
                depth: ColorDepth::C256,
                cell_pixels: Some((8, 8)),
            },
            256,
            224,
        );
        r.update_dimensions(132, 59);
        r.set_text_area_px(Some((472, 1056)));
        r.set_sixel_aspect(SixelAspect::Display(4.0 / 3.0));
        assert_eq!(r.sixel_px, (608, 456));

        // FillCanvas: use everything (terminals that re-squeeze to 4:3
        // themselves). Width rides the 4x horizontal source-scale cap.
        r.set_sixel_aspect(SixelAspect::FillCanvas);
        let (px_w, px_h) = r.sixel_px;
        assert!(px_w >= 1000, "fills nearly the whole 1056px width, got {px_w}");
        assert!((px_w as f64 / 256.0) <= 4.01, "horizontal scale capped at 4x");
        assert!(px_h < 456, "height gives way to the horizontal cap, got {px_h}");

        // CrtDisplay(4/3): 4:3 as seen THROUGH SyncTERM's 4:3-corrected
        // display of this 2.24:1 canvas — pre-widened to ~fill the width,
        // so the squeezed result on screen is a true 4:3 picture.
        r.set_sixel_aspect(SixelAspect::CrtDisplay(4.0 / 3.0));
        let (px_w, px_h) = r.sixel_px;
        assert_eq!(px_h, 456, "full height, got {px_h}");
        assert!(px_w >= 1010, "pre-widened to ~97% of the canvas, got {px_w}");
        // Squeezed by (4/3)/2.237 = 0.596 on screen -> displayed ratio 4:3.
        let displayed = px_w as f64 * ((4.0 / 3.0) / (1056.0 / 472.0)) / px_h as f64;
        assert!((displayed - 4.0 / 3.0).abs() < 0.02, "displays as 4:3, got {displayed}");
    }

    #[test]
    fn sixel_cell_pixels_report_refits() {
        let mut r = Renderer::new(
            RenderConfig { mode: RenderMode::Sixel, depth: ColorDepth::C256, cell_pixels: None },
            160,
            144,
        );
        r.update_dimensions(50, 20);
        // 8x16 fallback, 18 usable rows: 400x288 px canvas -> exact 2x.
        assert_eq!(r.sixel_px, (320, 288));
        // A late CSI = 3 n reply with a bigger font grows the pixel canvas:
        // 800x608 -> the 4x cap.
        r.set_cell_pixels(Some((32, 16)));
        assert_eq!(r.sixel_px, (640, 576));
    }

    #[test]
    fn mid_session_source_change_refits() {
        let mut r = Renderer::new(
            RenderConfig { mode: RenderMode::Block, depth: ColorDepth::C256, cell_pixels: None },
            160,
            144,
        );
        r.update_dimensions(255, 97);
        assert_eq!(r.fit_kind(), FitKind::Native);
        // GG -> SMS-sized picture mid-session.
        let fb = FrameBuffer::new(256, 192);
        let mut sink = Vec::new();
        r.render(&fb, &mut sink).unwrap();
        assert_eq!(r.fit_kind(), FitKind::Clipped);
    }
}
