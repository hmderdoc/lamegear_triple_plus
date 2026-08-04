//! Themed multi-system menu: a carousel system switcher where every console
//! gets its own color scheme and chrome, plus a ROM browser with type-ahead
//! search. Responsive with an 80x24 floor; draws through the CP437 writer so
//! box glyphs land as CP437 bytes on the wire.

use crate::color::{cell_sgr, ColorDepth, ColorSetting};
use crate::config::UserConfig;
use crate::emu::Machine;
use crate::keys::{Input, Key, MenuEvent};
use crate::multiplayer::{Event as MpEvent, LinkState, Multiplayer};
use crate::renderer::RenderMode;
use crate::systems::{Rgb8, SystemDef, Theme, SYSTEMS};
use crate::term::Term;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A netplay session to start instead of a solo game.
pub struct LinkedLaunch {
    pub peer: String,
    pub initiator: bool,
}

pub struct MenuResult {
    pub rom_path: PathBuf,
    pub machine: Machine,
    pub system_index: usize,
    pub linked: Option<LinkedLaunch>,
    /// True when this launch is an idle-timer attract demo, not a caller pick.
    pub attract: bool,
}

/// The game room is one more page on the system carousel, with its own
/// theme: phosphor green, the color of a healthy connection.
const LOBBY_THEME: Theme = Theme {
    backdrop: Rgb8(4, 12, 6),
    panel: Rgb8(8, 24, 12),
    frame: Rgb8(60, 170, 90),
    title: Rgb8(140, 255, 170),
    accent: Rgb8(80, 220, 120),
    accent2: Rgb8(255, 200, 60),
    text: Rgb8(200, 235, 205),
    dim: Rgb8(100, 140, 110),
    sel_fg: Rgb8(8, 24, 12),
    sel_bg: Rgb8(80, 220, 120),
};

/// A shelf of club consoles: one full, one running, one powered off.
const LOBBY_DOODLE: &[&str] = &[
    "┌────┐  ┌────┐  ┌────┐",
    "│▒▒▒▒│  │▒▒▒▒│  │    │",
    "└────┘  └────┘  └────┘",
    " o  o    o       off  ",
];

const MIN_COLS: u16 = 80;
const MIN_ROWS: u16 = 24;
// Generous ceilings: big terminals get a big panel (the extra width feeds
// the box-art pane, not the list — see sidebar_w).
const MAX_COLS: u16 = 160;
const MAX_ROWS: u16 = 50;
const TYPEAHEAD_RESET: Duration = Duration::from_secs(1);

struct RomEntry {
    path: PathBuf,
    display: String,
}

/// One entry in the BACKSPACE navigation stack.
#[derive(Clone, Copy, PartialEq)]
enum NavPage {
    Lobby,
    System(usize),
}

/// The friendly part of a ROM file name: title before any parenthesized
/// region tags, extension dropped.
fn friendly_name(path: &std::path::Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
    let cut = stem.find('(').map(|i| i.min(stem.len())).unwrap_or(stem.len());
    let name = stem[..cut].trim().trim_end_matches('-').trim();
    if name.is_empty() { stem.to_string() } else { name.to_string() }
}

/// The name a game travels under on the wire (STATUS, challenges): the
/// friendly title, so every door can resolve it against its own library.
pub(crate) fn friendly_rom_name(path: &std::path::Path) -> String {
    friendly_name(path)
}

/// Find a ROM by its friendly display name across every system's extension
/// set. Used wherever a game name arrives over the wire (challenges, knocks)
/// and must resolve to a local file.
pub(crate) fn find_rom_in_dir(roms_dir: &std::path::Path, name: &str) -> Option<(PathBuf, Machine)> {
    for sys in SYSTEMS {
        for entry in scan_roms(roms_dir, sys) {
            if entry.display.eq_ignore_ascii_case(name) {
                let machine = entry
                    .path
                    .extension()
                    .and_then(|e| e.to_str())
                    .and_then(Machine::from_extension)?;
                return Some((entry.path, machine));
            }
        }
    }
    None
}

/// Scan `dir` recursively for one system's extensions, so sysops can sort
/// ROMs into subfolders (per system, alphabetical, whatever). Dot-entries
/// (`.saves`, `.replays`, ...) are skipped; directories are deduped by
/// canonical path so symlink cycles can't loop the walk.
fn scan_roms(dir: &std::path::Path, system: &SystemDef) -> Vec<RomEntry> {
    /// Deep enough for any sane library layout, shallow enough to bound
    /// runaway nesting.
    const MAX_DEPTH: usize = 8;
    let mut found: Vec<RomEntry> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut seen_dirs = std::collections::HashSet::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(dir.to_path_buf(), 0)];
    while let Some((d, depth)) = stack.pop() {
        let dcanon = d.canonicalize().unwrap_or_else(|_| d.clone());
        if !seen_dirs.insert(dcanon) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let hidden = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if hidden {
                continue;
            }
            if path.is_dir() {
                if depth < MAX_DEPTH {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else { continue };
            if !system.extensions.contains(&ext.to_ascii_lowercase().as_str()) {
                continue;
            }
            let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
            if seen.insert(canon) {
                found.push(RomEntry { display: friendly_name(&path), path });
            }
        }
    }
    found.sort_by(|a, b| a.display.to_lowercase().cmp(&b.display.to_lowercase()));
    found
}

pub struct MenuState {
    pub system: usize,
    pub cfg: UserConfig,
    pub depth: ColorDepth,
    pub mode: RenderMode,
    roms_dir: PathBuf,
    roms: Vec<RomEntry>,
    selected: usize,
    scroll: usize,
    typeahead: String,
    typeahead_at: Option<Instant>,
    cols: u16,
    rows: u16,
    /// Showing the link-lobby carousel page (only reachable when connected).
    lobby: bool,
    lobby_sel: usize,
    /// Transient one-line notice (challenge rejected, player left, ...).
    flash: Option<(String, Instant)>,
    /// Peer we're mid-countdown with (LINK_START seen, waiting for LINK_OPEN).
    linking_with: Option<String>,
    /// Game named by the active challenge (resolved to a ROM on LINK_OPEN).
    linking_game: Option<String>,
    /// Per-user key for sidecar stores (Game Genie codes).
    pub user: Option<String>,
    /// Attract mode: Some(idle_secs) arms the menu idle timer.
    pub attract_idle_secs: Option<u64>,
    /// Indices into SYSTEMS shown on the carousel (Genesis is sysop-gated).
    enabled: Vec<usize>,
    /// Game-room console cap per SYSTEMS entry (how many machines of each
    /// kind the club owns; sysop ini `consoles = ...`).
    console_caps: Vec<usize>,
    /// BACKSPACE navigation stack: the pages the caller came from, newest
    /// last. TAB/arrows move around the carousel; BACKSPACE retraces this.
    nav: Vec<NavPage>,
    /// A link countdown carried in from mid-game (open-port join / accepted
    /// knock): show_menu resumes it instead of resetting to a clean menu.
    pub resume_link: Option<(String, String)>,
    /// Quitting a game lands back on that machine's shelf (lameboy behavior),
    /// last game still selected, with the room one BACKSPACE away.
    pub resume_shelf: Option<usize>,
    /// Auto-detected defaults (resolved from the terminal's capability
    /// probes), shown by the settings page next to "auto".
    pub auto_sound: bool,
    pub auto_screen: bool,
    /// GLOBAL CHAT modal (backtick): who's here + transcript + compose. In
    /// the menu nothing competes for the screen, so chat gets the full view.
    chat_open: bool,
    chat_input: String,
    /// Box-art previews.
    art: std::cell::RefCell<crate::art::ArtRenderer>,
    sixel_supported: bool,
    cell_pixels: Option<(u16, u16)>,
    sel_changed_at: Instant,
}

impl MenuState {
    pub fn new(
        roms_dir: PathBuf,
        cfg: UserConfig,
        depth: ColorDepth,
        mode: RenderMode,
        cols: u16,
        rows: u16,
    ) -> Self {
        let system = cfg.system.min(SYSTEMS.len() - 1);
        let mut st = MenuState {
            system,
            cfg,
            depth,
            mode,
            roms_dir,
            roms: Vec::new(),
            selected: 0,
            scroll: 0,
            typeahead: String::new(),
            typeahead_at: None,
            cols,
            rows,
            // The game room is the entry view; starting here keeps the very
            // first show_menu from pushing a phantom page onto the nav stack.
            lobby: true,
            lobby_sel: 0,
            flash: None,
            linking_with: None,
            linking_game: None,
            user: None,
            attract_idle_secs: None,
            enabled: (0..SYSTEMS.len()).collect(),
            console_caps: SYSTEMS
                .iter()
                .map(|s| crate::config::default_console_cap(s.id))
                .collect(),
            nav: Vec::new(),
            resume_link: None,
            resume_shelf: None,
            auto_sound: false,
            auto_screen: false,
            chat_open: false,
            chat_input: String::new(),
            art: std::cell::RefCell::new(crate::art::ArtRenderer::new()),
            sixel_supported: false,
            cell_pixels: None,
            sel_changed_at: Instant::now(),
        };
        st.rescan();
        st
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    /// Post a notice from outside the menu (e.g. "link server offline").
    pub fn set_notice(&mut self, msg: impl Into<String>) {
        self.flash(msg);
    }

    /// Find a ROM by its friendly display name across every system's
    /// extension set (a challenge names the game, not the file).
    fn find_rom_by_name(&self, name: &str) -> Option<(PathBuf, Machine)> {
        find_rom_in_dir(&self.roms_dir, name)
    }

    fn sys(&self) -> &'static SystemDef {
        &SYSTEMS[self.system]
    }

    pub fn term_size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Game-room console caps, one per SYSTEMS entry.
    pub fn set_console_caps(&mut self, caps: Vec<usize>) {
        if caps.len() == SYSTEMS.len() {
            self.console_caps = caps;
        }
    }

    /// Restrict the carousel to these SYSTEMS indices (sysop gating).
    pub fn set_enabled_systems(&mut self, enabled: Vec<usize>) {
        self.enabled = if enabled.is_empty() { vec![0] } else { enabled };
        if !self.enabled.contains(&self.system) {
            self.system = self.enabled[0];
            self.rescan();
        }
    }

    fn rescan(&mut self) {
        self.roms = scan_roms(&self.roms_dir, self.sys());
        self.selected = 0;
        self.scroll = 0;
        // Restore last game for this system if still present.
        if let Some(last) = &self.cfg.last_game {
            if let Some(i) = self.roms.iter().position(|r| {
                r.path.file_name().and_then(|n| n.to_str()) == Some(last.as_str())
            }) {
                self.selected = i;
            }
        }
    }

    /// The page the caller is looking at, as a nav-stack entry.
    fn nav_current(&self) -> NavPage {
        if self.lobby { NavPage::Lobby } else { NavPage::System(self.system) }
    }

    /// Remember the current page before leaving it — BACKSPACE returns here.
    fn nav_push(&mut self) {
        let cur = self.nav_current();
        if self.nav.last() == Some(&cur) {
            return;
        }
        if self.nav.len() >= 32 {
            self.nav.remove(0);
        }
        self.nav.push(cur);
    }

    /// BACKSPACE: retrace the navigation stack one page (browser-style back,
    /// as opposed to the arrows' carousel movement). False = nowhere to go.
    fn nav_back(&mut self) -> bool {
        while let Some(page) = self.nav.pop() {
            match page {
                NavPage::Lobby => {
                    self.lobby = true;
                }
                NavPage::System(i) if self.enabled.contains(&i) => {
                    self.lobby = false;
                    if self.system != i {
                        self.system = i;
                        self.rescan();
                    }
                }
                NavPage::System(_) => continue, // sysop-gated since: skip past
            }
            self.typeahead.clear();
            return true;
        }
        false
    }

    /// Cycle the carousel over the ENABLED systems plus the game room (the
    /// room is always a page — offline it just shows the machines powered
    /// off).
    fn switch_page(&mut self, dir: isize) {
        self.nav_push();
        let pages = self.enabled.len() as isize + 1;
        let cur = if self.lobby {
            self.enabled.len() as isize
        } else {
            self.enabled.iter().position(|&s| s == self.system).unwrap_or(0) as isize
        };
        let next = (cur + dir % pages + pages) % pages;
        if next == self.enabled.len() as isize {
            self.lobby = true;
        } else {
            self.lobby = false;
            self.system = self.enabled[next as usize];
            self.rescan();
        }
        self.typeahead.clear();
    }

    /// A search is "in flight" if it has text and the last keystroke was
    /// recent — settings/quit hotkeys defer to it so titles containing their
    /// letters stay searchable.
    fn typeahead_active(&self) -> bool {
        !self.typeahead.is_empty()
            && self.typeahead_at.is_some_and(|t| t.elapsed() <= TYPEAHEAD_RESET)
    }

    fn typeahead_push(&mut self, c: char) {
        let now = Instant::now();
        if self.typeahead_at.map_or(true, |t| now.duration_since(t) > TYPEAHEAD_RESET) {
            self.typeahead.clear();
        }
        self.typeahead_at = Some(now);
        self.typeahead.push(c.to_ascii_lowercase());
        let needle = self.typeahead.clone();
        if let Some(i) = self
            .roms
            .iter()
            .position(|r| r.display.to_lowercase().starts_with(&needle))
        {
            self.selected = i;
        }
    }
}

// ---------------------------------------------------------------------------
// Painting

/// Delta-encoded menu painter (same idea as the game renderer): paint()
/// renders into a cell grid, flush() transmits only the cells that differ
/// from the last transmitted frame. An arrow-key repaint costs a few hundred
/// bytes instead of a full-screen burst — full repaints over a slow link are
/// exactly the "menu flicker" a caller sees.
#[derive(Clone, Copy, PartialEq)]
struct PCell {
    ch: char,
    fg: Rgb8,
    bg: Rgb8,
}

impl PCell {
    const BLANK: PCell = PCell { ch: ' ', fg: Rgb8(0, 0, 0), bg: Rgb8(0, 0, 0) };
    /// No drawn cell has ch='\0': invalidates the transmit cache.
    const SENTINEL: PCell = PCell { ch: '\0', fg: Rgb8(0, 0, 0), bg: Rgb8(0, 0, 0) };
}

struct Painter {
    depth: ColorDepth,
    cols: u16,
    rows: u16,
    grid: Vec<PCell>,
    /// What the terminal currently shows (SENTINEL = unknown).
    sent: Vec<PCell>,
    cur_fg: Rgb8,
    cur_bg: Rgb8,
    cursor: (u16, u16), // (row, col), 0-based
    /// Emit a screen clear on the next flush (first frame / resize).
    want_clear: bool,
    /// Raw escape overlay (ANSI box art) appended after the cell diff, plus
    /// the key it was rendered for so identical frames don't resend it.
    overlay: Vec<u8>,
    overlay_key: Option<(PathBuf, u16, u16, u16, u16)>,
    overlay_dirty: bool,
}

impl Painter {
    fn new(depth: ColorDepth) -> Self {
        Painter {
            depth,
            cols: 0,
            rows: 0,
            grid: Vec::new(),
            sent: Vec::new(),
            cur_fg: Rgb8(255, 255, 255),
            cur_bg: Rgb8(0, 0, 0),
            cursor: (0, 0),
            want_clear: true,
            overlay: Vec::new(),
            overlay_key: None,
            overlay_dirty: false,
        }
    }

    /// Start a frame. Resizes/invalidates on geometry change; `full` forces a
    /// clear + full retransmit (first paint of a menu visit, real resize).
    fn begin(&mut self, cols: u16, rows: u16, full: bool) {
        let n = cols as usize * rows as usize;
        if cols != self.cols || rows != self.rows {
            self.cols = cols;
            self.rows = rows;
            self.sent = vec![PCell::SENTINEL; n];
            self.want_clear = true;
        } else if full {
            self.sent.iter_mut().for_each(|c| *c = PCell::SENTINEL);
            self.want_clear = true;
        }
        self.grid.clear();
        self.grid.resize(n, PCell::BLANK);
        // Carry forward what's on screen for cells this frame doesn't touch:
        // painting starts from the previous frame, not from blank.
        for (g, s) in self.grid.iter_mut().zip(self.sent.iter()) {
            if s.ch != '\0' {
                *g = *s;
            }
        }
        self.cursor = (0, 0);
    }

    fn move_to(&mut self, row: u16, col: u16) {
        self.cursor = (row, col);
    }

    fn color(&mut self, fg: Rgb8, bg: Rgb8) {
        self.cur_fg = fg;
        self.cur_bg = bg;
    }

    fn put(&mut self, ch: char) {
        let (r, c) = self.cursor;
        if r < self.rows && c < self.cols {
            self.grid[r as usize * self.cols as usize + c as usize] =
                PCell { ch, fg: self.cur_fg, bg: self.cur_bg };
        }
        self.cursor.1 = self.cursor.1.saturating_add(1);
    }

    fn text(&mut self, s: &str) {
        for ch in s.chars() {
            self.put(ch);
        }
    }

    /// A run of one repeated char (fills use this).
    fn run(&mut self, ch: char, n: usize) {
        for _ in 0..n {
            self.put(ch);
        }
    }

    fn reset(&mut self) {}

    /// Attach raw escape bytes (box art) drawn over `rect` for `key`. Re-sent
    /// only when the key changes; the covered cells are force-retransmitted
    /// the same frame so a stale image never lingers.
    fn set_overlay(&mut self, key: Option<(PathBuf, u16, u16, u16, u16)>, bytes: Vec<u8>) {
        if key == self.overlay_key {
            return;
        }
        // Invalidate the union of the old and new overlay rects.
        for k in [self.overlay_key.clone(), key.clone()].into_iter().flatten() {
            let (_, x, y, w, h) = k;
            for r in y..(y + h).min(self.rows) {
                for c in x..(x + w).min(self.cols) {
                    self.sent[r as usize * self.cols as usize + c as usize] = PCell::SENTINEL;
                }
            }
        }
        self.overlay_key = key;
        self.overlay = bytes;
        self.overlay_dirty = true;
    }

    /// Transmit the difference between `grid` and `sent` as one write.
    fn flush(&mut self, term: &mut dyn Term) -> io::Result<()> {
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        if self.want_clear {
            buf.extend_from_slice(b"\x1b[2J");
            self.want_clear = false;
        }
        let mut last_sgr = String::new();
        let cols = self.cols as usize;
        for r in 0..self.rows as usize {
            let mut drawing = false;
            for c in 0..cols {
                let idx = r * cols + c;
                let cell = self.grid[idx];
                if self.sent[idx] == cell {
                    drawing = false;
                    continue;
                }
                if !drawing {
                    let _ = write!(buf, "\x1b[{};{}H", r + 1, c + 1);
                    drawing = true;
                }
                let sgr = cell_sgr(
                    self.depth, cell.fg.0, cell.fg.1, cell.fg.2, cell.bg.0, cell.bg.1, cell.bg.2,
                );
                if sgr != last_sgr {
                    let _ = write!(buf, "\x1b[{}m", sgr);
                    last_sgr = sgr;
                }
                let mut tmp = [0u8; 4];
                buf.extend_from_slice(cell.ch.encode_utf8(&mut tmp).as_bytes());
                self.sent[idx] = cell;
            }
        }
        if self.overlay_dirty {
            buf.extend_from_slice(&self.overlay);
            self.overlay_dirty = false;
            // The overlay's cells are unknowable — anything the grid later
            // paints there must transmit, which the SENTINELs from
            // set_overlay already guarantee.
        }
        buf.extend_from_slice(b"\x1b[0m");
        // Park the cursor in a corner (some terminals show it despite Hide).
        let _ = write!(buf, "\x1b[{};{}H", self.rows.max(1), self.cols.max(1));
        let mut w = crate::cp437::Cp437Writer::new(&mut *term);
        w.write_all(&buf)?;
        w.flush()
    }
}

/// Truncate to `max` chars (char-safe, ASCII-focused).
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max <= 1 {
        s.chars().take(max).collect()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('~');
        t
    }
}

fn paint(st: &MenuState, p: &mut Painter, mp: Option<&Multiplayer>) {
    let sys = st.sys();
    let lobby = st.lobby;
    let (page_name, page_tagline, page_doodle, th): (&str, &str, &[&str], &Theme) = if lobby {
        ("GAME ROOM", "the club's consoles - walk up and play", LOBBY_DOODLE, &LOBBY_THEME)
    } else {
        (sys.name, sys.tagline, sys.doodle, &sys.theme)
    };

    let cols = st.cols.clamp(MIN_COLS, MAX_COLS);
    let rows = st.rows.clamp(MIN_ROWS, MAX_ROWS);
    let left = st.cols.saturating_sub(cols) / 2;
    let top = st.rows.saturating_sub(rows) / 2;
    let w = cols as usize;

    // Backdrop wash (only when the panel doesn't cover the whole screen).
    if st.cols > cols || st.rows > rows {
        p.color(th.dim, th.backdrop);
        for r in 0..st.rows {
            p.move_to(r, 0);
            p.run(' ', st.cols as usize);
        }
    }

    // ---- frame ----
    p.color(th.frame, th.panel);
    p.move_to(top, left);
    p.text("╔");
    p.run('═', w - 2);
    p.text("╗");
    for r in 1..rows - 1 {
        p.move_to(top + r, left);
        p.text("║");
        p.color(th.text, th.panel);
        p.run(' ', w - 2);
        p.color(th.frame, th.panel);
        p.text("║");
    }
    p.move_to(top + rows - 1, left);
    p.text("╚");
    p.run('═', w - 2);
    p.text("╝");

    let inner_left = left + 2;
    let inner_w = w - 4;

    // ---- header: doodle + carousel wordmark ----
    let doodle_w = page_doodle.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    for (i, line) in page_doodle.iter().enumerate() {
        p.color(th.accent, th.panel);
        p.move_to(top + 1 + i as u16, inner_left);
        p.text(line);
    }
    // Wordmark with carousel arrows, centered in the space right of the doodle.
    let head_left = inner_left + doodle_w as u16 + 2;
    let head_w = inner_w.saturating_sub(doodle_w + 2);
    let spaced: String = page_name.chars().flat_map(|c| [c, ' ']).collect();
    let wordmark = format!("<<  {}  >>", spaced.trim_end());
    let wm_pad = head_w.saturating_sub(wordmark.chars().count()) / 2;
    p.move_to(top + 2, head_left + wm_pad as u16);
    p.color(th.title, th.panel);
    p.text(&clip(&wordmark, head_w));
    // Tagline.
    let tag_pad = head_w.saturating_sub(page_tagline.chars().count()) / 2;
    p.move_to(top + 3, head_left + tag_pad as u16);
    p.color(th.dim, th.panel);
    p.text(&clip(page_tagline, head_w));
    // Game room: a loud chat hint on the free header line under the tagline.
    if lobby && mp.is_some() {
        let hint = "press ` to open GLOBAL CHAT";
        let hint_pad = head_w.saturating_sub(hint.chars().count()) / 2;
        p.move_to(top + 4, head_left + hint_pad as u16);
        p.color(Rgb8(255, 255, 85), th.panel);
        p.text(&clip(hint, head_w));
    }
    // Carousel dots: one per page (enabled systems + the game room).
    let page_count = st.enabled.len() + 1;
    let cur_page = if lobby {
        st.enabled.len()
    } else {
        st.enabled.iter().position(|&s| s == st.system).unwrap_or(0)
    };
    let dots_w = page_count * 2 - 1;
    let dots_pad = head_w.saturating_sub(dots_w + 20) / 2;
    p.move_to(top + 5, head_left + dots_pad as u16);
    for i in 0..page_count {
        if i > 0 {
            p.text(" ");
        }
        if i == cur_page {
            p.color(th.accent2, th.panel);
            p.text("■");
        } else {
            p.color(th.dim, th.panel);
            p.text("·");
        }
    }
    p.color(th.dim, th.panel);
    p.text("  TAB: next page");

    // ---- separator under header ----
    let list_top = top + 1 + page_doodle.len().max(5) as u16 + 1;
    p.color(th.frame, th.panel);
    p.move_to(list_top - 1, left);
    p.text("╠");
    p.run('═', w - 2);
    p.text("╣");

    // ---- footer (separator + two help rows, above the bottom border) ----
    let footer_top = top + rows - 4;
    p.move_to(footer_top, left);
    p.text("╠");
    p.run('═', w - 2);
    p.text("╣");
    let opts_in_sidebar = !lobby && sidebar_options(st, mp.is_some()).is_some();
    let help1 = if lobby {
        // The footer's first line describes what ENTER does on the selection.
        let rows = lobby_rows(st, mp);
        match rows.get(st.lobby_sel.min(rows.len().saturating_sub(1))) {
            Some(row) => lobby_action(st, row),
            None => "no machines - check back in a bit".to_string(),
        }
    } else {
        "Up/Dn  ENTER play  TAB page  BKSP back  L room  type to find  ? keys  Q quit".to_string()
    };
    let help2 = if lobby {
        format!(
            "A let in   R turn away   C withdraw   P 2P port: {}   BKSP back   Q quit",
            if st.cfg.port_open { "OPEN" } else { "closed" }
        )
    } else if opts_in_sidebar {
        // Everything lives in the sidebar's options menu; keep the footer light.
        format!("native {}x{}", sys.native.0, sys.native.1)
    } else {
        // Small screens: point at the consolidated pages instead of
        // enumerating every toggle (they competed with the artwork).
        format!(
            "S settings   A box art   G cheats   P 2P port: {}   native {}x{}",
            if st.cfg.port_open { "OPEN" } else { "closed" },
            sys.native.0,
            sys.native.1,
        )
    };
    p.move_to(footer_top + 1, inner_left);
    p.color(th.text, th.panel);
    p.text(&clip(&help1, inner_w));
    p.move_to(footer_top + 2, inner_left);
    p.color(th.dim, th.panel);
    p.text(&clip(&help2, inner_w));

    // ---- notices: linking countdown > incoming challenge > flash ----
    let notice = if let Some(peer) = &st.linking_with {
        Some((format!(" LINKING WITH {} ... ", peer), true))
    } else if let Some(inc) = mp.and_then(|m| m.incoming().first()) {
        Some((
            format!(" ! {} knocks: {} - A lets them in, R turns them away ", inc.from, friendly_str(&inc.game)),
            true,
        ))
    } else if let Some((msg, at)) = &st.flash {
        (at.elapsed() < Duration::from_secs(5)).then(|| (format!(" {} ", msg), false))
    } else {
        None
    };
    if let Some((text, loud)) = notice {
        p.move_to(footer_top, left + 3);
        if loud {
            p.color(th.panel, th.accent2);
        } else {
            p.color(th.panel, th.accent);
        }
        p.text(&clip(&text, inner_w - 4));
    }

    if lobby {
        paint_lobby(st, p, th, mp, list_top, footer_top, inner_left, inner_w);
        if st.chat_open {
            if let Some(m) = mp {
                paint_chat_modal(st, p, th, m);
            }
        }
        p.reset();
        p.move_to(st.rows - 1, st.cols - 1);
        return;
    }

    // ---- game list ----
    // A controller doodle sits under the list on tall terminals (the lameboy
    // menu look); the list gives up those rows.
    let pad_h = pad_rows(st);
    let list_rows = (footer_top - list_top - pad_h) as usize;
    let count_badge = format!(" {} GAME{} ", st.roms.len(), if st.roms.len() == 1 { "" } else { "S" });
    p.move_to(list_top - 1, left + w as u16 - 4 - count_badge.len() as u16);
    p.color(th.panel, th.accent);
    p.text(&count_badge);

    if !st.typeahead.is_empty() {
        let find = format!(" FIND: {} ", st.typeahead);
        p.move_to(list_top - 1, left + 3);
        p.color(th.panel, th.accent2);
        p.text(&clip(&find, inner_w / 2));
    }

    // Right sidebar (lameboy-menu layout): link banner on top, box art in the
    // middle, options menu at the bottom — only when there's room. SIXEL is
    // never emitted here (SyncTERM drops graphics landing on cells painted in
    // the same burst — it goes out later as its own flush from show_menu's
    // idle arm); ANSI half-block art is plain text and safe to draw inline
    // once the selection has settled.
    let sidebar = sidebar_rect(st);
    let preview = preview_rect(st);
    let list_w = match sidebar {
        Some((_, _, sw, _)) => inner_w - (sw as usize + 3),
        None => inner_w,
    };

    if st.roms.is_empty() {
        p.move_to(list_top + 2, inner_left + 2);
        p.color(th.accent2, th.panel);
        p.text(&clip("no games found!", inner_w));
        p.move_to(list_top + 4, inner_left + 2);
        p.color(th.dim, th.panel);
        let hint = format!(
            "drop .{} files into {}",
            sys.extensions.join("/."),
            st.roms_dir.display()
        );
        p.text(&clip(&hint, inner_w - 2));
    } else {
        // Keep selection in view.
        let scroll = st.scroll.min(st.selected).max(st.selected + 1 - list_rows.min(st.selected + 1));
        // Right-aligned extension tag column (GB/GBC-style from the lameboy
        // menu) when the list is wide enough to spare it.
        let tag_w = if list_w >= 44 { 5 } else { 0 };
        for (row, idx) in (scroll..st.roms.len().min(scroll + list_rows)).enumerate() {
            let entry = &st.roms[idx];
            let r = list_top + row as u16;
            let selected = idx == st.selected;
            let tag = if tag_w > 0 {
                entry
                    .path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_uppercase())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let name_w = list_w - 4 - tag_w;
            p.move_to(r, inner_left);
            if selected {
                p.color(th.sel_fg, th.sel_bg);
                let label = format!(" > {:<width$}", clip(&entry.display, name_w - 1), width = name_w);
                p.text(&clip(&label, list_w - tag_w));
                if tag_w > 0 {
                    p.text(&format!("{:>tag_w$} ", tag, tag_w = tag_w - 1));
                }
            } else {
                p.color(th.text, th.panel);
                let label = format!("   {:<width$}", clip(&entry.display, name_w), width = name_w);
                p.text(&clip(&label, list_w - tag_w));
                if tag_w > 0 {
                    p.color(th.dim, th.panel);
                    p.text(&format!("{:>tag_w$} ", tag, tag_w = tag_w - 1));
                }
            }
        }
        // Scroll indicators.
        if scroll > 0 {
            p.move_to(list_top, inner_left + list_w as u16 - 1);
            p.color(th.accent, th.panel);
            p.text("^");
        }
        if scroll + list_rows < st.roms.len() {
            p.move_to(footer_top - pad_h - 1, inner_left + list_w as u16 - 1);
            p.color(th.accent, th.panel);
            p.text("v");
        }
    }

    // ---- controller doodle under the list (tall terminals) ----
    if pad_h > 0 {
        paint_pad(p, th, sys.machine, footer_top - pad_h, inner_left, list_w);
    }

    if let Some((sb_x, sb_y, sb_w, sb_h)) = sidebar {
        // Separator between list and sidebar.
        p.color(th.frame, th.panel);
        for r in sb_y..sb_y + sb_h {
            p.move_to(r, sb_x - 2);
            p.text("│");
        }
        // ---- link banner (top of the sidebar) ----
        let banner = match mp {
            Some(m) => {
                let n = m.others().count() + 1;
                format!("* {} PLAYER{} ONLINE *", n, if n == 1 { "" } else { "S" })
            }
            None => "- LINK OFFLINE -".to_string(),
        };
        let b_pad = (sb_w as usize).saturating_sub(banner.chars().count()) / 2;
        p.move_to(sb_y, sb_x + b_pad as u16);
        if mp.is_some() {
            p.color(th.accent2, th.panel);
        } else {
            p.color(th.dim, th.panel);
        }
        p.text(&clip(&banner, sb_w as usize));

        // ---- options menu (bottom of the sidebar) ----
        if let Some(opts) = sidebar_options(st, mp.is_some()) {
            let opt_top = sb_y + sb_h - opts.len() as u16 - 1;
            p.color(th.frame, th.panel);
            p.move_to(opt_top, sb_x);
            p.run('─', sb_w as usize);
            for (i, (key, label, bright)) in opts.iter().enumerate() {
                p.move_to(opt_top + 1 + i as u16, sb_x + 1);
                p.color(th.accent, th.panel);
                p.text(&format!("{key:<2}"));
                if *bright {
                    p.color(th.text, th.panel);
                } else {
                    p.color(th.dim, th.panel);
                }
                p.text(&clip(label, sb_w as usize - 4));
            }
        }
    }

    if let Some((pv_x, pv_y, pv_w, pv_h)) = preview {
        // Panel fill (letterbox color matches the theme).
        for r in pv_y..pv_y + pv_h {
            p.move_to(r, pv_x);
            p.color(th.dim, th.panel);
            p.run(' ', pv_w as usize);
        }
        if let Some(entry) = st.roms.get(st.selected) {
            let settled = st.sel_changed_at.elapsed() >= PREVIEW_DEBOUNCE;
            if st.chat_open {
                // The modal owns the screen center: no art underneath it.
                p.set_overlay(None, Vec::new());
            } else if st.sixel_supported {
                // SIXEL is emitted later as its own flush; changing the
                // overlay key here invalidates the panel cells so the
                // previous image gets wiped before the new one lands.
                p.set_overlay(Some((entry.path.clone(), pv_x, pv_y, pv_w, pv_h)), Vec::new());
            } else if settled {
                let bg = [th.panel.0, th.panel.1, th.panel.2];
                let mut art_bytes = Vec::new();
                let drew = st
                    .art
                    .borrow_mut()
                    .draw(&mut art_bytes, &entry.path, pv_x, pv_y, pv_w, pv_h, p.depth, false, st.cell_pixels, bg)
                    .unwrap_or(false);
                if drew {
                    p.set_overlay(Some((entry.path.clone(), pv_x, pv_y, pv_w, pv_h)), art_bytes);
                } else {
                    p.set_overlay(None, Vec::new());
                    p.move_to(pv_y + pv_h / 2, pv_x + pv_w.saturating_sub(8) / 2);
                    p.color(th.dim, th.panel);
                    p.text("no art");
                }
            }
        }
    }

    if st.chat_open {
        if let Some(m) = mp {
            paint_chat_modal(st, p, th, m);
        }
    }

    p.reset();
}

const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(150);
/// Sidebar rows the link banner occupies (text + spacing).
const BANNER_H: u16 = 2;

/// Sidebar width for a given panel width: fixed floor at 100 cols, then the
/// terminal's extra width feeds the box-art pane (the game list keeps a
/// readable width instead of stretching).
fn sidebar_w(cols: u16) -> u16 {
    cols.saturating_sub(70).clamp(30, 64)
}

/// Where the right sidebar (banner + art + options) sits, if the terminal is
/// wide enough: (x, y, w, h) in 0-based cells. Mirrors paint()'s layout math.
fn sidebar_rect(st: &MenuState) -> Option<(u16, u16, u16, u16)> {
    if st.lobby || st.roms.is_empty() {
        return None;
    }
    let cols = st.cols.clamp(MIN_COLS, MAX_COLS);
    let rows = st.rows.clamp(MIN_ROWS, MAX_ROWS);
    if cols < 100 {
        return None;
    }
    let sw = sidebar_w(cols);
    let left = st.cols.saturating_sub(cols) / 2;
    let top = st.rows.saturating_sub(rows) / 2;
    let list_top = top + 1 + st.sys().doodle.len().max(5) as u16 + 1;
    let footer_top = top + rows - 4;
    let sb_x = left + cols - 2 - sw;
    Some((sb_x, list_top, sw, footer_top - list_top))
}

/// The box-art rect inside the sidebar: below the banner, above the options.
fn preview_rect(st: &MenuState) -> Option<(u16, u16, u16, u16)> {
    let (x, y, w, h) = sidebar_rect(st)?;
    let opts_h = sidebar_options(st, true)
        .map(|o| o.len() as u16 + 1)
        .filter(|oh| h >= BANNER_H + oh + 6)
        .unwrap_or(0);
    Some((x, y + BANNER_H, w, h - BANNER_H - opts_h))
}

/// The sidebar's options menu: (hotkey, label, bright). None when the
/// sidebar is too short to fit it (settings stay reachable via the footer
/// hotkeys either way). `online` brightens the lobby line.
fn sidebar_options(
    st: &MenuState,
    online: bool,
) -> Option<Vec<(&'static str, String, bool)>> {
    let (_, _, _, h) = sidebar_rect(st)?;
    // Render/color/sound/screen toggles moved to the settings page (S):
    // they were crowding out the box art, and they're global, not per-shelf.
    let opts: Vec<(&'static str, String, bool)> = vec![
        (
            "L",
            if online { "game room".into() } else { "game room (offline)".into() },
            online,
        ),
        ("`", "global chat".into(), online),
        (
            "P",
            format!("2P port    {}", if st.cfg.port_open { "OPEN" } else { "closed" }),
            online,
        ),
        ("S", "settings".into(), true),
        ("A", "box art".into(), true),
        ("G", "cheat codes".into(), true),
        ("?", "controls / help".into(), true),
        ("Q", "exit".into(), true),
    ];
    (h >= BANNER_H + opts.len() as u16 + 1 + 6).then_some(opts)
}

/// Rows the controller doodle takes under the game list (0 = hidden).
fn pad_rows(st: &MenuState) -> u16 {
    let rows = st.rows.clamp(MIN_ROWS, MAX_ROWS);
    if rows >= 31 && !st.lobby && !st.roms.is_empty() { 6 } else { 0 }
}

/// A compact controller doodle with this system's real key mapping (the
/// lameboy menu's gamepad graphic, adapted per machine): d-pad on the left,
/// select/start slants in the middle, face buttons on the right.
fn paint_pad(
    p: &mut Painter,
    th: &Theme,
    machine: Machine,
    top: u16,
    left: u16,
    width: usize,
) {
    use Machine::*;
    // (face keys shown over the buttons, button names under them)
    let (faces, mid1, mid2): (&[(&str, &str)], &str, &str) = match machine {
        MasterSystem | Sg1000 => (&[("Z", "START"), ("X", "2")], "", "ENTER·pause"),
        GameGear | GameGearExpanded => (&[("Z", "1"), ("X", "2")], "", "ENTER·start"),
        // 6-button pad: top row X/Y/Z on A/S/C, bottom row A/B/C on Z/X/V
        // (SNES-aligned via Street Fighter II).
        Genesis => (
            &[("A", "X"), ("S", "Y"), ("C", "Z"), ("Z", "A"), ("X", "B"), ("V", "C")],
            "SPACE·mode",
            "ENTER·start",
        ),
        Nes => (&[("Z", "A"), ("X", "B")], "SPACE·select", "ENTER·start"),
        Snes => (&[("A", "Y"), ("S", "X"), ("Z", "B"), ("X", "A")], "SPACE·select", "ENTER·start"),
        Gba => (&[("Z", "A"), ("X", "B"), ("C", "L"), ("V", "R")], "SPACE·select", "ENTER·start"),
        Pce => (&[("Z", "I"), ("X", "II")], "SPACE·select", "ENTER·run"),
    };

    // Separator over the doodle.
    p.color(th.frame, th.panel);
    p.move_to(top, left);
    p.run('─', width);

    // d-pad, 4 rows.
    let dp = ["  ┌─┐  ", "┌─┘▲└─┐", "│◄ · ►│", "└─┐▼┌─┘", "  └─┘  "];
    p.color(th.text, th.panel);
    for (i, line) in dp.iter().enumerate().take(5) {
        p.move_to(top + 1 + i as u16, left + 1);
        p.text(line);
    }

    // select/start slants, centered-ish.
    let mid_x = left + 13;
    p.color(th.dim, th.panel);
    if !mid1.is_empty() {
        p.move_to(top + 2, mid_x);
        p.text(&format!("/{mid1}/"));
    }
    if !mid2.is_empty() {
        p.move_to(top + 3, mid_x);
        p.text(&format!("/{mid2}/"));
    }

    // face buttons, right-aligned: (Z) (X) over their button names.
    let btn_w = faces.len() * 6;
    let btn_x = left + (width as u16).saturating_sub(btn_w as u16 + 2);
    for (i, (key, name)) in faces.iter().enumerate() {
        let x = btn_x + i as u16 * 6;
        p.move_to(top + 2, x);
        p.color(th.accent, th.panel);
        p.text(&format!("( {key} )"));
        p.move_to(top + 3, x);
        p.color(th.dim, th.panel);
        let pad = (5usize.saturating_sub(name.chars().count())) / 2;
        p.text(&format!("{:>w$}", name, w = pad + name.chars().count()));
    }
    p.move_to(top + 5, left + 9);
    p.color(th.dim, th.panel);
    p.text("ARROWS·d-pad");
}

/// Challenge game names travel as friendly display names already; guard
/// against empty strings for display.
fn friendly_str(s: &str) -> &str {
    if s.is_empty() { "(none)" } else { s }
}

/// One selectable line in the game room: the lobby is a rack of machines
/// (occupied consoles, then a free one per system with capacity left), with
/// the callers who aren't at a console lounging underneath.
enum LobbyRow {
    /// A powered-on console. Three states of the machine map here: P1 alone
    /// (`p2: None`, port open or closed) or both pads taken (`p2: Some`).
    Console {
        sys_idx: Option<usize>,
        /// Display name of the machine ("GENESIS"; unknown slugs uppercased).
        label: String,
        game: String,
        p1: String,
        /// Roster id to JOIN (open port) or knock on (closed).
        p1_id: String,
        p2: Option<String>,
        port_open: bool,
    },
    /// A powered-off machine of an enabled system: walk up, insert cartridge.
    Free { sys_idx: usize },
    /// A caller browsing the menus (or mid-link-countdown): challengeable.
    Lounge { id: String, status: String },
}

/// System label for a console row ("GENESIS", or the slug an unknown door
/// sent — a lameboy caller shows up as its own kind of machine).
fn console_label(sys_idx: Option<usize>, slug: &str) -> String {
    match sys_idx {
        Some(i) => SYSTEMS[i].name.to_string(),
        None if slug.is_empty() => "???".to_string(),
        None => slug.to_uppercase(),
    }
}

/// Consoles of one system currently powered on, as the roster sees it: solo
/// players count one each, a linked pair counts once (its pad-0 side).
fn consoles_in_use(mp: &Multiplayer, slug: &str) -> usize {
    mp.others()
        .filter(|e| e.system == slug && e.status == "game")
        .filter(|e| !e.in_session() || e.slot == "0")
        .count()
}

/// Build the game room's rows from the roster: linked pairs and solo players
/// become consoles grouped in carousel order, each enabled system with
/// capacity to spare shows one free machine (`in use + 1`), and everyone not
/// at a machine sits in the lounge.
fn lobby_rows(st: &MenuState, mp: Option<&Multiplayer>) -> Vec<LobbyRow> {
    use crate::multiplayer::RosterEntry;
    let others: Vec<&RosterEntry> = mp.map(|m| m.others().collect()).unwrap_or_default();
    let mut seated: std::collections::HashSet<&str> = std::collections::HashSet::new();
    // (system slug, row) so grouping below can claim consoles per system.
    let mut consoles: Vec<(String, LobbyRow)> = Vec::new();

    let build = |p1: &RosterEntry, p2: Option<&RosterEntry>| {
        let sys_idx = crate::systems::system_index_for_id(&p1.system);
        (
            p1.system.clone(),
            LobbyRow::Console {
                sys_idx,
                label: console_label(sys_idx, &p1.system),
                game: p1.game.clone(),
                p1: p1.id.clone(),
                p1_id: p1.id.clone(),
                p2: p2.map(|p| p.id.clone()),
                port_open: p1.port_open(),
            },
        )
    };

    // Linked pairs: one console, both pads taken; pad-0 side listed as P1.
    for e in &others {
        if !e.in_session() || seated.contains(e.id.as_str()) {
            continue;
        }
        let partner = others.iter().find(|o| o.id == e.peer).copied();
        seated.insert(e.id.as_str());
        if let Some(p) = partner {
            seated.insert(p.id.as_str());
        }
        let (p1, p2) = match partner {
            Some(p) if e.slot == "1" => (p, Some(*e)),
            other => (*e, other),
        };
        consoles.push(build(p1, p2));
    }
    // Solo players at a machine.
    for e in &others {
        if e.status == "game" && !e.in_session() && !seated.contains(e.id.as_str()) {
            seated.insert(e.id.as_str());
            consoles.push(build(e, None));
        }
    }

    // Group per enabled system: its consoles, then one free machine if the
    // club has capacity left.
    let mut rows: Vec<LobbyRow> = Vec::new();
    let mut claimed: Vec<bool> = vec![false; consoles.len()];
    for &si in &st.enabled {
        let slug = SYSTEMS[si].id;
        let mine: Vec<usize> = consoles
            .iter()
            .enumerate()
            .filter(|(i, (s, _))| s == slug && !claimed[*i])
            .map(|(i, _)| i)
            .collect();
        let in_use = mine.len();
        for i in mine {
            claimed[i] = true;
            rows.push(std::mem::replace(&mut consoles[i].1, LobbyRow::Free { sys_idx: si }));
        }
        if in_use < st.console_caps.get(si).copied().unwrap_or(1) {
            rows.push(LobbyRow::Free { sys_idx: si });
        }
    }
    // Consoles on systems this door doesn't page (other doors' machines).
    for (i, (_, row)) in consoles.into_iter().enumerate() {
        if !claimed[i] {
            rows.push(row);
        }
    }
    // The lounge: connected but not at a machine.
    for e in &others {
        if !seated.contains(e.id.as_str()) {
            rows.push(LobbyRow::Lounge {
                id: e.id.clone(),
                status: if e.status == "linking" {
                    "linking...".into()
                } else {
                    "browsing the menus".into()
                },
            });
        }
    }
    rows
}

/// Greedy char wrap for chat lines (ASCII-safe transcripts).
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split(' ') {
        let need = if line.is_empty() { word.len() } else { line.len() + 1 + word.len() };
        if need <= width {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        } else {
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            let mut w = word;
            while w.len() > width {
                out.push(w[..width].to_string());
                w = &w[width..];
            }
            line = w.to_string();
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

/// The GLOBAL CHAT modal (spectre conventions: ` compose, ESC closes):
/// who's-here strip, transcript, and a compose line, boxed over the page.
fn paint_chat_modal(st: &MenuState, p: &mut Painter, th: &Theme, mp: &Multiplayer) {
    let cols = st.cols.clamp(MIN_COLS, MAX_COLS);
    let rows = st.rows.clamp(MIN_ROWS, MAX_ROWS);
    let left = st.cols.saturating_sub(cols) / 2;
    let top = st.rows.saturating_sub(rows) / 2;
    let bw = ((cols as usize).saturating_sub(8)).min(72);
    let bh = ((rows as usize).saturating_sub(6)).clamp(12, 22);
    let bx = left + ((cols as usize - bw) / 2) as u16;
    let by = top + ((rows as usize - bh) / 2) as u16;
    let inner = bw - 2;

    // Box + fill.
    p.color(th.accent, th.backdrop);
    p.move_to(by, bx);
    p.text("┌");
    p.run('─', inner);
    p.text("┐");
    for r in 1..bh - 1 {
        p.move_to(by + r as u16, bx);
        p.text("│");
        p.color(th.text, th.backdrop);
        p.run(' ', inner);
        p.color(th.accent, th.backdrop);
        p.text("│");
    }
    p.move_to(by + bh as u16 - 1, bx);
    p.text("└");
    p.run('─', inner);
    p.text("┘");

    let title = " GLOBAL CHAT  (ENTER send ∙ ` or ESC close) ";
    p.move_to(by, bx + 2);
    p.color(th.sel_fg, th.sel_bg);
    p.text(&clip(title, inner - 2));

    // Who's-here strip (roster incl. us), then a divider.
    let mut row = by + 1;
    let mut who: Vec<String> = vec![mp.my_id().to_string()];
    who.extend(mp.others().map(|e| e.id.clone()));
    for line in wrap_text(&format!("here: {}", who.join(", ")), inner - 2).into_iter().take(2) {
        p.move_to(row, bx + 1);
        p.color(th.dim, th.backdrop);
        p.text(&clip(&format!(" {line}"), inner));
        row += 1;
    }
    p.move_to(row, bx);
    p.color(th.accent, th.backdrop);
    p.text("├");
    p.run('─', inner);
    p.text("┤");
    row += 1;

    // Transcript: newest lines that fit above the compose row.
    let compose_row = by + bh as u16 - 2;
    let mut lines: Vec<String> = Vec::new();
    for m in mp.chat_log() {
        for l in wrap_text(&format!("{}: {}", m.from, m.text), inner - 2) {
            lines.push(l);
        }
    }
    let space = compose_row.saturating_sub(row) as usize;
    if lines.len() > space {
        lines = lines[lines.len() - space..].to_vec();
    }
    if lines.is_empty() {
        p.move_to(row, bx + 1);
        p.color(th.dim, th.backdrop);
        p.text(&clip(" (nobody has said anything yet)", inner));
    }
    for (i, l) in lines.iter().enumerate() {
        p.move_to(row + i as u16, bx + 1);
        p.color(th.text, th.backdrop);
        p.text(&clip(&format!(" {l}"), inner));
    }

    // Compose line, spectre-style: "` text_".
    let prompt = format!("` {}_", st.chat_input);
    let shown: String = if prompt.chars().count() > inner - 1 {
        prompt.chars().skip(prompt.chars().count() - (inner - 1)).collect()
    } else {
        prompt
    };
    p.move_to(compose_row, bx + 1);
    p.color(th.sel_fg, th.sel_bg);
    p.text(&clip(&format!("{shown:<w$}", w = inner), inner));
}

/// What ENTER does on this row — shown in the footer for the selection.
fn lobby_action(st: &MenuState, row: &LobbyRow) -> String {
    match row {
        LobbyRow::Console { p2: Some(_), .. } => {
            "both pads taken - pick another machine".into()
        }
        LobbyRow::Console { sys_idx, port_open, p1, .. } => {
            let two = sys_idx
                .map(|i| crate::systems::two_player(SYSTEMS[i].machine))
                .unwrap_or(false);
            if !two {
                "single-player machine - no second pad".into()
            } else if *port_open {
                "ENTER: sit down as P2 - the game restarts for both".into()
            } else {
                format!("ENTER: knock - {p1} must let you in (their game restarts)")
            }
        }
        LobbyRow::Free { sys_idx } => {
            format!("ENTER: insert a cartridge (browse {} games)", SYSTEMS[*sys_idx].name)
        }
        LobbyRow::Lounge { id, .. } => {
            let game = st
                .cfg
                .last_game
                .as_deref()
                .map(|f| friendly_name(std::path::Path::new(f)))
                .unwrap_or_else(|| "(pick a game first)".into());
            format!("ENTER: challenge {id} to {game}")
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_lobby(
    st: &MenuState,
    p: &mut Painter,
    th: &Theme,
    mp: Option<&Multiplayer>,
    list_top: u16,
    footer_top: u16,
    inner_left: u16,
    inner_w: usize,
) {
    let list_rows = (footer_top - list_top) as usize;

    let rows = lobby_rows(st, mp);
    match mp {
        Some(m) => {
            let badge = format!(" {} ONLINE ", m.others().count() + 1);
            p.move_to(list_top - 1, inner_left + inner_w as u16 - 2 - badge.len() as u16);
            p.color(th.panel, th.accent);
            p.text(&badge);
            let me = format!(" you: {} ", m.my_id());
            p.move_to(list_top - 1, inner_left + 1);
            p.color(th.panel, th.frame);
            p.text(&clip(&me, inner_w / 2));
        }
        None => {
            let badge = " LINK OFFLINE - solo only ";
            p.move_to(list_top - 1, inner_left + inner_w as u16 - 2 - badge.len() as u16);
            p.color(th.panel, th.dim);
            p.text(badge);
        }
    }

    if rows.is_empty() {
        p.move_to(list_top + 2, inner_left + 2);
        p.color(th.accent2, th.panel);
        p.text(&clip("no machines on the floor right now", inner_w));
        return;
    }

    // Column layout: SYSTEM | state glyph | cartridge | players | port tag.
    // Fixed overhead: caret(2) + sys + sp + glyph(1) + sp + sp + sp + tag.
    let sys_w = 13usize;
    let tag_w = 10usize;
    let rest = inner_w.saturating_sub(sys_w + tag_w + 7);
    let game_w = rest * 55 / 100;
    let who_w = rest - game_w;

    let sel = st.lobby_sel.min(rows.len() - 1);
    let lounge_at = rows.iter().position(|r| matches!(r, LobbyRow::Lounge { .. }));
    let mut line = 0u16;
    for (i, row) in rows.iter().enumerate() {
        if line as usize >= list_rows {
            break;
        }
        // A shelf divider above the lounge section.
        if lounge_at == Some(i) {
            p.move_to(list_top + line, inner_left);
            p.color(th.frame, th.panel);
            p.text("─── in the lounge ");
            p.run('─', inner_w.saturating_sub(19));
            line += 1;
            if line as usize >= list_rows {
                break;
            }
        }
        let selected = i == sel;
        let r = list_top + line;
        p.move_to(r, inner_left);

        let (sys_label, glyph, cart, who, tag) = match row {
            LobbyRow::Console { sys_idx, label, game, p1, p2, port_open, .. } => {
                let two = sys_idx
                    .map(|ix| crate::systems::two_player(SYSTEMS[ix].machine))
                    .unwrap_or(false);
                let (who, tag) = match p2 {
                    Some(p2) => (format!("{} + {}", p1, p2), "[FULL]".to_string()),
                    None if !two => (format!("P1 {}", p1), "[1P]".to_string()),
                    None if *port_open => (format!("P1 {}", p1), "[2P OPEN]".to_string()),
                    None => (format!("P1 {}", p1), "[solo]".to_string()),
                };
                (label.clone(), '►', friendly_str(game).to_string(), who, tag)
            }
            LobbyRow::Free { sys_idx } => (
                SYSTEMS[*sys_idx].name.to_string(),
                '·',
                "(powered off)".to_string(),
                String::new(),
                "insert >".to_string(),
            ),
            LobbyRow::Lounge { id, status } => {
                let outgoing = mp.and_then(|m| m.outgoing()) == Some(id.as_str());
                (
                    String::new(),
                    ' ',
                    id.clone(),
                    status.clone(),
                    if outgoing { "<< asked".to_string() } else { String::new() },
                )
            }
        };

        let free = matches!(row, LobbyRow::Free { .. });
        let text = format!(
            "{caret}{sys:<sys_w$} {glyph} {cart:<game_w$} {who:<who_w$} {tag:>tag_w$}",
            caret = if selected { "> " } else { "  " },
            sys = clip(&sys_label, sys_w),
            glyph = glyph,
            cart = clip(&cart, game_w),
            who = clip(&who, who_w),
            tag = clip(&tag, tag_w),
        );
        if selected {
            p.color(th.sel_fg, th.sel_bg);
            p.text(&clip(&format!("{:<w$}", text, w = inner_w), inner_w));
        } else if free {
            p.color(th.dim, th.panel);
            p.text(&clip(&text, inner_w));
        } else {
            p.color(th.text, th.panel);
            p.text(&clip(&text, inner_w));
        }
        line += 1;
    }
}

fn visible_rows(st: &MenuState) -> usize {
    let rows = st.rows.clamp(MIN_ROWS, MAX_ROWS);
    let sys = st.sys();
    let header = 1 + sys.doodle.len().max(5) + 1;
    (rows as usize)
        .saturating_sub(header + 4 + pad_rows(st) as usize)
        .max(1)
}

/// Run the menu loop. Returns None when the caller quits the door.
/// The friendly game name we advertise in challenges (derived from the last
/// selected ROM's file name).
fn challenge_game(st: &MenuState) -> Option<String> {
    st.cfg.last_game.as_deref().map(|f| friendly_name(std::path::Path::new(f)))
}

/// Resolve a link that just opened into a launch. The game name travels in
/// the challenge; both sides find their local copy of it.
fn resolve_link_launch(st: &mut MenuState, mp: &mut Multiplayer) -> Option<MenuResult> {
    let peer = match mp.link_state() {
        LinkState::Open { peer, .. } => peer.clone(),
        _ => return None,
    };
    let game = st.linking_game.clone().unwrap_or_default();
    match st.find_rom_by_name(&game) {
        Some((rom_path, machine)) => Some(MenuResult {
            rom_path,
            machine,
            system_index: SYSTEMS.iter().position(|s| s.machine == machine).unwrap_or(0),
            linked: Some(LinkedLaunch { peer, initiator: mp.is_link_initiator() }),
            attract: false,
        }),
        None => {
            mp.abort();
            st.flash(format!("you don't have '{game}' - link aborted"));
            None
        }
    }
}

pub fn show_menu(
    term: &mut dyn Term,
    input: &mut Input,
    st: &mut MenuState,
    mut mp: Option<&mut Multiplayer>,
) -> io::Result<Option<MenuResult>> {
    let mut painter = Painter::new(st.depth);
    let mut dirty = true;
    let mut full_clear = true;
    let mut last_probe = Instant::now();
    let mut last_key_at = Instant::now();
    let mut painted_settled = false;
    let mut lost_link_announced = false;
    // Which selection's SIXEL is currently on screen. Reset on every repaint:
    // paint() opens with a full clear, which destroys any on-screen SIXEL.
    let mut emitted_art: Option<usize> = None;
    // The game room is the menu's entry view when walking into the club —
    // but standing up from a game lands back on that machine's shelf (the
    // last game still selected, lameboy-style), with the room one
    // BACKSPACE away.
    if let Some(si) = st.resume_shelf.take() {
        st.lobby = false;
        if st.enabled.contains(&si) {
            st.system = si;
        }
        st.rescan();
        if st.nav.last() != Some(&NavPage::Lobby) {
            if st.nav.len() >= 32 {
                st.nav.remove(0);
            }
            st.nav.push(NavPage::Lobby);
        }
    } else {
        if !st.lobby {
            st.nav_push();
        }
        st.lobby = true;
        st.lobby_sel = 0;
    }
    st.linking_with = None;
    st.linking_game = None;
    if let Some((peer, game)) = st.resume_link.take() {
        // Mid-game link (open-port join / accepted knock): READY is already
        // sent; show the countdown and let LINK_OPEN below launch it.
        st.linking_with = Some(peer);
        st.linking_game = Some(game);
    } else if let Some(m) = mp.as_deref_mut() {
        m.set_status("menu", "", "", st.cfg.port_open);
    }

    loop {
        // Terminal capability snapshots (resolve asynchronously via probes).
        let (six, cp) = (input.sixel_supported(), input.cell_pixels());
        if six != st.sixel_supported || cp != st.cell_pixels {
            st.sixel_supported = six;
            st.cell_pixels = cp;
            dirty = true;
        }

        // Pump the link connection and react to lobby events.
        if let Some(m) = mp.as_deref_mut() {
            m.pump();
            while let Some(ev) = m.take_event() {
                dirty = true;
                match ev {
                    MpEvent::ChallengeReceived { from, game } => {
                        st.flash(format!("{} challenges you: {}", from, friendly_str(&game)));
                    }
                    MpEvent::ChallengeCanceled => st.flash("challenge withdrawn"),
                    MpEvent::ChallengeRejected { by } => {
                        st.flash(format!("{by} declined"));
                    }
                    MpEvent::LinkStarting { peer, game } => {
                        st.linking_with = Some(peer);
                        st.linking_game = Some(game);
                        m.ready();
                    }
                    MpEvent::LinkOpen { .. } => {
                        if let Some(result) = resolve_link_launch(st, m) {
                            st.cfg.system = st.system;
                            return Ok(Some(result));
                        }
                        st.linking_with = None;
                        st.linking_game = None;
                    }
                    MpEvent::LinkEnded { reason } => {
                        st.linking_with = None;
                        st.linking_game = None;
                        st.flash(format!("link ended: {reason}"));
                    }
                    MpEvent::Error { msg } => st.flash(msg),
                    MpEvent::Chat { from, text } => {
                        // Modal open: it repaints with the new line. Closed:
                        // surface it on the notice row (` opens the modal).
                        if !st.chat_open {
                            st.flash(format!("` {from}: {text}"));
                        }
                    }
                }
            }
            if !m.is_alive() && !lost_link_announced {
                // Stay in the room — it works offline; just say so once.
                lost_link_announced = true;
                st.flash("link server connection lost - machines are solo-only now");
                dirty = true;
            }
        }

        if dirty {
            painter.begin(st.cols, st.rows, full_clear);
            // A dead link paints as offline: stale roster consoles would
            // otherwise sit on the floor forever.
            paint(st, &mut painter, mp.as_deref().filter(|m| m.is_alive()));
            painter.flush(term)?;
            dirty = false;
            if full_clear {
                // A clear destroyed any on-screen SIXEL; re-emit it.
                emitted_art = None;
            }
            full_clear = false;
            painted_settled = st.sel_changed_at.elapsed() >= PREVIEW_DEBOUNCE;
        }

        let timeout = if mp.is_some() { Duration::from_millis(150) } else { Duration::from_millis(400) };
        match input.wait_event_timeout(term, timeout)? {
            MenuEvent::Resize(r, c) => {
                if (c, r) != (st.cols, st.rows) {
                    st.cols = c;
                    st.rows = r;
                    dirty = true;
                    full_clear = true;
                }
            }
            MenuEvent::Idle => {
                // Re-probe the terminal size occasionally: a door never gets
                // SIGWINCH, so this is the only way to notice a resize.
                if last_probe.elapsed() > Duration::from_secs(2) {
                    let _ = crate::send_size_probe(&mut *term, !input.caps_resolved());
                    last_probe = Instant::now();
                }
                // Let transient flashes fade without a keypress.
                if st.flash.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5)) {
                    st.flash = None;
                    dirty = true;
                }
                // ANSI art paints inline once the selection settles.
                if !painted_settled
                    && preview_rect(st).is_some()
                    && st.sel_changed_at.elapsed() >= PREVIEW_DEBOUNCE
                {
                    dirty = true;
                }
                // SIXEL goes out as its own late flush (SyncTERM drops
                // graphics landing on cells painted in the same burst).
                if st.sixel_supported && emitted_art != Some(st.selected) && !dirty
                    && st.sel_changed_at.elapsed() >= PREVIEW_DEBOUNCE
                {
                    if let (Some((x, y, w, h)), Some(entry)) =
                        (preview_rect(st), st.roms.get(st.selected))
                    {
                        let bg = [0u8, 0, 170];
                        let mut wtr = crate::cp437::Cp437Writer::new(&mut *term);
                        let _ = st.art.borrow_mut().draw(
                            &mut wtr, &entry.path, x, y, w, h, st.depth, true, st.cell_pixels, bg,
                        );
                        wtr.flush()?;
                        emitted_art = Some(st.selected);
                    }
                }
                // Attract mode: menu idle long enough -> run a demo of a
                // random game from a random ENABLED system, so the demo reel
                // roams the whole library, not just the page being shown.
                if let Some(idle) = st.attract_idle_secs {
                    let busy = st.linking_with.is_some()
                        || st.chat_open
                        || mp.as_deref().is_some_and(|m| !m.incoming().is_empty());
                    if !busy && last_key_at.elapsed() >= Duration::from_secs(idle) {
                        let nanos = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.subsec_nanos() as usize)
                            .unwrap_or(0);
                        for attempt in 0..st.enabled.len() {
                            let sys_idx =
                                st.enabled[(nanos + attempt) % st.enabled.len()];
                            let roms = scan_roms(&st.roms_dir, &SYSTEMS[sys_idx]);
                            if roms.is_empty() {
                                continue;
                            }
                            let entry = &roms[(nanos / 7919) % roms.len()];
                            return Ok(Some(MenuResult {
                                rom_path: entry.path.clone(),
                                machine: SYSTEMS[sys_idx].machine,
                                system_index: sys_idx,
                                linked: None,
                                attract: true,
                            }));
                        }
                        // Nothing playable anywhere; re-arm the timer.
                        last_key_at = Instant::now();
                    }
                }
            }
            MenuEvent::Key(key) => {
                dirty = true;
                last_key_at = Instant::now();
                let prev_selected = (st.system, st.selected);
                if st.chat_open {
                    // ---- GLOBAL CHAT modal keys ----
                    match key {
                        Key::Esc => st.chat_open = false,
                        Key::Char('`') if st.chat_input.is_empty() => st.chat_open = false,
                        Key::Enter => {
                            if let Some(m) = mp.as_deref_mut() {
                                m.send_chat(&st.chat_input);
                            }
                            st.chat_input.clear();
                        }
                        Key::Backspace => {
                            st.chat_input.pop();
                        }
                        Key::Char(c)
                            if c != '`'
                                && c != '~'
                                && (c.is_ascii_graphic() || c == ' ')
                                && st.chat_input.len() < 180 =>
                        {
                            st.chat_input.push(c);
                        }
                        _ => {}
                    }
                    continue;
                }
                if st.lobby {
                    // ---- game-room keys (no typeahead here) ----
                    let rows = lobby_rows(st, mp.as_deref().filter(|m| m.is_alive()));
                    match key {
                        Key::Esc | Key::Char('q') | Key::Char('Q') => return Ok(None),
                        Key::Tab | Key::Right => st.switch_page(1),
                        Key::BackTab | Key::Left => st.switch_page(-1),
                        Key::Backspace => {
                            st.nav_back();
                        }
                        Key::Up => {
                            if !rows.is_empty() {
                                st.lobby_sel =
                                    st.lobby_sel.checked_sub(1).unwrap_or(rows.len() - 1);
                            }
                        }
                        Key::Down => {
                            if !rows.is_empty() {
                                st.lobby_sel = (st.lobby_sel + 1) % rows.len();
                            }
                        }
                        Key::Enter => {
                            let row = rows.get(st.lobby_sel.min(rows.len().saturating_sub(1)));
                            match row {
                                Some(LobbyRow::Console { p2: Some(_), .. }) => {
                                    st.flash("both pads are taken - try another machine");
                                }
                                Some(LobbyRow::Console {
                                    sys_idx,
                                    p1_id,
                                    p1,
                                    game,
                                    port_open,
                                    ..
                                }) => {
                                    let two = sys_idx
                                        .map(|i| crate::systems::two_player(SYSTEMS[i].machine))
                                        .unwrap_or(false);
                                    if !two {
                                        st.flash("single-player machine - no second pad");
                                    } else if st.find_rom_by_name(game).is_none() {
                                        st.flash(format!(
                                            "you don't have '{}' in your library",
                                            friendly_str(game)
                                        ));
                                    } else if *port_open {
                                        if let Some(m) = mp.as_deref_mut() {
                                            m.join(p1_id);
                                        }
                                        st.flash(format!(
                                            "sitting down at {p1}'s console ..."
                                        ));
                                    } else {
                                        if let Some(m) = mp.as_deref_mut() {
                                            m.challenge(p1_id, game);
                                        }
                                        st.flash(format!(
                                            "knocked - waiting for {p1} to let you in"
                                        ));
                                    }
                                }
                                Some(LobbyRow::Free { sys_idx }) => {
                                    // Walk up to the machine: its game shelf.
                                    st.nav_push();
                                    st.lobby = false;
                                    st.system = *sys_idx;
                                    st.rescan();
                                    st.flash("insert a cartridge: pick a game, ENTER plays it");
                                }
                                Some(LobbyRow::Lounge { id, .. }) => {
                                    let id = id.clone();
                                    match (mp.as_deref_mut(), challenge_game(st)) {
                                        (Some(m), Some(game)) => {
                                            m.challenge(&id, &game);
                                            // Port-trace sweep says this game never
                                            // reads pad 2: P2 would only spectate.
                                            let solo = st
                                                .find_rom_by_name(&game)
                                                .and_then(|(p, _)| crate::link_class_for_rom(&p))
                                                .is_some_and(|c| c == "single-player");
                                            if solo {
                                                st.flash(format!(
                                                    "challenged {id} to {game} - NOTE: looks 1-player, P2 may only watch"
                                                ));
                                            } else {
                                                st.flash(format!("challenged {id} to {game}"));
                                            }
                                        }
                                        (Some(_), None) => {
                                            st.flash("pick a game on a system page first");
                                        }
                                        _ => {}
                                    }
                                }
                                None => {}
                            }
                        }
                        Key::Char('p') | Key::Char('P') => {
                            st.cfg.port_open = !st.cfg.port_open;
                            if let Some(m) = mp.as_deref_mut() {
                                m.set_status("menu", "", "", st.cfg.port_open);
                            }
                            st.flash(if st.cfg.port_open {
                                "2P port OPEN: while you play, anyone may plug in (resets your game)"
                            } else {
                                "2P port closed: callers can only knock"
                            });
                        }
                        Key::Char('`') | Key::Char('~') => {
                            if mp.as_deref().is_some_and(|m| m.is_alive()) {
                                st.chat_open = true;
                            } else {
                                st.flash("chat needs the link server - offline");
                            }
                        }
                        Key::Char('s') | Key::Char('S') => {
                            show_settings_page(term, input, st, mp.as_deref_mut())?;
                            full_clear = true;
                        }
                        Key::Char('a') | Key::Char('A') => {
                            if let Some(m) = mp.as_deref_mut() {
                                if let Some(inc) = m.incoming().first().cloned() {
                                    if st.find_rom_by_name(&inc.game).is_some() {
                                        m.accept(&inc.from);
                                    } else {
                                        m.reject(&inc.from);
                                        st.flash(format!(
                                            "you don't have '{}' - declined",
                                            inc.game
                                        ));
                                    }
                                }
                            }
                        }
                        Key::Char('r') | Key::Char('R') => {
                            if let Some(m) = mp.as_deref_mut() {
                                if let Some(inc) = m.incoming().first().cloned() {
                                    m.reject(&inc.from);
                                }
                            }
                        }
                        Key::Char('c') | Key::Char('C') => {
                            if let Some(m) = mp.as_deref_mut() {
                                m.cancel();
                                st.flash("challenge withdrawn");
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                match key {
                    Key::Esc => return Ok(None),
                    Key::Char('q' | 'Q') if !st.typeahead_active() => return Ok(None),
                    Key::Tab | Key::Right => st.switch_page(1),
                    Key::BackTab | Key::Left => st.switch_page(-1),
                    Key::Up => {
                        if st.selected > 0 {
                            st.selected -= 1;
                        } else if !st.roms.is_empty() {
                            st.selected = st.roms.len() - 1;
                        }
                    }
                    Key::Down => {
                        if !st.roms.is_empty() {
                            st.selected = (st.selected + 1) % st.roms.len();
                        }
                    }
                    Key::PageUp => {
                        st.selected = st.selected.saturating_sub(visible_rows(st));
                    }
                    Key::PageDown => {
                        if !st.roms.is_empty() {
                            st.selected =
                                (st.selected + visible_rows(st)).min(st.roms.len() - 1);
                        }
                    }
                    Key::Backspace => {
                        // Editing a search takes precedence; with nothing
                        // typed, BACKSPACE walks the navigation stack.
                        if st.typeahead.is_empty() {
                            st.nav_back();
                        } else {
                            st.typeahead.pop();
                            st.typeahead_at = Some(Instant::now());
                        }
                    }
                    Key::Enter => {
                        // The club only owns so many of each machine: powering
                        // one on past the cap is blocked while the roster can
                        // see the floor (offline play can't, and isn't).
                        if let Some(m) = mp.as_deref() {
                            let cap = st.console_caps.get(st.system).copied().unwrap_or(1);
                            let in_use = consoles_in_use(m, st.sys().id);
                            if in_use >= cap {
                                st.flash(format!(
                                    "all {cap} {} console{} in use - the game room (L) can seat you as P2",
                                    st.sys().name,
                                    if cap == 1 { " is" } else { "s are" },
                                ));
                                continue;
                            }
                        }
                        if let Some(entry) = st.roms.get(st.selected) {
                            let sys = st.sys();
                            let machine = match (sys.machine, st.cfg.gg_full_frame) {
                                (Machine::GameGear, true) => Machine::GameGearExpanded,
                                (m, _) => m,
                            };
                            st.cfg.system = st.system;
                            st.cfg.last_game = entry
                                .path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .map(String::from);
                            return Ok(Some(MenuResult {
                                rom_path: entry.path.clone(),
                                machine,
                                system_index: st.system,
                                linked: None,
                                attract: false,
                            }));
                        }
                    }
                    // Settings hotkeys are UPPERCASE only, and only when no
                    // search is in flight — lowercase letters always feed the
                    // type-ahead (otherwise "sonic" would toggle sound and
                    // cycle colors on its way to the list).
                    Key::Char(c) => {
                        let searching = st.typeahead_active();
                        match c {
                            // B/C/V stay as silent power-user toggles (with a
                            // flash so the change is visible now that state
                            // moved off the footer); the settings page (S) is
                            // the advertised home for all of them.
                            'B' if !searching => {
                                st.mode = next_render_mode(st.mode, st.sixel_supported);
                                st.cfg.render = Some(st.mode);
                                st.flash(format!(
                                    "render: {}",
                                    crate::config::render_slug(st.mode)
                                ));
                            }
                            'C' if !searching => {
                                let cur = st.cfg.color.unwrap_or(ColorSetting::Auto);
                                st.cfg.color = Some(cur.next());
                                st.flash(format!("color: {}", cur.next().slug()));
                            }
                            'V' if !searching => {
                                st.cfg.gg_full_frame = !st.cfg.gg_full_frame;
                                st.flash(format!(
                                    "GG view: {}",
                                    if st.cfg.gg_full_frame { "full frame" } else { "LCD window" }
                                ));
                            }
                            'S' if !searching => {
                                show_settings_page(term, input, st, mp.as_deref_mut())?;
                                full_clear = true;
                            }
                            'A' if !searching => {
                                if let Some(entry) = st.roms.get(st.selected) {
                                    let rom = entry.path.clone();
                                    show_art_page(term, input, st, &rom)?;
                                    full_clear = true;
                                }
                            }
                            'L' if !searching => {
                                st.nav_push();
                                st.lobby = true;
                                st.typeahead.clear();
                            }
                            'P' if !searching => {
                                st.cfg.port_open = !st.cfg.port_open;
                                if let Some(m) = mp.as_deref_mut() {
                                    m.set_status("menu", "", "", st.cfg.port_open);
                                }
                                st.flash(if st.cfg.port_open {
                                    "2P port OPEN: while you play, anyone may plug in (resets your game)"
                                } else {
                                    "2P port closed: callers can only knock"
                                });
                            }
                            'G' if !searching => {
                                let supported = matches!(
                                    st.sys().machine,
                                    Machine::MasterSystem
                                        | Machine::GameGear
                                        | Machine::GameGearExpanded
                                        | Machine::Sg1000
                                        | Machine::Genesis
                                );
                                if !supported {
                                    st.flash("cheats: SMS/GG/Genesis carts only (for now)");
                                } else if let Some(entry) = st.roms.get(st.selected) {
                                    let rom = entry.path.clone();
                                    show_game_genie_page(term, input, st, &rom)?;
                                }
                            }
                            '?' if !searching => {
                                show_controls_page(term, input, st)?;
                                full_clear = true;
                            }
                            // Backtick opens GLOBAL CHAT even mid-search (no
                            // game title contains one).
                            '`' | '~' => {
                                if mp.as_deref().is_some_and(|m| m.is_alive()) {
                                    st.chat_open = true;
                                } else {
                                    st.flash("chat needs the link server - offline");
                                }
                            }
                            c if c.is_ascii_graphic() || c == ' ' => {
                                st.typeahead_push(c);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
                // Any scroll adjustment happens in paint(); keep scroll near
                // selection here for the visible_rows math.
                let vis = visible_rows(st);
                if st.selected < st.scroll {
                    st.scroll = st.selected;
                } else if st.selected >= st.scroll + vis {
                    st.scroll = st.selected + 1 - vis;
                }
                if (st.system, st.selected) != prev_selected {
                    st.sel_changed_at = Instant::now();
                }
            }
        }
    }
}

/// Full-screen controls reference for the current system: exactly which
/// keyboard key is which pad button, plus the universal door keys. Any key
/// dismisses it.
fn show_controls_page(
    term: &mut dyn Term,
    input: &mut Input,
    st: &MenuState,
) -> io::Result<()> {
    let sys = st.sys();
    let th = &sys.theme;
    let mut painter = Painter::new(st.depth);
    painter.begin(st.cols, st.rows, true);
    let p = &mut painter;
    let left = st.cols.saturating_sub(64) / 2;
    let top = st.rows.saturating_sub(22) / 2;

    p.color(th.title, th.backdrop);
    p.move_to(top, left);
    p.text(&format!("CONTROLS - {}", sys.name));

    p.color(th.accent, th.backdrop);
    p.move_to(top + 2, left);
    p.text("in game");
    let mut row = top + 3;
    p.color(th.text, th.backdrop);
    p.move_to(row, left + 2);
    p.text(&format!("{:<8} d-pad", "ARROWS"));
    row += 1;
    for (key, action) in crate::input::game_key_table(sys.machine) {
        p.move_to(row, left + 2);
        p.text(&format!("{key:<8} {action}"));
        row += 1;
    }
    p.move_to(row, left + 2);
    p.text(&format!("{:<8} save state / load state", "5 / 8"));
    row += 1;
    p.move_to(row, left + 2);
    p.text(&format!("{:<8} 2P port: let a knock in / toggle - decline", "2 / 0"));
    row += 1;
    p.move_to(row, left + 2);
    p.text(&format!("{:<8} chat: compose (again stashes, ESC cancels) / transcript", "` / ~"));
    row += 1;
    p.move_to(row, left + 2);
    p.text(&format!("{:<8} back to the menu", "Q, ESC"));

    row += 2;
    p.color(th.accent, th.backdrop);
    p.move_to(row, left);
    p.text("in the menu");
    row += 1;
    p.color(th.text, th.backdrop);
    for (key, action) in [
        ("TAB", "next page on the carousel (systems + GAME ROOM)"),
        ("BKSP", "back to the page you came from (browser-style)"),
        ("L", "jump to the game room (the club's consoles)"),
        ("P", "your 2P port: open = others may join your game"),
        ("`", "GLOBAL CHAT (everyone connected; ESC closes)"),
        ("S", "settings: render / color / sound / screen / GG view / port"),
        ("A", "full-screen box art for the selected game"),
        ("a-z 0-9", "type to search the game list"),
        ("B C V", "quick toggles: render / color / GG view"),
        ("G", "cheat codes for the selected game (SMS/GG/Genesis)"),
    ] {
        p.move_to(row, left + 2);
        p.text(&format!("{key:<8} {action}"));
        row += 1;
    }

    row += 1;
    p.color(th.dim, th.backdrop);
    p.move_to(row, left);
    p.text("tip: SyncTERM sends true key press/release - held combos work");
    p.move_to(row + 1, left);
    p.text("best there. Plain telnet infers key-release from auto-repeat.");
    p.move_to(row + 3, left);
    p.color(th.accent2, th.backdrop);
    p.text("press any key to return");
    p.reset();
    painter.flush(term)?;

    loop {
        if let MenuEvent::Key(_) = input.wait_event_timeout(term, Duration::from_secs(30))? {
            return Ok(());
        }
    }
}

/// Cycle one settings row. Tri-state options run auto -> on -> off -> auto.
/// The render-mode carousel. Sixel is in the rotation ONLY while the terminal
/// advertises it (DA attribute 4) — and even then it is never auto-selected;
/// a caller reaches it exclusively by cycling past block/ascii themselves.
fn next_render_mode(cur: RenderMode, sixel_ok: bool) -> RenderMode {
    match cur {
        RenderMode::Block => RenderMode::Ascii,
        RenderMode::Ascii if sixel_ok => RenderMode::Sixel,
        RenderMode::Ascii => RenderMode::Block,
        RenderMode::Sixel => RenderMode::Block,
    }
}

fn cycle_setting(st: &mut MenuState, sel: usize, mp: &mut Option<&mut Multiplayer>) {
    match sel {
        0 => {
            st.mode = next_render_mode(st.mode, st.sixel_supported);
            st.cfg.render = Some(st.mode);
        }
        1 => {
            let cur = st.cfg.color.unwrap_or(ColorSetting::Auto);
            st.cfg.color = Some(cur.next());
        }
        2 => {
            st.cfg.sound_apc = match st.cfg.sound_apc {
                None => Some(true),
                Some(true) => Some(false),
                Some(false) => None,
            };
        }
        3 => {
            st.cfg.screen_best = match st.cfg.screen_best {
                None => Some(true),
                Some(true) => Some(false),
                Some(false) => None,
            };
        }
        4 => st.cfg.gg_full_frame = !st.cfg.gg_full_frame,
        5 => {
            st.cfg.gfx_wide = match st.cfg.gfx_wide {
                None => Some(false),
                Some(false) => Some(true),
                Some(true) => None,
            };
        }
        6 => {
            st.cfg.port_open = !st.cfg.port_open;
            if let Some(m) = mp.as_deref_mut() {
                m.set_status("menu", "", "", st.cfg.port_open);
            }
        }
        _ => {}
    }
}

/// Full-screen global settings page (S): every knob with what it does and
/// what "auto" detected, so shelf footers and sidebars don't have to carry
/// them (they were crowding out the box art).
fn show_settings_page(
    term: &mut dyn Term,
    input: &mut Input,
    st: &mut MenuState,
    mut mp: Option<&mut Multiplayer>,
) -> io::Result<()> {
    let th = &LOBBY_THEME; // a global page wears the club's phosphor green
    const N: usize = 7;
    let mut sel = 0usize;
    let mut painter = Painter::new(st.depth);
    let mut dirty = true;
    let mut first = true;

    loop {
        if dirty {
            painter.begin(st.cols, st.rows, first);
            first = false;
            let p = &mut painter;
            let left = st.cols.saturating_sub(64) / 2;
            let top = st.rows.saturating_sub(26) / 2;

            p.color(th.title, th.backdrop);
            p.move_to(top, left);
            p.text("SETTINGS");
            p.color(th.dim, th.backdrop);
            p.move_to(top, left + 10);
            p.text("global - they apply on every machine");

            let depth_label = match st.depth {
                crate::color::ColorDepth::True => "truecolor",
                crate::color::ColorDepth::C256 => "256",
                crate::color::ColorDepth::C16 => "16",
            };
            let color_val = {
                let c = st.cfg.color.unwrap_or(ColorSetting::Auto);
                if c == ColorSetting::Auto {
                    format!("auto ({depth_label})")
                } else {
                    c.slug().to_string()
                }
            };
            let rows_data: [(&str, String, [&str; 2]); N] = [
                (
                    "render",
                    crate::config::render_slug(st.mode).into(),
                    if st.sixel_supported {
                        [
                            "block: half-block cells. ascii: pure text art.",
                            "sixel: real pixel graphics (your terminal supports it).",
                        ]
                    } else {
                        [
                            "block: colored half-block cells - the sharpest picture.",
                            "ascii: pure text art, for terminals without block glyphs.",
                        ]
                    },
                ),
                (
                    "color",
                    color_val,
                    [
                        "auto probes the terminal for truecolor / 256 / 16 colors.",
                        "override only if the probe guesses wrong.",
                    ],
                ),
                (
                    "sound",
                    match st.cfg.sound_apc {
                        Some(true) => "on (APC)".into(),
                        Some(false) => "off".into(),
                        None => format!("auto ({})", if st.auto_sound { "on" } else { "off" }),
                    },
                    [
                        "streams game audio to the terminal as APC base64 PCM.",
                        "auto: on when a SyncTERM-class terminal is detected.",
                    ],
                ),
                (
                    "screen",
                    match st.cfg.screen_best {
                        Some(true) => "best".into(),
                        Some(false) => "as-is".into(),
                        None => format!("auto ({})", if st.auto_screen { "best" } else { "as-is" }),
                    },
                    [
                        "best: resize the terminal to each game's exact pixel size.",
                        "xterm-family honors it, BBS terminals ignore it. auto detects.",
                    ],
                ),
                (
                    "gg view",
                    (if st.cfg.gg_full_frame { "full frame" } else { "LCD window" }).into(),
                    [
                        "LCD: the Game Gear's real 160x144 screen window.",
                        "full: the whole 256x192 frame the hardware renders.",
                    ],
                ),
                (
                    "gfx aspect",
                    match st.cfg.gfx_wide {
                        None => "auto (TV 4:3)".into(),
                        Some(false) => "square px".into(),
                        Some(true) => "wide fill".into(),
                    },
                    [
                        "sixel shape. auto: the 4:3 a console TV showed - and on",
                        "CRT-corrected terminals (SyncTERM), pre-widened to match.",
                    ],
                ),
                (
                    "2P port",
                    (if st.cfg.port_open { "OPEN" } else { "closed" }).into(),
                    [
                        "open: any caller may join your game as P2 (the game resets).",
                        "closed: they knock; nothing happens until you let them in.",
                    ],
                ),
            ];
            for (i, (name, value, desc)) in rows_data.iter().enumerate() {
                let r = top + 2 + i as u16 * 3;
                p.move_to(r, left);
                if i == sel {
                    p.color(th.sel_fg, th.sel_bg);
                } else {
                    p.color(th.text, th.backdrop);
                }
                p.text(&format!(
                    "{} {:<8} < {:^18} >",
                    if i == sel { ">" } else { " " },
                    name,
                    clip(value, 18)
                ));
                p.color(th.dim, th.backdrop);
                for (j, d) in desc.iter().enumerate() {
                    p.move_to(r + 1 + j as u16, left + 4);
                    p.text(&clip(d, 62));
                }
            }
            p.move_to(top + 2 + N as u16 * 3, left);
            p.color(th.accent2, th.backdrop);
            p.text("Up/Dn choose   ENTER change   ESC done");
            p.reset();
            painter.flush(term)?;
            dirty = false;
        }
        if let MenuEvent::Key(key) = input.wait_event_timeout(term, Duration::from_millis(400))? {
            dirty = true;
            match key {
                Key::Esc | Key::Char('q') | Key::Char('Q') | Key::Char('s') | Key::Char('S') => {
                    break;
                }
                Key::Up => sel = sel.checked_sub(1).unwrap_or(N - 1),
                Key::Down => sel = (sel + 1) % N,
                Key::Enter | Key::Right | Key::Left | Key::Char(' ') => {
                    cycle_setting(st, sel, &mut mp);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Full-screen box art for the selected game ('A'): the 80-column way to see
/// the artwork properly — ANSI half-block cells over the whole screen, any
/// key returns.
fn show_art_page(
    term: &mut dyn Term,
    input: &mut Input,
    st: &MenuState,
    rom: &std::path::Path,
) -> io::Result<()> {
    let _ = term.write_all(b"\x1b[2J");
    let drew = {
        let mut w = crate::cp437::Cp437Writer::new(&mut *term);
        let drew = st
            .art
            .borrow_mut()
            .draw_fullscreen(&mut w, rom, st.cols, st.rows.saturating_sub(2), st.depth)
            .unwrap_or(false);
        w.flush()?;
        drew
    };
    let name = friendly_name(rom);
    let title = if drew {
        format!("{name}  -  any key returns")
    } else {
        format!("no art for {name}  -  any key returns")
    };
    let col = st.cols.saturating_sub(title.chars().count() as u16) / 2 + 1;
    let _ = write!(term, "\x1b[{};{}H\x1b[1;37m{}\x1b[0m", st.rows, col, title);
    term.flush()?;
    loop {
        if let MenuEvent::Key(_) = input.wait_event_timeout(term, Duration::from_secs(30))? {
            return Ok(());
        }
    }
}

const GG_MAX_SLOTS: usize = 4;

/// Full-screen Game Genie code editor for one ROM (modeled on lameboy's
/// show_game_genie_entry): N slots, hex typing with auto-grouping, Space
/// toggles a completed code, Enter/Esc saves and returns.
fn show_game_genie_page(
    term: &mut dyn Term,
    input: &mut Input,
    st: &MenuState,
    rom: &std::path::Path,
) -> io::Result<()> {
    use crate::gamegenie::{self, GameGenieCode};
    let rom_file = rom.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let user = st.user.clone().unwrap_or_default();
    let mut codes = gamegenie::load_codes(&user, rom_file);
    codes.truncate(GG_MAX_SLOTS);
    let mut slots: Vec<GameGenieCode> = codes;
    while slots.len() < GG_MAX_SLOTS {
        slots.push(GameGenieCode { code: String::new(), enabled: false });
    }
    let mut cur = 0usize;
    let th = &st.sys().theme;
    // Genesis codes use the Game Genie letter alphabet / Action Replay hex
    // with explicit separators; SMS/GG codes are 6/9 hex digits auto-grouped.
    let genesis = st.sys().machine == Machine::Genesis;
    let complete =
        |code: &str| if genesis { gamegenie::genesis_is_valid(code) } else { gamegenie::is_complete(code) };
    let max_len = if genesis { 11 } else { 9 };
    let mut painter = Painter::new(st.depth);
    let mut dirty = true;
    let mut first = true;

    loop {
        if dirty {
            painter.begin(st.cols, st.rows, first);
            first = false;
            let p = &mut painter;
            let left = st.cols.saturating_sub(60) / 2;
            let top = st.rows.saturating_sub(14) / 2;
            p.color(th.title, th.backdrop);
            p.move_to(top, left);
            p.text(&format!("CHEAT CODES - {}", friendly_name(rom)));
            p.move_to(top + 1, left);
            p.color(th.dim, th.backdrop);
            p.text(if genesis {
                "codes: XXXX-XXXX (Game Genie) or XXXXXX:XXXX / AR (hex)"
            } else {
                "codes: XXX-XXX-XXX or XXX-XXX (hex)"
            });
            for (i, slot) in slots.iter().enumerate() {
                p.move_to(top + 3 + i as u16 * 2, left);
                let caret = if i == cur { ">" } else { " " };
                let check = if complete(&slot.code) {
                    if slot.enabled { "[*]" } else { "[ ]" }
                } else {
                    "   "
                };
                let shown = if genesis {
                    format!("{:<11}", slot.code)
                } else {
                    format!("{:<11}", gamegenie::format_grouped(&slot.code))
                };
                if i == cur {
                    p.color(th.sel_fg, th.sel_bg);
                } else {
                    p.color(th.text, th.backdrop);
                }
                p.text(&format!("{caret} {check} slot {}: {}_", i + 1, shown));
            }
            p.move_to(top + 12, left);
            p.color(th.dim, th.backdrop);
            p.text("type code   SPACE toggle   BKSP erase   ENTER/ESC done");
            p.reset();
            painter.flush(term)?;
            dirty = false;
        }
        match input.wait_event_timeout(term, Duration::from_millis(400))? {
            MenuEvent::Key(key) => {
                dirty = true;
                match key {
                    Key::Esc | Key::Enter => break,
                    Key::Up => cur = cur.checked_sub(1).unwrap_or(GG_MAX_SLOTS - 1),
                    Key::Down => cur = (cur + 1) % GG_MAX_SLOTS,
                    Key::Backspace => {
                        slots[cur].code.pop();
                        slots[cur].enabled = complete(&slots[cur].code);
                    }
                    Key::Char(' ') if !genesis => {
                        if complete(&slots[cur].code) {
                            slots[cur].enabled = !slots[cur].enabled;
                        }
                    }
                    // Genesis AR codes may contain a space separator; use TAB
                    // (BackTab-safe) or a full code + SPACE only when the
                    // code is already complete without it.
                    Key::Char(' ') if genesis => {
                        if complete(&slots[cur].code) {
                            slots[cur].enabled = !slots[cur].enabled;
                        } else if slots[cur].code.len() < max_len {
                            slots[cur].code.push(' ');
                        }
                    }
                    Key::Char(c)
                        if (genesis && (c.is_ascii_alphanumeric() || c == '-' || c == ':'))
                            || (!genesis && c.is_ascii_hexdigit()) =>
                    {
                        if slots[cur].code.len() < max_len {
                            slots[cur].code.push(c.to_ascii_uppercase());
                            slots[cur].enabled = complete(&slots[cur].code);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let keep: Vec<GameGenieCode> =
        slots.into_iter().filter(|s| !s.code.is_empty()).collect();
    gamegenie::save_codes(&user, rom_file, &keep);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys(ext: &str) -> &'static SystemDef {
        SYSTEMS.iter().find(|s| s.extensions.contains(&ext)).unwrap()
    }

    #[test]
    fn scan_finds_roms_in_subdirectories() {
        let root = std::env::temp_dir().join(format!("lamegear-scan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("nes/platformers")).unwrap();
        std::fs::create_dir_all(root.join(".saves/alice")).unwrap();
        std::fs::write(root.join("Alpha (U).nes"), b"x").unwrap();
        std::fs::write(root.join("nes/Bravo (U).nes"), b"x").unwrap();
        std::fs::write(root.join("nes/platformers/Charlie (U).nes"), b"x").unwrap();
        std::fs::write(root.join("nes/Delta (U).gg"), b"x").unwrap();
        std::fs::write(root.join(".saves/alice/Echo (U).nes"), b"x").unwrap();

        let names: Vec<String> =
            scan_roms(&root, sys("nes")).into_iter().map(|r| r.display).collect();
        assert_eq!(names, ["Alpha", "Bravo", "Charlie"]);

        let gg: Vec<String> =
            scan_roms(&root, sys("gg")).into_iter().map(|r| r.display).collect();
        assert_eq!(gg, ["Delta"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scan_survives_symlink_cycles() {
        let root = std::env::temp_dir().join(format!("lamegear-cycle-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("deep")).unwrap();
        std::fs::write(root.join("deep/Foxtrot (U).nes"), b"x").unwrap();
        std::os::unix::fs::symlink(&root, root.join("deep/loop")).unwrap();

        let names: Vec<String> =
            scan_roms(&root, sys("nes")).into_iter().map(|r| r.display).collect();
        assert_eq!(names, ["Foxtrot"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
