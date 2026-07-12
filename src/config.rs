//! Sysop ini (`lamegear.ini`) + per-user preferences, in lameboy's key=value
//! format (stored under `~/.config/lamegear/config-<user>`).

use crate::color::ColorSetting;
use crate::renderer::RenderMode;
use std::fs;
use std::path::PathBuf;

pub const DEFAULT_RENDER_FPS: f64 = 20.0;

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("lamegear"))
}

fn config_file(user: &str) -> Option<PathBuf> {
    let safe: String = user
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    config_dir().map(|d| d.join(format!("config-{safe}")))
}

/// Per-user preferences, persisted between calls.
#[derive(Clone)]
pub struct UserConfig {
    /// Index into systems::SYSTEMS last browsed.
    pub system: usize,
    /// Render mode. `Sixel` is honored only while the terminal advertises
    /// sixel support — on any other terminal the session falls back to block
    /// (the saved preference survives for the next sixel-capable call).
    pub render: Option<RenderMode>,
    pub color: Option<ColorSetting>,
    /// Game Gear viewport: false = LCD window (160x144), true = full frame.
    pub gg_full_frame: bool,
    /// Streamed APC audio. None = auto: on when a SyncTERM-class terminal
    /// (CTerm physical-keys protocol) is detected, else off.
    pub sound_apc: Option<bool>,
    /// Screen size "best": ask the terminal to resize itself to each game's
    /// pixel-perfect dimensions on launch (and back on exit). None = auto:
    /// on for kitty-class (xterm-family) terminals that honor the request;
    /// BBS terminals ignore it anyway.
    pub screen_best: Option<bool>,
    /// Sixel graphic aspect. None = auto: TV 4:3 for the console systems
    /// (that's what the original hardware put on a CRT — square framebuffer
    /// pixels are the distortion, not the correction), native for GBA
    /// (a real 3:2 square-pixel LCD). Some(false) = square pixels (the raw
    /// framebuffer shape), Some(true) = fill the whole canvas (for terminals
    /// that squeeze their display back to 4:3 themselves).
    pub gfx_wide: Option<bool>,
    /// 2P controller port: open = other callers may sit down as player 2
    /// without asking (the console power-cycles into the linked session).
    /// Closed (default) = they can only knock; nothing happens without an
    /// explicit accept.
    pub port_open: bool,
    pub last_game: Option<String>,
}

impl Default for UserConfig {
    fn default() -> Self {
        UserConfig {
            system: 0,
            render: None,
            color: None,
            gg_full_frame: false,
            sound_apc: None,
            screen_best: None,
            gfx_wide: None,
            port_open: false,
            last_game: None,
        }
    }
}

impl UserConfig {
    pub fn load(user: &str) -> UserConfig {
        let mut cfg = UserConfig::default();
        let Some(path) = config_file(user) else { return cfg };
        let Ok(text) = fs::read_to_string(path) else { return cfg };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            match (key.trim(), value.trim()) {
                ("system", v) => cfg.system = v.parse().unwrap_or(0),
                ("render", v) => {
                    cfg.render = Some(match v.to_ascii_lowercase().as_str() {
                        "ascii" => RenderMode::Ascii,
                        "sixel" => RenderMode::Sixel,
                        _ => RenderMode::Block, // legacy files wrote "block"
                    })
                }
                ("color", v) => cfg.color = ColorSetting::parse(v),
                ("gg_full_frame", v) => cfg.gg_full_frame = v == "1" || v == "true",
                // Legacy files wrote "off"/"auto" for the old bool defaults;
                // "auto" now means auto-detect (the new default), an explicit
                // "off"/"asis" stays an explicit off.
                ("sound", v) => {
                    cfg.sound_apc = match v.to_ascii_lowercase().as_str() {
                        "apc" | "on" => Some(true),
                        "off" => Some(false),
                        _ => None,
                    }
                }
                ("screen", v) => {
                    cfg.screen_best = match v.to_ascii_lowercase().as_str() {
                        "best" => Some(true),
                        "asis" | "off" => Some(false),
                        _ => None,
                    }
                }
                ("aspect", v) => {
                    cfg.gfx_wide = match v.to_ascii_lowercase().as_str() {
                        "wide" | "fill" => Some(true),
                        "square" => Some(false),
                        _ => None, // "auto" / "tv"
                    }
                }
                ("port", v) => cfg.port_open = v.eq_ignore_ascii_case("open"),
                ("last_game", v) if !v.is_empty() => cfg.last_game = Some(v.to_string()),
                _ => {}
            }
        }
        cfg
    }

    pub fn save(&self, user: &str) {
        let Some(path) = config_file(user) else { return };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let mut text = String::new();
        text.push_str(&format!("system={}\n", self.system));
        if let Some(mode) = self.render {
            text.push_str(&format!("render={}\n", render_slug(mode)));
        }
        if let Some(color) = self.color {
            text.push_str(&format!("color={}\n", color.slug()));
        }
        text.push_str(&format!("gg_full_frame={}\n", if self.gg_full_frame { 1 } else { 0 }));
        text.push_str(&format!(
            "sound={}\n",
            match self.sound_apc {
                Some(true) => "apc",
                Some(false) => "off",
                None => "auto",
            }
        ));
        text.push_str(&format!(
            "screen={}\n",
            match self.screen_best {
                Some(true) => "best",
                Some(false) => "asis",
                None => "auto",
            }
        ));
        text.push_str(&format!(
            "aspect={}\n",
            match self.gfx_wide {
                Some(true) => "wide",
                Some(false) => "square",
                None => "auto",
            }
        ));
        text.push_str(&format!("port={}\n", if self.port_open { "open" } else { "closed" }));
        if let Some(game) = &self.last_game {
            text.push_str(&format!("last_game={game}\n"));
        }
        let _ = fs::write(path, text);
    }
}

/// The config-file / settings-page name of a render mode.
pub fn render_slug(mode: RenderMode) -> &'static str {
    match mode {
        RenderMode::Block => "block",
        RenderMode::Ascii => "ascii",
        RenderMode::Sixel => "sixel",
    }
}

/// Sysop-level door configuration.
pub struct DoorIni {
    pub roms_dir: Option<String>,
    pub fps: Option<f64>,
    pub color: Option<ColorSetting>,
    pub default_mode: Option<RenderMode>,
    /// Link (lobby/lockstep relay) server, `host:port`. None = single-player.
    pub link_server: Option<String>,
    /// Attract mode: after `attract_idle_secs` of menu idle, run a game demo
    /// for `attract_game_secs`, then return to the menu. Off by default.
    pub attract: bool,
    pub attract_idle_secs: u64,
    pub attract_game_secs: u64,
    /// Genesis page enable. OFF by default per the design spec: a Genesis
    /// session (68000 + Z80 + VDP + YM2612) costs roughly an order of
    /// magnitude more CPU per caller than SMS — budget concurrent callers
    /// before enabling.
    pub genesis: bool,
    /// SNES page enable (65816 + SPC700 + PPU: comparable cost to Genesis).
    pub snes: bool,
    /// GBA page enable. Commercial games additionally need a real 16KB BIOS
    /// (`gba_bios`); without one only BIOS-call-free homebrew runs.
    pub gba: bool,
    /// Path to the GBA BIOS image (default: gba_bios.bin beside the binary).
    pub gba_bios: Option<String>,
    /// PC Engine / TurboGrafx-16 page enable (single-player; HuCard only).
    pub pce: bool,
    /// Per-system console cap overrides for the game room, by system slug
    /// (`consoles = md:1, sfc:1, gg:4`). Systems not listed use
    /// `default_console_cap`. Only enforced while the link server is up —
    /// offline callers can't see the roster.
    pub consoles: Vec<(String, usize)>,
}

impl Default for DoorIni {
    fn default() -> Self {
        DoorIni {
            roms_dir: None,
            fps: None,
            color: None,
            default_mode: None,
            link_server: None,
            attract: false,
            attract_idle_secs: 30,
            attract_game_secs: 90,
            genesis: false,
            snes: false,
            gba: false,
            gba_bios: None,
            pce: false,
            consoles: Vec::new(),
        }
    }
}

impl DoorIni {
    /// The game room's console cap for one system (how many machines of this
    /// kind the "club" owns). Ini override first, then the defaults: one each
    /// of the heavy 16/32-bit cores, a shelf of the cheap 8-bit ones.
    pub fn console_cap(&self, system_id: &str) -> usize {
        if let Some((_, n)) = self.consoles.iter().find(|(id, _)| id == system_id) {
            return *n;
        }
        default_console_cap(system_id)
    }
}

/// Built-in cap when the ini doesn't name a system. A "console" is a session
/// (solo or linked); note each *linked* session actually runs one emulator
/// per participating door process, so a cap of 1 still allows 2 instances.
pub fn default_console_cap(system_id: &str) -> usize {
    match system_id {
        "md" | "sfc" | "gba" => 1,
        "pce" => 2,
        _ => 3, // gg / sms / sg / nes: cheap 8-bit cores
    }
}

/// Load `lamegear.ini` from the CWD, then beside the executable.
pub fn load_door_ini() -> DoorIni {
    let mut candidates = vec![PathBuf::from("lamegear.ini")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("lamegear.ini"));
        }
    }
    for path in candidates {
        if let Ok(text) = fs::read_to_string(&path) {
            return parse_door_ini(&text);
        }
    }
    DoorIni::default()
}

fn parse_door_ini(text: &str) -> DoorIni {
    let mut ini = DoorIni::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        match (key.trim(), value.trim()) {
            ("roms", v) if !v.is_empty() => ini.roms_dir = Some(v.to_string()),
            ("fps", v) => ini.fps = v.parse::<f64>().ok().map(|f| f.clamp(5.0, 60.0)),
            ("color", v) => ini.color = ColorSetting::parse(v),
            ("render", v) => {
                ini.default_mode = Some(if v.eq_ignore_ascii_case("ascii") {
                    RenderMode::Ascii
                } else {
                    RenderMode::Block
                });
            }
            ("link_server" | "link" | "link_relay", v) if !v.is_empty() => {
                ini.link_server = Some(v.to_string());
            }
            ("attract", v) => ini.attract = v == "1" || v.eq_ignore_ascii_case("true"),
            ("attract_idle_secs", v) => {
                if let Ok(n) = v.parse::<u64>() {
                    ini.attract_idle_secs = n.clamp(10, 3600);
                }
            }
            ("attract_game_secs", v) => {
                if let Ok(n) = v.parse::<u64>() {
                    ini.attract_game_secs = n.clamp(15, 3600);
                }
            }
            ("genesis", v) => ini.genesis = v == "1" || v.eq_ignore_ascii_case("true"),
            ("snes", v) => ini.snes = v == "1" || v.eq_ignore_ascii_case("true"),
            ("gba", v) => ini.gba = v == "1" || v.eq_ignore_ascii_case("true"),
            ("gba_bios", v) if !v.is_empty() => ini.gba_bios = Some(v.to_string()),
            ("pce", v) => ini.pce = v == "1" || v.eq_ignore_ascii_case("true"),
            ("consoles", v) => {
                ini.consoles = v
                    .split(',')
                    .filter_map(|pair| {
                        let (id, n) = pair.split_once(':')?;
                        Some((id.trim().to_lowercase(), n.trim().parse().ok()?))
                    })
                    .collect();
            }
            _ => {}
        }
    }
    ini
}

/// `<rom_dir>/.saves/<user>/<rom_stem>` — extensionless base handed to the
/// emulator's save writer (it appends `.sav` for SRAM).
pub fn save_base(rom_path: &std::path::Path, user: Option<&str>) -> Option<PathBuf> {
    let dir = rom_path.parent()?;
    let stem = rom_path.file_stem()?;
    let mut saves = dir.join(".saves");
    if let Some(u) = user {
        let safe: String = u
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        saves = saves.join(safe);
    }
    Some(saves.join(stem))
}
