//! LameGear+ — a multi-system Sega BBS door (Game Gear / Master System /
//! SG-1000), sibling to lameboy. Same door architecture: DOOR32.SYS inherited
//! socket, CP437 half-block rendering, cursor-report resize probing, transmit
//! fps cap with congestion skipping; different emulation backend (vendored
//! jgenesis smsgg-core).

#[macro_use]
mod out;
mod apc_audio;
mod art;
mod color;
mod config;
mod cp437;
mod door32;
mod emu;
mod framebuffer;
mod gamegenie;
mod input;
mod keys;
mod lockstep;
mod menu;
mod multiplayer;
mod renderer;
mod selftest;
mod splash;
mod systems;
mod term;

use color::ColorSetting;
use config::{save_base, UserConfig, DEFAULT_RENDER_FPS};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use emu::{DoorInputs, Emu, EmuOptions, Machine};
use framebuffer::FrameBuffer;
use input::{button_index, evdev_to_button, map_key_to_button, BUTTON_COUNT};
use keys::{Input, KeyboardMode};
use renderer::{RenderConfig, RenderMode, Renderer};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use systems::SYSTEMS;
use term::Term;

fn print_usage() {
    eprintln!("LameGear+ - Sega Game Gear / Master System / SG-1000 BBS door");
    eprintln!();
    eprintln!("Usage: lamegear [options] [rom]");
    eprintln!();
    eprintln!("  --dropfile <path>   DOOR32.SYS (file or its directory)");
    eprintln!("  --user <id>         per-user save/pref key (default: from dropfile)");
    eprintln!("  --roms <dir>        ROM directory (default: roms/ beside the binary)");
    eprintln!("  --fps <n>           transmit frame-rate cap, 5-60 (default 20)");
    eprintln!("  --color <mode>      auto|truecolor|256|16");
    eprintln!("  --block / --ascii   render mode override");
    eprintln!("  --mute              disable APC streamed audio globally");
    eprintln!("  --link <host:port>  lobby/netplay relay server (see link-server/)");
    eprintln!("  --handle <name>     lobby display name (default: dropfile alias)");
    eprintln!("  --selftest <rom> [frames]      determinism harness (headless)");
    eprintln!("  --golden <rom> [frame]         framebuffer CRC at frame N (headless)");
    eprintln!("  --replay <rom> <file.lgr>      verify a recorded netplay session");
    eprintln!("  --dump <rom> <frames> <out>    run headless, write final frame as PPM");
    eprintln!();
    eprintln!("Menu:    TAB=switch system/lobby  B=render  C=color  S=sound  V=gg-view");
    eprintln!("         G=game genie  type to find  ENTER=play  Q=quit");
    eprintln!("In game: arrows=d-pad  Z/X=buttons 1/2 (NES A/B, Genesis A/B)");
    eprintln!("         Genesis 6-btn: Z/X/V=A/B/C A/S/C=X/Y/Z (SNES-aligned)");
    eprintln!("         SPACE=select/mode  ENTER=start/pause");
    eprintln!("         5=save state  8=load state  Q/Esc=quit");
}

/// Ask the terminal for its size: park the cursor at the far corner and
/// request a cursor-position report. A door's pty size is frozen at launch
/// (an inherited socket has no winsize at all), so this round-trip is the only
/// way to track the caller's real terminal. `with_caps` folds the keyboard /
/// color / sixel capability queries into the same burst (see lameboy).
pub(crate) fn send_size_probe<W: Write + ?Sized>(term: &mut W, with_caps: bool) -> io::Result<()> {
    emit!(term, MoveTo(9998, 9998))?;
    // 14t = xterm text-area pixels; ?2;1;0S = XTSMGRAPHICS sixel geometry —
    // CTerm's only pixel-area report (it doesn't speak 14t/16t).
    term.write_all(b"\x1b[6n\x1b[=3n\x1b[16t\x1b[14t\x1b[?2;1;0S")?;
    if with_caps {
        term.write_all(b"\x1b[<c\x1b[?u\x1b[38;2;1;2;3m\x1bP$qm\x1b\\\x1b[0m\x1b[c")?;
    }
    term.flush()
}

fn enable_physical_keys<W: Write + ?Sized>(term: &mut W) -> io::Result<()> {
    term.write_all(b"\x1b[=1h\x1b[=2h")?;
    term.flush()
}

fn disable_physical_keys<W: Write + ?Sized>(term: &mut W) -> io::Result<()> {
    term.write_all(b"\x1b[=1l\x1b[=2l")?;
    term.flush()
}

fn enable_kitty_keys<W: Write + ?Sized>(term: &mut W) -> io::Result<()> {
    term.write_all(b"\x1b[>10u")?;
    term.flush()
}

fn disable_kitty_keys<W: Write + ?Sized>(term: &mut W) -> io::Result<()> {
    term.write_all(b"\x1b[<u")?;
    term.flush()
}

/// Ask the terminal to resize its text area to `rows` x `cols` (xterm
/// `CSI 8 ; rows ; cols t`). xterm-family terminals honor it; SyncTERM/CTerm
/// ignore it harmlessly (their `CSI ... t` is 24-bit colour and needs 4
/// params). Ported from lameboy.
fn resize_terminal<W: Write + ?Sized>(term: &mut W, rows: u16, cols: u16) -> io::Result<()> {
    write!(term, "\x1b[8;{};{}t", rows, cols)?;
    term.flush()
}

/// Append one line of sixel fit geometry to `sixel-debug.log` beside the
/// binary: every report the terminal gave and what the fit decided —
/// diagnosing a caller's terminal (SyncTERM et al) without seeing their
/// screen. Truncated when it grows past 64KB.
fn log_sixel_geometry(tag: &str, renderer: &Renderer, input: &Input, cols: u16, rows: u16) {
    let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf()))
    else {
        return;
    };
    let path = dir.join("sixel-debug.log");
    let fresh = std::fs::metadata(&path).map(|m| m.len() > 65536).unwrap_or(false);
    let line = format!(
        "[{tag}] probe={cols}x{rows} cell={:?} 14t={:?} gfx={:?} | {}\n",
        input.cell_pixels(),
        input.text_area_pixels(),
        input.gfx_geometry(),
        renderer.debug_geometry(),
    );
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    if fresh {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    if let Ok(mut f) = opts.create(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// The terminal size at which this machine renders pixel-perfect: for the
/// cell modes, native width columns x half the native height in rows (1 cell
/// = 1x2 pixels); for sixel, the size whose PIXEL canvas lands exactly on
/// the biggest crisp integer scale the renderer's caps allow — anything
/// larger only buys letterbox bars around a capped graphic. Plus the status
/// row. Genesis is sized for H40 (its wider mode) so mode switches stay 1:1.
fn optimal_terminal(
    machine: Machine,
    mode: RenderMode,
    cell_pixels: Option<(u16, u16)>,
) -> (u16, u16) {
    let (w, h): (usize, usize) = match machine {
        Machine::GameGear => (160, 144),
        Machine::Genesis => (320, 224),
        Machine::Nes | Machine::Snes | Machine::Pce => (256, 224),
        Machine::Gba => (240, 160),
        _ => (256, 192), // SMS / SG-1000 / GG expanded
    };
    if mode == RenderMode::Sixel {
        let (cell_h, cell_w) = cell_pixels.unwrap_or((16, 8));
        let (cell_w, cell_h) = (cell_w.max(1) as usize, cell_h.max(1) as usize);
        let k = renderer::SIXEL_GAME_MAX_SCALE
            .min(renderer::SIXEL_GAME_MAX_SIDE / w.max(h))
            .max(1);
        // +2: the status row and the spacer row above it (see refit_sixel).
        return ((w * k).div_ceil(cell_w) as u16, ((h * k).div_ceil(cell_h) + 2) as u16);
    }
    (w as u16, (h / 2 + 1) as u16)
}

fn parse_value(args: &[String], flag: &str) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if a == flag {
            return args.get(i + 1).cloned();
        }
        if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
            return Some(v.to_string());
        }
    }
    None
}

/// Congestion pacing (from lameboy): charge each transmit's write+flush time;
/// when a write takes much longer than the render budget the socket is backed
/// up, so skip upcoming transmit slots proportionally. Emulation never skips.
struct LinkPace {
    budget: Duration,
    skip: u32,
}

impl LinkPace {
    fn new(render_interval: Duration) -> Self {
        LinkPace { budget: render_interval, skip: 0 }
    }
    fn note(&mut self, write_time: Duration) {
        if write_time > self.budget * 2 {
            let over = (write_time.as_millis() / self.budget.as_millis().max(1)) as u32;
            self.skip = (self.skip + over).min(40); // cap ~2s at 20fps
        } else if self.skip > 0 {
            self.skip -= 1;
        }
    }
    fn skip_frame(&mut self) -> bool {
        if self.skip > 0 {
            self.skip -= 1;
            true
        } else {
            false
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return;
    }

    // Headless harness modes run before any terminal setup.
    if let Some(rom) = parse_value(&args, "--selftest") {
        let frames = args
            .iter()
            .skip_while(|a| *a != "--selftest")
            .nth(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600);
        if let Err(e) = selftest::run_selftest(Path::new(&rom), frames) {
            eprintln!("FAIL: {e}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(rom) = parse_value(&args, "--selftest-link") {
        let frames = args
            .iter()
            .skip_while(|a| *a != "--selftest-link")
            .nth(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600);
        if let Err(e) = selftest::run_selftest_link(Path::new(&rom), frames) {
            eprintln!("FAIL: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.iter().any(|a| a == "--port-trace") {
        let pos: Vec<&String> =
            args.iter().skip_while(|a| *a != "--port-trace").skip(1).take(2).collect();
        if pos.is_empty() {
            eprintln!("usage: lamegear --port-trace <rom> [frames]");
            std::process::exit(2);
        }
        let frames = pos.get(1).and_then(|s| s.parse().ok()).unwrap_or(3600);
        if let Err(e) = selftest::run_port_trace(Path::new(pos[0]), frames) {
            eprintln!("FAIL: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.iter().any(|a| a == "--golden") {
        let pos: Vec<&String> =
            args.iter().skip_while(|a| *a != "--golden").skip(1).take(2).collect();
        if pos.is_empty() {
            eprintln!("usage: lamegear --golden <rom> [frame]");
            std::process::exit(2);
        }
        let frame = pos.get(1).and_then(|s| s.parse().ok()).unwrap_or(300);
        if let Err(e) = selftest::golden_frame(Path::new(pos[0]), frame) {
            eprintln!("FAIL: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.iter().any(|a| a == "--replay") {
        let pos: Vec<&String> =
            args.iter().skip_while(|a| *a != "--replay").skip(1).take(2).collect();
        if pos.len() != 2 {
            eprintln!("usage: lamegear --replay <rom> <file.lgr>");
            std::process::exit(2);
        }
        if let Err(e) = selftest::run_replay(Path::new(pos[0]), Path::new(pos[1])) {
            eprintln!("FAIL: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.iter().any(|a| a == "--replay-dump") {
        let pos: Vec<&String> =
            args.iter().skip_while(|a| *a != "--replay-dump").skip(1).take(4).collect();
        if pos.len() < 3 {
            eprintln!("usage: lamegear --replay-dump <rom> <file.lgr> <outdir> [every]");
            std::process::exit(2);
        }
        let every = pos.get(3).and_then(|s| s.parse().ok()).unwrap_or(300);
        if let Err(e) = selftest::run_replay_dump(
            Path::new(pos[0]),
            Path::new(pos[1]),
            Path::new(pos[2]),
            every,
        ) {
            eprintln!("FAIL: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.iter().any(|a| a == "--dump") {
        let pos: Vec<&String> =
            args.iter().skip_while(|a| *a != "--dump").skip(1).take(3).collect();
        if pos.len() != 3 {
            eprintln!("usage: lamegear --dump <rom> <frames> <out.ppm>");
            std::process::exit(2);
        }
        let frames = pos[1].parse().unwrap_or(120);
        if let Err(e) = selftest::dump_ppm(Path::new(pos[0]), frames, Path::new(pos[2])) {
            eprintln!("FAIL: {e}");
            std::process::exit(1);
        }
        return;
    }

    let ini = config::load_door_ini();

    let door = parse_value(&args, "--dropfile").and_then(|p| door32::read(Path::new(&p)));
    let user = parse_value(&args, "--user").or_else(|| door.as_ref().and_then(|d| d.user_key()));

    let roms_dir = parse_value(&args, "--roms")
        .or(ini.roms_dir.clone())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|p| p.join("roms")))
                .unwrap_or_else(|| PathBuf::from("roms"))
        });

    let render_fps = parse_value(&args, "--fps")
        .and_then(|v| v.parse::<f64>().ok())
        .map(|f| f.clamp(5.0, 60.0))
        .or(ini.fps)
        .unwrap_or(DEFAULT_RENDER_FPS);

    let cli_color = parse_value(&args, "--color").and_then(|v| ColorSetting::parse(&v));
    let cli_mode = if args.iter().any(|a| a == "--ascii") {
        Some(RenderMode::Ascii)
    } else if args.iter().any(|a| a == "--block") {
        Some(RenderMode::Block)
    } else if args.iter().any(|a| a == "--sixel") {
        // Dev/test override: forces sixel without the DA capability gate the
        // settings path applies. A terminal that can't decode DCS shows mush.
        Some(RenderMode::Sixel)
    } else {
        None
    };
    let force_mute = args.iter().any(|a| a == "--mute");
    let link_addr = parse_value(&args, "--link").or(ini.link_server.clone());
    let handle = parse_value(&args, "--handle")
        .or_else(|| door.as_ref().map(|d| d.display_name()))
        .filter(|h| !h.is_empty());

    // First non-flag arg = positional ROM (value-taking flags skipped).
    let value_flags =
        ["--user", "--fps", "--dropfile", "--roms", "--color", "--keylog", "--link", "--handle"];
    let mut positional_rom: Option<PathBuf> = None;
    let mut skip_next = false;
    for a in &args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if value_flags.contains(&a.as_str()) {
            skip_next = true;
            continue;
        }
        if !a.starts_with('-') && positional_rom.is_none() {
            positional_rom = Some(PathBuf::from(a));
        }
    }

    let mut term = match term::open(door.as_ref()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("lamegear: cannot open terminal: {e}");
            std::process::exit(1);
        }
    };
    let mut input = Input::new();
    if let Some(path) = parse_value(&args, "--keylog") {
        input.enable_keylog(&path);
    }

    let _ = emit!(term, EnterAlternateScreen, Hide, Clear(ClearType::All));
    let _ = term.write_all(b"\x1b[?7l"); // autowrap off for the whole session
    let _ = term.flush();

    let result = run_session(
        &mut *term,
        &mut input,
        user.as_deref(),
        &roms_dir,
        render_fps,
        cli_color.or(ini.color),
        cli_mode.or(ini.default_mode),
        positional_rom,
        link_addr.as_deref(),
        handle.as_deref(),
        force_mute,
        ini.attract.then_some(ini.attract_idle_secs),
        ini.attract_game_secs,
        {
            // Per-system sysop gates (heavy cores default off).
            let mut enabled = Vec::new();
            for (i, s) in SYSTEMS.iter().enumerate() {
                let on = match s.machine {
                    Machine::Genesis => ini.genesis,
                    Machine::Snes => ini.snes,
                    Machine::Gba => ini.gba,
                    Machine::Pce => ini.pce,
                    _ => true,
                };
                if on {
                    enabled.push(i);
                }
            }
            enabled
        },
        // Game-room console caps, one per SYSTEMS entry (ini `consoles = ...`
        // overrides; heavy cores default to a single machine).
        SYSTEMS.iter().map(|s| ini.console_cap(s.id)).collect(),
        {
            // GBA BIOS: ini path, else gba_bios.bin beside the binary / CWD.
            let mut candidates: Vec<PathBuf> =
                ini.gba_bios.iter().map(PathBuf::from).collect();
            candidates.push(PathBuf::from("gba_bios.bin"));
            if let Ok(exe) = std::env::current_exe() {
                if let Some(dir) = exe.parent() {
                    candidates.push(dir.join("gba_bios.bin"));
                }
            }
            candidates.iter().find_map(|p| std::fs::read(p).ok()).and_then(|b| {
                if b.len() == 16 * 1024 {
                    Some(b)
                } else {
                    log::warn!("gba bios has wrong size {} (want 16384); ignoring", b.len());
                    None
                }
            })
        },
    );

    let _ = disable_physical_keys(&mut *term);
    let _ = disable_kitty_keys(&mut *term);
    let _ = term.write_all(b"\x1b[?7h");
    let _ = emit!(term, Show, LeaveAlternateScreen);
    let _ = term.flush();

    if let Err(e) = result {
        eprintln!("lamegear: {e}");
    }
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    term: &mut dyn Term,
    input: &mut Input,
    user: Option<&str>,
    roms_dir: &Path,
    render_fps: f64,
    color_setting: Option<ColorSetting>,
    mode_override: Option<RenderMode>,
    positional_rom: Option<PathBuf>,
    link_addr: Option<&str>,
    handle: Option<&str>,
    force_mute: bool,
    attract_idle: Option<u64>,
    attract_game_secs: u64,
    enabled_systems: Vec<usize>,
    console_caps: Vec<usize>,
    gba_bios: Option<Vec<u8>>,
) -> io::Result<()> {
    // Probe terminal size + capabilities; give the reply a moment to arrive.
    send_size_probe(term, true)?;
    let (mut cols, mut rows) = (80u16, 24u16);
    let deadline = Instant::now() + Duration::from_millis(700);
    while Instant::now() < deadline {
        let _ = input.poll(term)?;
        if let Some((r, c)) = input.take_cursor() {
            rows = r;
            cols = c;
            if input.caps_resolved() {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }

    let mut cfg = user.map(UserConfig::load).unwrap_or_default();
    let setting = color_setting.or(cfg.color).unwrap_or(ColorSetting::Auto);

    // Title card: dismissed by any key or a 10s timeout; also finishes the
    // capability probing started above.
    if positional_rom.is_none() {
        let _ = splash::show_splash(term, input, setting);
    }
    let depth = setting.resolve(input.color_probe());
    let mut mode = mode_override.unwrap_or(cfg.render.unwrap_or(RenderMode::Block));
    // A saved sixel preference only holds on a terminal that (still)
    // advertises sixel; anywhere else this session runs block. The saved
    // preference is kept — it re-applies on the next sixel-capable call.
    if mode == RenderMode::Sixel && mode_override.is_none() && !input.sixel_supported() {
        mode = RenderMode::Block;
    }

    // Direct ROM launch: no menu.
    if let Some(rom_path) = positional_rom {
        let machine = rom_path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Machine::from_extension)
            .unwrap_or(Machine::MasterSystem);
        let sys_index = SYSTEMS.iter().position(|s| s.machine == machine).unwrap_or(0);
        return run_game(
            term,
            input,
            GameParams {
                rom_path: &rom_path,
                machine,
                sys_index,
                roms_dir,
                depth,
                mode,
                render_fps,
                user,
                cols,
                rows,
                apc_enabled: !force_mute
                    && cfg
                        .sound_apc
                        .unwrap_or(input.keyboard_mode() == KeyboardMode::CtermPhysical),
                attract_secs: None,
                gba_bios: gba_bios.clone(),
                resize_best: cfg
                    .screen_best
                    .unwrap_or(input.keyboard_mode() == KeyboardMode::Kitty),
                gfx_wide: cfg.gfx_wide,
            },
            None,
            None,
        )
        .map(|_| ());
    }

    // Lobby connection: best-effort — the door is fully usable without it.
    let mut mp = link_addr.and_then(|addr| {
        let name = handle.or(user).unwrap_or("caller");
        match multiplayer::Multiplayer::connect(addr, name) {
            Ok(m) => Some(m),
            Err(e) => {
                log::warn!("link server {addr}: {e}");
                None
            }
        }
    });

    let mut state =
        menu::MenuState::new(roms_dir.to_path_buf(), cfg.clone(), depth, mode, cols, rows);
    state.user = user.map(String::from);
    state.attract_idle_secs = attract_idle;
    // Multiplayer is configured but unreachable: the game room still opens
    // (machines are solo-only); say why nobody's on the floor.
    if link_addr.is_some() && mp.is_none() {
        state.set_notice("link server offline - machines are solo-only");
    }
    state.set_enabled_systems(enabled_systems);
    state.set_console_caps(console_caps);
    loop {
        // Auto-detected defaults for "auto" settings: SyncTERM-class (CTerm
        // physical keys) terminals handle APC audio; kitty-class terminals
        // honor screen-size requests. Resolved lazily — probes land async.
        state.auto_sound = input.keyboard_mode() == KeyboardMode::CtermPhysical;
        state.auto_screen = input.keyboard_mode() == KeyboardMode::Kitty;
        let Some(mut choice) = menu::show_menu(term, input, &mut state, mp.as_mut())? else {
            break;
        };
        cfg = state.cfg.clone();
        if let Some(u) = user {
            cfg.save(u);
        }
        // Re-resolve depth in case the caller cycled the color setting.
        let depth = cfg.color.unwrap_or(setting).resolve(input.color_probe());
        state.depth = depth;
        let (cols, rows) = state.term_size();
        // One menu pick = one console lifetime, which can span several boots:
        // an attract demo taken over relaunches for real, and a linked peer
        // unplugging power-cycles the same cartridge solo (P2 -> P1).
        let mut linked = choice.linked.take();
        loop {
            if let Some(m) = mp.as_mut() {
                // Attract demos are nobody at the machine: don't advertise
                // them as occupied consoles (or ghost machines fill the
                // game room every time the menu idles).
                if !choice.attract {
                    m.set_status(
                        "game",
                        &menu::friendly_rom_name(&choice.rom_path),
                        SYSTEMS[choice.system_index].id,
                        state.cfg.port_open,
                    );
                }
            }
            let sound_on = cfg.sound_apc.unwrap_or(state.auto_sound);
            let screen_on = cfg.screen_best.unwrap_or(state.auto_screen);
            let params = GameParams {
                rom_path: &choice.rom_path,
                machine: choice.machine,
                sys_index: choice.system_index,
                roms_dir,
                depth,
                mode: state.mode,
                render_fps,
                user,
                cols,
                rows,
                apc_enabled: !force_mute && sound_on,
                attract_secs: choice.attract.then_some(attract_game_secs),
                gba_bios: gba_bios.clone(),
                // Attract demos never resize the caller's terminal.
                resize_best: screen_on && !choice.attract,
                gfx_wide: cfg.gfx_wide,
            };
            let exit = match (&linked, mp.as_mut()) {
                (Some(l), Some(m)) => run_game(
                    term,
                    input,
                    params,
                    Some(LinkCtx { mp: m, peer: l.peer.clone(), initiator: l.initiator }),
                    None,
                )?,
                _ => {
                    let lobby = mp
                        .as_mut()
                        .filter(|_| !choice.attract)
                        .map(|m| LobbyCtx { mp: m, port_open: &mut state.cfg.port_open });
                    run_game(term, input, params, None, lobby)?
                }
            };
            match exit {
                GameExit::Menu => {
                    // Standing up from a real session lands back on this
                    // machine's shelf; attract demos return to the room.
                    if !choice.attract {
                        state.resume_shelf = Some(choice.system_index);
                    }
                    break;
                }
                // Attract take-over: the caller pressed a game key during the
                // demo — relaunch as a real session (fresh boot, their saves).
                GameExit::TakeOver => {
                    choice.attract = false;
                    linked = None;
                }
                // The peer unplugged mid-session: same cartridge, solo, we
                // keep the console as P1.
                GameExit::PeerLeft => {
                    linked = None;
                }
                // Someone sat down at our open port (or we let a knock in):
                // confirm READY and let the menu's countdown flow resolve
                // LINK_OPEN into the linked launch.
                GameExit::LinkStart { peer, game } => {
                    if let Some(m) = mp.as_mut() {
                        m.ready();
                    }
                    state.resume_link = Some((peer, game));
                    break;
                }
            }
        }
        if let Some(m) = mp.as_mut() {
            if state.resume_link.is_none() {
                m.set_status("menu", "", "", state.cfg.port_open);
            }
        }
    }
    if let Some(m) = mp.as_mut() {
        m.abort();
    }
    Ok(())
}

/// How a game session ended — decides what run_session does next.
enum GameExit {
    /// Normal quit (or attract demo finished): back to the menu.
    Menu,
    /// Attract demo taken over: relaunch this ROM as a real session.
    TakeOver,
    /// The server started a link while we were playing (someone sat down at
    /// our open 2P port, or we accepted a knock): send READY and let the
    /// menu's countdown flow launch the linked session.
    LinkStart { peer: String, game: String },
    /// Linked session over because the other player unplugged: power-cycle
    /// the same cartridge solo — the survivor keeps the console as P1.
    PeerLeft,
}

/// Lobby connection for a SOLO game: keeps presence pumped so knocks and
/// open-port JOINs reach the player mid-game (see the '2'/'0' keys).
struct LobbyCtx<'a> {
    mp: &'a mut multiplayer::Multiplayer,
    /// Live 2P-port flag; '2' toggles it and the change outlives the game.
    port_open: &'a mut bool,
}

struct GameParams<'a> {
    rom_path: &'a Path,
    machine: Machine,
    sys_index: usize,
    /// ROM library root, for resolving a knock's game while in-game.
    roms_dir: &'a Path,
    depth: color::ColorDepth,
    mode: RenderMode,
    render_fps: f64,
    user: Option<&'a str>,
    cols: u16,
    rows: u16,
    apc_enabled: bool,
    /// Some(secs) = attract demo: unattended, any key exits, auto-quits.
    attract_secs: Option<u64>,
    /// GBA BIOS image, when the sysop supplied one.
    gba_bios: Option<Vec<u8>>,
    /// "Screen: best" — ask the terminal to resize to this machine's
    /// pixel-perfect dimensions for the game, restoring afterwards.
    resize_best: bool,
    /// Sixel aspect setting (UserConfig::gfx_wide): None = auto.
    gfx_wide: Option<bool>,
}

/// A live netplay session for this game: relay client + role.
struct LinkCtx<'a> {
    mp: &'a mut multiplayer::Multiplayer,
    peer: String,
    initiator: bool,
}

/// Session shape for a ROM: the sysop's port-trace sweep cache
/// (`<roms>/.link-shapes`, lines of `file<TAB>shape`) decides; otherwise
/// Game Gear ROMs default to Gear-to-Gear (each player at least plays their
/// own machine) and everything else to shared-console P2.
fn shape_for_rom(rom_path: &Path) -> multiplayer::SessionShape {
    use multiplayer::SessionShape;
    let file = rom_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if let Some(dir) = rom_path.parent() {
        if let Ok(text) = std::fs::read_to_string(dir.join(".link-shapes")) {
            for line in text.lines() {
                if let Some((name, shape)) = line.split_once('\t') {
                    if name == file {
                        return match shape.trim() {
                            "gear-to-gear" => SessionShape::GearToGear,
                            _ => SessionShape::SharedConsole,
                        };
                    }
                }
            }
        }
    }
    let is_gg = rom_path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("gg"));
    if is_gg { SessionShape::GearToGear } else { SessionShape::SharedConsole }
}

/// The sysop sweep's raw classification for a ROM ("single-player",
/// "shared-console", "gear-to-gear"), if `<roms>/.link-shapes` has one.
/// Lets the lobby warn when someone challenges with a game that never reads
/// player 2's pad.
pub(crate) fn link_class_for_rom(rom_path: &Path) -> Option<String> {
    let file = rom_path.file_name().and_then(|n| n.to_str())?;
    let text = std::fs::read_to_string(rom_path.parent()?.join(".link-shapes")).ok()?;
    text.lines()
        .find_map(|l| l.split_once('\t').filter(|(n, _)| *n == file))
        .map(|(_, shape)| shape.trim().to_string())
}

/// Session handshake (after LINK_OPEN): the initiator measures RTT, derives
/// the input delay D = ceil(rtt/frame)+1, and proposes it with its ROM
/// SHA-256 and session shape; the acceptor verifies the hash byte-for-byte
/// (contract §4.3.7: identical ROMs or no session) and acks. Returns the
/// primed Lockstep and the agreed shape.
fn link_handshake(
    term: &mut dyn Term,
    input: &mut Input,
    link: &mut LinkCtx<'_>,
    rom: &[u8],
    frame_ms: f64,
    proposed_shape: multiplayer::SessionShape,
) -> io::Result<Option<(lockstep::Lockstep, multiplayer::SessionShape)>> {
    use sha2::{Digest, Sha256};
    let sha: [u8; 32] = Sha256::digest(rom).into();

    let _ = write!(
        term,
        "\x1b[2J\x1b[1;1H\r\n  establishing link with {} ...\r\n",
        link.peer
    );
    let _ = term.flush();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pings: Vec<(u32, Instant)> = Vec::new();
    let mut rtts: Vec<f64> = Vec::new();
    let mut sent_hello = false;
    // The delay we proposed in Hello. The acceptor builds its Lockstep from
    // the Hello, so we MUST build ours from the same number — recomputing
    // from rtts at Accept time can disagree (a pong that lands between Hello
    // and Accept lowers the min), and mismatched delays deadlock the session.
    let mut hello_delay: Option<u8> = None;

    // Pings are STAGGERED (next one sent when the previous pong returns):
    // the first RTT includes however long the peer takes to reach this loop
    // after LINK_OPEN (menu latency, ROM load), so it can read hundreds of ms
    // even on localhost. Later pings measure the true path; the min wins.
    const PING_ROUNDS: u32 = 4;
    if link.initiator {
        link.mp.send_session_ping(1);
        pings.push((1, Instant::now()));
    }

    loop {
        if Instant::now() > deadline {
            link.mp.send_session_end("handshake timeout");
            link.mp.abort();
            return Ok(None);
        }
        // Allow the caller to bail out of a hung handshake.
        for key in input.poll(term)? {
            if matches!(key, keys::Key::Esc | keys::Key::Char('q') | keys::Key::Char('Q')) {
                link.mp.send_session_end("peer quit");
                link.mp.abort();
                return Ok(None);
            }
        }
        link.mp.pump();
        if !link.mp.is_alive() {
            return Ok(None);
        }
        while let Some(ev) = link.mp.take_event() {
            if matches!(ev, multiplayer::Event::LinkEnded { .. }) {
                return Ok(None);
            }
        }
        while let Some(msg) = link.mp.take_session() {
            use multiplayer::SessionMsg::*;
            match msg {
                Ping { token } => link.mp.send_session_pong(token),
                Pong { token } => {
                    if let Some(&(_, at)) = pings.iter().find(|(t, _)| *t == token) {
                        rtts.push(at.elapsed().as_secs_f64() * 1000.0);
                        let next = token + 1;
                        if next <= PING_ROUNDS && !pings.iter().any(|(t, _)| *t == next) {
                            link.mp.send_session_ping(next);
                            pings.push((next, Instant::now()));
                        }
                    }
                }
                Hello { rom_sha, delay, shape, .. } => {
                    // Acceptor path: the initiator's shape decision wins.
                    if rom_sha != sha {
                        link.mp.send_session_end("rom mismatch");
                        link.mp.abort();
                        let _ = write!(term, "\r\n  ROM differs from {}'s copy - aborted\r\n", link.peer);
                        let _ = term.flush();
                        std::thread::sleep(Duration::from_secs(2));
                        return Ok(None);
                    }
                    link.mp.send_session_accept(&sha);
                    return Ok(Some((lockstep::Lockstep::new(1, delay), shape)));
                }
                Accept { rom_sha } => {
                    // Initiator path.
                    if rom_sha != sha {
                        link.mp.send_session_end("rom mismatch");
                        link.mp.abort();
                        return Ok(None);
                    }
                    // Same delay the acceptor got in our Hello — never recompute.
                    let delay = hello_delay.unwrap_or_else(|| pings_delay(&rtts, frame_ms));
                    return Ok(Some((lockstep::Lockstep::new(0, delay), proposed_shape)));
                }
                End { reason } => {
                    let _ = write!(term, "\r\n  link ended: {reason}\r\n");
                    let _ = term.flush();
                    std::thread::sleep(Duration::from_secs(2));
                    return Ok(None);
                }
                _ => {}
            }
        }
        // Initiator: once RTT is measured (or probing timed out), propose.
        // Requiring 3 rounds means at least two post-warmup samples inform
        // the min (round 1 is polluted by the peer's arrival latency).
        if link.initiator
            && !sent_hello
            && (rtts.len() >= 3 || pings[0].1.elapsed() > Duration::from_secs(2))
        {
            let delay = pings_delay(&rtts, frame_ms);
            link.mp.send_session_hello(&sha, 0 /* NTSC */, delay, proposed_shape);
            hello_delay = Some(delay);
            sent_hello = true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// D from the best measured RTT; a conservative default when probing failed.
fn pings_delay(rtts: &[f64], frame_ms: f64) -> u8 {
    match rtts.iter().copied().reduce(f64::min) {
        Some(best) => lockstep::Lockstep::delay_for_rtt(best, frame_ms),
        None => 6,
    }
}

/// How an attract demo ended: the caller either wants the menu back or —
/// arcade style — pressed a game key to take over and PLAY this game.
#[derive(PartialEq)]
enum AttractExit {
    Menu,
    TakeOver,
}

/// Classify attract-mode keystrokes: quit keys end the demo to the menu,
/// anything else is "let me play". Release edges are ignored (stale key-ups
/// from the keystroke that started the demo).
fn attract_key(keys: &[keys::Key], edges: &[keys::KeyEdge]) -> Option<AttractExit> {
    use keys::Key;
    for k in keys {
        return Some(match k {
            Key::Esc | Key::Char('q') | Key::Char('Q') => AttractExit::Menu,
            _ => AttractExit::TakeOver,
        });
    }
    for e in edges.iter().filter(|e| e.pressed) {
        return Some(if e.code == input::EVDEV_ESC || e.code == input::EVDEV_Q {
            AttractExit::Menu
        } else {
            AttractExit::TakeOver
        });
    }
    None
}

/// Full-screen box-art title card shown before an attract demo. Returns how
/// to proceed: run the demo (None), or a key already decided the exit.
fn attract_splash(
    term: &mut dyn Term,
    input: &mut Input,
    rom_path: &Path,
    sys_index: usize,
    depth: color::ColorDepth,
    cols: u16,
    rows: u16,
) -> io::Result<Option<AttractExit>> {
    let _ = term.write_all(b"\x1b[2J");
    let mut art = art::ArtRenderer::new();
    let drew = {
        let mut w = cp437::Cp437Writer::new(&mut *term);
        let drew = art
            .draw_fullscreen(&mut w, rom_path, cols, rows.saturating_sub(3), depth)
            .unwrap_or(false);
        w.flush()?;
        drew
    };
    let name = rom_path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
    let name = name.split('(').next().unwrap_or(name).trim();
    let title = format!("{}  -  {}", name, SYSTEMS[sys_index].name);
    let hint = "* ATTRACT MODE *  any key: play this game   Q/ESC: menu";
    let center = |s: &str| cols.saturating_sub(s.chars().count() as u16) / 2 + 1;
    let _ = write!(term, "\x1b[{};{}H\x1b[1;37m{}", rows - 1, center(&title), title);
    let _ = write!(term, "\x1b[{};{}H\x1b[0;36m{}\x1b[0m", rows, center(hint), hint);
    term.flush()?;
    // Without art there's nothing to look at: shorten the card.
    let deadline =
        Instant::now() + if drew { Duration::from_millis(3500) } else { Duration::from_millis(1200) };
    while Instant::now() < deadline {
        let keys = input.poll(term)?;
        let edges = input.take_key_edges();
        if let Some(exit) = attract_key(&keys, &edges) {
            return Ok(Some(exit));
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    Ok(None)
}

/// Kill/restore the raw key-edge protocols around chat compose: while typing,
/// the compose line needs plain characters, not physical-key events.
fn chat_typing_protocols(term: &mut dyn Term, input: &mut Input, typing: bool) {
    match input.keyboard_mode() {
        KeyboardMode::CtermPhysical => {
            if typing {
                let _ = disable_physical_keys(term);
            } else {
                let _ = enable_physical_keys(term);
            }
        }
        KeyboardMode::Kitty => {
            if typing {
                let _ = disable_kitty_keys(term);
                input.set_kitty_active(false);
            } else {
                let _ = enable_kitty_keys(term);
                input.set_kitty_active(true);
            }
        }
        KeyboardMode::Legacy => {}
    }
}

/// What one keypress does to the in-game chat compose line.
enum ComposeAct {
    Stay,
    Cancel,          // ESC: draft dropped, back to the game
    Stash,           // '`': draft kept for the next compose
    Send(String),    // ENTER with a message
    Command(String), // ENTER with a /command line
    History,         // '~' opens the transcript overlay
}

fn compose_key(key: &keys::Key, buf: &mut String) -> ComposeAct {
    use keys::Key;
    match key {
        Key::Esc => ComposeAct::Cancel,
        Key::Char('`') => ComposeAct::Stash,
        Key::Char('~') => ComposeAct::History,
        Key::Enter => {
            let text = buf.trim().to_string();
            buf.clear();
            match text.strip_prefix('/') {
                Some(cmd) => ComposeAct::Command(cmd.to_string()),
                None if text.is_empty() => ComposeAct::Cancel,
                None => ComposeAct::Send(text),
            }
        }
        Key::Backspace => {
            buf.pop();
            ComposeAct::Stay
        }
        Key::Char(c)
            if *c != '`' && *c != '~' && (c.is_ascii_graphic() || *c == ' ') && buf.len() < 180 =>
        {
            buf.push(*c);
            ComposeAct::Stay
        }
        _ => ComposeAct::Stay,
    }
}

/// The in-game compose line, drawn where the status bar lives (spectre
/// convention: "` text_" black-on-cyan).
fn draw_chat_compose(
    term: &mut dyn Term,
    renderer: &Renderer,
    depth: color::ColorDepth,
    buf: &str,
) -> io::Result<()> {
    let row = renderer.fps_row() + 1;
    let width = renderer.term_cols().max(40) as usize;
    let prompt = format!("` {buf}_  (ENTER send ∙ ` stash ∙ ESC cancel ∙ ~ history)");
    let mut clipped: String = if buf.len() + 4 > width {
        // Long drafts win the space over the hint; keep the tail visible.
        let tail: String = format!("` {buf}_");
        tail.chars().skip(tail.chars().count().saturating_sub(width)).collect()
    } else {
        prompt.chars().take(width).collect()
    };
    while clipped.chars().count() < width {
        clipped.push(' ');
    }
    let sgr = color::cell_sgr(depth, 0, 0, 0, 0, 170, 170);
    let mut buf_out = Vec::with_capacity(width + 24);
    let _ = write!(buf_out, "\x1b[{};1H\x1b[{}m{}\x1b[0m", row, sgr, clipped);
    let mut w = cp437::Cp437Writer::new(&mut *term);
    w.write_all(&buf_out)?;
    w.flush()
}

/// The in-game chat transcript overlay ('~' or /history): a centered box over
/// the game. The game keeps running underneath (a linked session must);
/// closing forces a full repaint.
fn draw_chat_overlay(
    term: &mut dyn Term,
    cols: u16,
    rows: u16,
    depth: color::ColorDepth,
    title: &str,
    lines: &[String],
) -> io::Result<()> {
    let bw = (cols as usize).saturating_sub(8).clamp(30, 72);
    let bh = (rows as usize).saturating_sub(4).clamp(8, 18);
    let bx = (cols as usize - bw) / 2 + 1; // 1-based
    let by = (rows as usize - bh) / 2 + 1;
    let inner = bw - 2;
    let frame = color::cell_sgr(depth, 80, 220, 120, 8, 24, 12);
    let text = color::cell_sgr(depth, 200, 235, 205, 8, 24, 12);
    let titles = color::cell_sgr(depth, 8, 24, 12, 80, 220, 120);
    let mut out = Vec::with_capacity(bw * bh + 256);
    let _ = write!(out, "\x1b[{};{}H\x1b[{}m┌{}┐", by, bx, frame, "─".repeat(inner));
    for r in 1..bh - 1 {
        let _ = write!(out, "\x1b[{};{}H\x1b[{}m│\x1b[{}m{:w$}\x1b[{}m│", by + r, bx, frame, text, "", frame, w = inner);
    }
    let _ = write!(out, "\x1b[{};{}H\x1b[{}m└{}┘", by + bh - 1, bx, frame, "─".repeat(inner));
    let t: String = title.chars().take(inner - 2).collect();
    let _ = write!(out, "\x1b[{};{}H\x1b[{}m {} ", by, bx + 2, titles, t);
    let space = bh - 2;
    let shown = if lines.len() > space { &lines[lines.len() - space..] } else { lines };
    if shown.is_empty() {
        let _ = write!(out, "\x1b[{};{}H\x1b[{}m (nothing said yet - ` composes)", by + 1, bx + 1, text);
    }
    for (i, l) in shown.iter().enumerate() {
        let l: String = l.chars().take(inner - 1).collect();
        let _ = write!(out, "\x1b[{};{}H\x1b[{}m {}", by + 1 + i, bx + 1, text, l);
    }
    out.extend_from_slice(b"\x1b[0m");
    let mut w = cp437::Cp437Writer::new(&mut *term);
    w.write_all(&out)?;
    w.flush()
}

/// The in-game controller-port keys. `let_in` is the '2' key: accept the
/// pending knock, or with none pending toggle the 2P port. '0' declines the
/// knock. Returns the status-bar flash to show.
fn port_key(
    let_in: bool,
    lobby: &mut LobbyCtx<'_>,
    two_player: bool,
    roms_dir: &Path,
    status_game: &str,
    sys_id: &str,
) -> Option<String> {
    if let Some(inc) = lobby.mp.incoming().first().cloned() {
        if !let_in {
            lobby.mp.reject(&inc.from);
            return Some(format!("declined {}", inc.from));
        }
        if menu::find_rom_in_dir(roms_dir, &inc.game).is_none() {
            lobby.mp.reject(&inc.from);
            return Some(format!("you don't have '{}' - declined", inc.game));
        }
        // LINK_START comes back via the pump and exits to the countdown.
        lobby.mp.accept(&inc.from);
        return Some(format!("letting {} in ...", inc.from));
    }
    if !let_in {
        return None;
    }
    if !two_player {
        return Some("single-player system - no second port".into());
    }
    *lobby.port_open = !*lobby.port_open;
    lobby.mp.set_status("game", status_game, sys_id, *lobby.port_open);
    Some(if *lobby.port_open {
        "2P port OPEN - a joiner resets the game".into()
    } else {
        "2P port closed".into()
    })
}

/// Runs one game session; the exit says whether the menu, a relaunch, or a
/// link countdown comes next. `link` = a live netplay session, `lobby` = a
/// solo game with the lobby connection kept warm; never both.
fn run_game(
    term: &mut dyn Term,
    input: &mut Input,
    params: GameParams<'_>,
    mut link: Option<LinkCtx<'_>>,
    mut lobby: Option<LobbyCtx<'_>>,
) -> io::Result<GameExit> {
    let GameParams {
        rom_path,
        machine,
        sys_index,
        roms_dir,
        depth,
        mode,
        render_fps,
        user,
        cols,
        rows,
        apc_enabled,
        attract_secs,
        gba_bios,
        resize_best,
        gfx_wide,
    } = params;
    let linked = link.is_some();
    let attract = attract_secs.is_some();
    let sys_id = SYSTEMS[sys_index].id;
    let status_game = menu::friendly_rom_name(rom_path);
    let joinable = systems::two_player(machine);

    let rom = match std::fs::read(rom_path) {
        Ok(r) => r,
        Err(e) => {
            let _ = write!(term, "\x1b[2J\x1b[1;1HCannot read {}: {e}\r\n", rom_path.display());
            let _ = term.flush();
            std::thread::sleep(Duration::from_secs(2));
            return Ok(GameExit::Menu);
        }
    };

    let rom_file = rom_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    // Netplay contract: fresh boot, empty SRAM, and no cheats (a cheat only
    // one side applies is a guaranteed desync). Attract demos are volatile
    // too — an unattended demo must never touch a caller's saves.
    let opts = if linked || attract {
        EmuOptions { save_base: None, gba_bios: gba_bios.clone(), ..EmuOptions::default() }
    } else {
        let codes = gamegenie::load_codes(user.unwrap_or(""), rom_file);
        EmuOptions {
            save_base: save_base(rom_path, user),
            cheat_codes: gamegenie::to_core(&codes),
            genesis_cheats: if machine == Machine::Genesis {
                gamegenie::to_genesis(&codes)
            } else {
                Vec::new()
            },
            gba_bios: gba_bios.clone(),
            ..EmuOptions::default()
        }
    };
    // Lockstep handshake happens before any terminal-mode changes so an
    // aborted link drops straight back to the menu — and before the emulator
    // is constructed, because the negotiated shape decides what to build
    // (shared console vs. two cabled Game Gears).
    let mut ls = None;
    let mut shape = multiplayer::SessionShape::SharedConsole;
    if let Some(l) = link.as_mut() {
        // Delay negotiation only needs an approximate frame time (all cores
        // run ~60Hz NTSC in netplay); the precise pacing clock comes from
        // the constructed emulator below.
        let est_frame_ms = 1000.0 / 59.92;
        match link_handshake(term, input, l, &rom, est_frame_ms, shape_for_rom(rom_path))? {
            Some((session, sh)) => {
                ls = Some(session);
                shape = sh;
            }
            None => return Ok(GameExit::Menu),
        }
    }

    // Attract demos open on a box-art title card; a key there already
    // decides where we're headed.
    if attract {
        match attract_splash(term, input, rom_path, sys_index, depth, cols, rows)? {
            Some(AttractExit::Menu) => return Ok(GameExit::Menu),
            Some(AttractExit::TakeOver) => return Ok(GameExit::TakeOver),
            None => {}
        }
        let _ = term.write_all(b"\x1b[2J");
    }
    // The demo window starts after the title card.
    let attract_deadline = attract_secs.map(|s| Instant::now() + Duration::from_secs(s));

    let built = if linked && shape == multiplayer::SessionShape::GearToGear {
        let slot = ls.as_ref().map(|s| s.local_slot).unwrap_or(0);
        Emu::new_gg_link(rom.clone(), slot, &opts)
    } else {
        Emu::new(rom.clone(), machine, opts)
    };
    let mut emu = match built {
        Ok(e) => e,
        Err(e) => {
            let _ = write!(term, "\x1b[2J\x1b[1;1HCannot load {}: {e}\r\n", rom_path.display());
            let _ = term.flush();
            std::thread::sleep(Duration::from_secs(2));
            return Ok(GameExit::Menu);
        }
    };
    let frame_duration = Duration::from_secs_f64(1.0 / emu.target_fps());

    // Keyboard protocol: CTerm physical keys or kitty edges when available.
    match input.keyboard_mode() {
        KeyboardMode::CtermPhysical => {
            let _ = enable_physical_keys(&mut *term);
        }
        KeyboardMode::Kitty => {
            let _ = enable_kitty_keys(&mut *term);
            input.set_kitty_active(true);
        }
        KeyboardMode::Legacy => {}
    }

    let (init_w, init_h) = match machine {
        Machine::GameGear => (160, 144),
        Machine::Genesis => (320, 224), // H40; refits live on H32/H40 switches
        Machine::Nes | Machine::Snes | Machine::Pce => (256, 224),
        Machine::Gba => (240, 160),
        _ => (256, 192),
    };
    let mut fb = FrameBuffer::new(init_w, init_h);
    let mut renderer = Renderer::new(
        RenderConfig { mode, depth, cell_pixels: input.cell_pixels() },
        init_w,
        init_h,
    );
    renderer.update_dimensions(cols, rows);
    renderer.set_text_area_px(input.pixel_area(cols, rows));
    // Sixel aspect: auto = the shape the original hardware displayed — 4:3
    // for the TV consoles (square framebuffer pixels are the distortion),
    // 3:2 for GBA (a genuine square-pixel LCD). On CTerm-class terminals
    // the target is computed THROUGH their 4:3 display correction: SyncTERM
    // shows the whole pixel canvas at ~4:3 whatever its shape, so a
    // geometrically-correct image displays squeezed unless pre-widened.
    renderer.set_sixel_aspect(match gfx_wide {
        Some(true) => renderer::SixelAspect::FillCanvas,
        Some(false) => renderer::SixelAspect::SquarePx,
        None => {
            let r = if machine == Machine::Gba { 1.5 } else { 4.0 / 3.0 };
            if input.keyboard_mode() == KeyboardMode::CtermPhysical {
                renderer::SixelAspect::CrtDisplay(r)
            } else if machine == Machine::Gba {
                renderer::SixelAspect::SquarePx
            } else {
                renderer::SixelAspect::Display(r)
            }
        }
    });
    if mode == RenderMode::Sixel {
        // A visible cursor hops to the graphic origin and back every frame —
        // reads as flicker. The cell modes never move it far enough to care.
        let _ = term.write_all(b"\x1b[?25l");
        log_sixel_geometry("entry", &renderer, input, cols, rows);
    }

    // "Screen: best": ask an xterm-family terminal to grow to this machine's
    // pixel-perfect size (different per system — GG wants 160x73, Genesis
    // H40 wants 320x113). The follow-up probe reads back whatever the
    // terminal actually did; BBS terminals ignore the request entirely.
    // Effective cell size for the resize request: the text-area pixel report
    // divided by the current grid outranks the reported font size (they can
    // disagree — mode/font switches, hidpi).
    let eff_cell = input
        .pixel_area(cols, rows)
        .map(|(ah, aw)| (ah / rows.max(1), aw / cols.max(1)))
        .filter(|&(h, w)| h >= 4 && w >= 4)
        .or(input.cell_pixels());
    let (opt_cols, opt_rows) = optimal_terminal(machine, mode, eff_cell);
    let mut cur_size = (cols, rows);
    if resize_best && (cols, rows) != (opt_cols, opt_rows) {
        let _ = resize_terminal(&mut *term, opt_rows, opt_cols);
        let _ = send_size_probe(&mut *term, false);
        if mode == RenderMode::Sixel {
            // Hold the first frame until the post-resize geometry lands:
            // frames fitted to the OLD size overflow the resized screen
            // (sixel wider = cropped, taller = scrolled to the bottom).
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                let _ = input.poll(term)?;
                if let Some((r, c)) = input.take_cursor() {
                    cur_size = (c, r);
                    renderer.update_dimensions(c, r);
                    renderer.set_cell_pixels(input.cell_pixels());
                    renderer.set_text_area_px(input.pixel_area(c, r));
                    log_sixel_geometry("post-resize", &renderer, input, c, r);
                    break;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        }
    }

    let render_interval = Duration::from_secs_f64(1.0 / render_fps);
    let mut pace = LinkPace::new(render_interval);

    // APC streamed audio (spec: no local audio device; sound goes to the
    // caller's terminal as base64 PCM). See apc_audio.rs for the protocol.
    let audio_clock = Instant::now();
    let tuning = apc_audio::ApcTuning::DEFAULT.sanitized();
    let mut apc = apc_enabled.then(|| apc_audio::ApcAudio::new(tuning.chunk_ms, tuning.rate));
    emu.audio.enabled = apc.is_some();
    let resync_interval =
        (tuning.resync_secs != 0).then(|| Duration::from_secs(tuning.resync_secs as u64));
    let mut resync_timer = Instant::now();

    // Telnet has no key-up: a held button is re-asserted by auto-repeat, so
    // release anything not seen within the timeout.
    let mut held = [false; BUTTON_COUNT];
    let mut last_seen: [Option<Instant>; BUTTON_COUNT] = [None; BUTTON_COUNT];
    let mut edge_driven = [false; BUTTON_COUNT];
    let button_timeout = Duration::from_millis(150);

    let mut sim_deadline = Instant::now();
    let mut last_render = Instant::now() - render_interval;
    let mut last_probe = Instant::now();
    let mut last_keyframe = Instant::now();
    let mut status_dirty = true;
    let state_path = (!linked && !attract)
        .then(|| save_base(rom_path, user).map(|b| b.with_extension("state")))
        .flatten();
    let mut note: BarNote = None;
    let mut end_reason: Option<String> = None;
    // Rolling transmitted-frames counter for the status bar's FPS readout.
    let mut fps_frames: u32 = 0;
    let mut fps_val: f32 = 0.0;
    let mut fps_tick = Instant::now();
    // Linked sessions record their executed input stream: an input stream is
    // a few KB per session and replays the whole game (spec 4.4.5).
    let mut replay_log: Vec<(u16, u16)> = Vec::new();

    // ---- in-game chat (spectre conventions: ` compose / stash, ESC cancel,
    // ~ or /history transcript). Linked games talk over the console's own
    // cable (session relay, peers only); solo games talk on GLOBAL chat.
    let chat_capable = !attract && (linked || lobby.is_some());
    let mut chat_compose: Option<String> = None;
    let mut chat_draft = String::new();
    let mut chat_hist = false;
    let mut chat_hist_dirty = false;
    let mut session_chat: Vec<String> = Vec::new(); // "who: text" lines

    let mut exit = GameExit::Menu;
    'game: loop {
        // Attract demo: quit keys hand the door back to the menu, a game key
        // takes the game over for real (arcade style); the timer ends the
        // demo on its own.
        if attract {
            let keys = input.poll(term)?;
            let edges = input.take_key_edges();
            match attract_key(&keys, &edges) {
                Some(AttractExit::TakeOver) => {
                    exit = GameExit::TakeOver;
                    break 'game;
                }
                Some(AttractExit::Menu) => break 'game,
                None => {}
            }
            if attract_deadline.is_some_and(|d| Instant::now() >= d) {
                break 'game;
            }
        }
        // ---- input ----
        if !attract {
        for key in input.poll(term)? {
            use keys::Key;
            // Transcript overlay: any key dismisses it.
            if chat_hist {
                chat_hist = false;
                if mode == RenderMode::Sixel {
                    // The overlay text sits INSIDE the image region; on
                    // CTerm-class terminals it stays in the character
                    // buffer under the re-sent graphic and ghosts back.
                    renderer.request_ground_clear();
                } else {
                    renderer.request_repaint();
                }
                status_dirty = true;
                continue;
            }
            // Compose line swallows every key while open.
            if chat_compose.is_some() {
                let mut buf = chat_compose.take().unwrap();
                let act = compose_key(&key, &mut buf);
                match act {
                    ComposeAct::Stay => chat_compose = Some(buf),
                    ComposeAct::Cancel => {}
                    ComposeAct::Stash => chat_draft = buf,
                    ComposeAct::History => {
                        chat_draft = buf;
                        chat_hist = true;
                        chat_hist_dirty = true;
                    }
                    ComposeAct::Send(text) => {
                        if let Some(l) = link.as_mut() {
                            l.mp.send_session_chat(&text);
                            session_chat.push(format!("you: {text}"));
                            note = Some((BarKind::Chat, format!("` you: {text}"), Instant::now()));
                        } else if let Some(lb) = lobby.as_mut() {
                            // The server echo lands it in the transcript and
                            // flashes it back as delivery confirmation.
                            lb.mp.send_chat(&text);
                        }
                    }
                    ComposeAct::Command(cmd) => {
                        match cmd.trim().to_ascii_lowercase().as_str() {
                            "history" | "h" | "last" | "l" => {
                                chat_hist = true;
                                chat_hist_dirty = true;
                            }
                            _ => {
                                note = Some((
                                    BarKind::Notice,
                                    "commands: /history (/h, /last, /l)".into(),
                                    Instant::now(),
                                ));
                            }
                        }
                    }
                }
                if chat_compose.is_none() {
                    chat_typing_protocols(term, input, false);
                    renderer.request_repaint(); // wipe the compose row
                }
                status_dirty = true;
                continue;
            }
            match key {
                Key::Esc | Key::Char('q') | Key::Char('Q') => break 'game,
                Key::Char('`') if chat_capable => {
                    chat_compose = Some(std::mem::take(&mut chat_draft));
                    held = [false; BUTTON_COUNT]; // drop the pad while typing
                    chat_typing_protocols(term, input, true);
                    status_dirty = true;
                }
                Key::Char('~') if chat_capable => {
                    chat_hist = true;
                    chat_hist_dirty = true;
                }
                Key::Char('5') => {
                    if let Some(msg) = state_io(true, &state_path, &mut emu) {
                        note = Some((BarKind::Notice, msg, Instant::now()));
                        status_dirty = true;
                    }
                }
                Key::Char('8') => {
                    if let Some(msg) = state_io(false, &state_path, &mut emu) {
                        note = Some((BarKind::Notice, msg, Instant::now()));
                        status_dirty = true;
                    }
                }
                Key::Char(c @ ('2' | '0')) => {
                    if let Some(lb) = lobby.as_mut() {
                        if let Some(msg) =
                            port_key(c == '2', lb, joinable, roms_dir, &status_game, sys_id)
                        {
                            note = Some((BarKind::Notice, msg, Instant::now()));
                        }
                        status_dirty = true;
                    }
                }
                other => {
                    if let Some(b) = map_key_to_button(&other) {
                        let i = button_index(b);
                        if !edge_driven[i] {
                            held[i] = true;
                            last_seen[i] = Some(Instant::now());
                        }
                    }
                }
            }
        }
        for edge in input.take_key_edges() {
            if edge.code == input::EVDEV_ESC || edge.code == input::EVDEV_Q {
                if edge.pressed {
                    break 'game;
                }
                continue;
            }
            // Save-state keys arrive as edges too in the kitty/CTerm modes.
            if edge.code == input::EVDEV_5 || edge.code == input::EVDEV_8 {
                if edge.pressed {
                    if let Some(msg) = state_io(edge.code == input::EVDEV_5, &state_path, &mut emu) {
                        note = Some((BarKind::Notice, msg, Instant::now()));
                        status_dirty = true;
                    }
                }
                continue;
            }
            if edge.code == input::EVDEV_2 || edge.code == input::EVDEV_0 {
                if edge.pressed {
                    if let Some(lb) = lobby.as_mut() {
                        if let Some(msg) = port_key(
                            edge.code == input::EVDEV_2,
                            lb,
                            joinable,
                            roms_dir,
                            &status_game,
                            sys_id,
                        ) {
                            note = Some((BarKind::Notice, msg, Instant::now()));
                        }
                        status_dirty = true;
                    }
                }
                continue;
            }
            if edge.code == input::EVDEV_GRAVE {
                // Edge protocols can't type into the compose line, so this
                // only OPENS it — chat_typing_protocols then switches to
                // plain chars until the compose closes.
                if edge.pressed && chat_capable && chat_compose.is_none() && !chat_hist {
                    chat_compose = Some(std::mem::take(&mut chat_draft));
                    held = [false; BUTTON_COUNT];
                    chat_typing_protocols(term, input, true);
                    status_dirty = true;
                }
                continue;
            }
            if chat_compose.is_some() || chat_hist {
                continue; // stray queued edges must not press pad buttons
            }
            if let Some(b) = evdev_to_button(edge.code) {
                let i = button_index(b);
                edge_driven[i] = true;
                held[i] = edge.pressed;
            }
        }
        // Timeout-release for translated (no key-up) input.
        let now = Instant::now();
        for i in 0..BUTTON_COUNT {
            if held[i]
                && !edge_driven[i]
                && last_seen[i].map_or(true, |t| now.duration_since(t) > button_timeout)
            {
                held[i] = false;
            }
        }
        } // !attract

        // ---- lobby presence (solo games) ----
        // Keeps knocks and open-port JOINs live mid-game: a knock shows in
        // the status bar ('2' lets them in, '0' declines), and a LINK_START
        // (open-port join, or our accept round-tripping) exits to the
        // countdown so the console can power-cycle into the linked session.
        if let Some(lb) = lobby.as_mut() {
            lb.mp.pump();
            while let Some(ev) = lb.mp.take_event() {
                use multiplayer::Event::*;
                match ev {
                    ChallengeReceived { from, .. } => {
                        note = Some((
                            BarKind::Alert,
                            format!("{from} wants to play - 2=let in 0=decline"),
                            Instant::now(),
                        ));
                        status_dirty = true;
                    }
                    ChallengeCanceled => {
                        note = Some((BarKind::Notice, "knock withdrawn".into(), Instant::now()));
                        status_dirty = true;
                    }
                    LinkStarting { peer, game } => {
                        exit = GameExit::LinkStart { peer, game };
                    }
                    Chat { from, text } => {
                        // Global chat reaches a solo game inline, bottom row.
                        note = Some((BarKind::Chat, format!("` {from}: {text}"), Instant::now()));
                        status_dirty = true;
                        chat_hist_dirty = true;
                    }
                    Error { msg } => {
                        note = Some((BarKind::Alert, msg, Instant::now()));
                        status_dirty = true;
                    }
                    _ => {}
                }
            }
            if matches!(exit, GameExit::LinkStart { .. }) {
                break 'game;
            }
        }

        // ---- network (linked sessions) ----
        if let (Some(l), Some(session)) = (link.as_mut(), ls.as_mut()) {
            l.mp.pump();
            if !l.mp.is_alive() {
                end_reason = Some("connection lost".into());
            }
            while let Some(ev) = l.mp.take_event() {
                if let multiplayer::Event::LinkEnded { reason } = ev {
                    end_reason = Some(reason);
                }
            }
            while let Some(msg) = l.mp.take_session() {
                use multiplayer::SessionMsg::*;
                match msg {
                    Input { frame, slot, buttons } => {
                        session.push_input(lockstep::InputMsg { frame, slot, buttons });
                    }
                    Crc { frame, crc } => {
                        if let Some(d) = session.note_peer_crc(frame, crc) {
                            end_reason = Some(format!(
                                "DESYNC at frame {}: {:08x} vs {:08x}",
                                d.frame, d.ours, d.theirs
                            ));
                        }
                    }
                    Ping { token } => l.mp.send_session_pong(token),
                    End { reason } => end_reason = Some(reason),
                    Chat { text } => {
                        // Console-scoped: only this machine's players hear it.
                        session_chat.push(format!("{}: {}", l.peer, text));
                        if session_chat.len() > 50 {
                            session_chat.remove(0);
                        }
                        note = Some((BarKind::Chat, format!("` {}: {}", l.peer, text), Instant::now()));
                        status_dirty = true;
                        chat_hist_dirty = true;
                    }
                    _ => {}
                }
            }
            if end_reason.is_some() {
                break 'game;
            }
        }

        // ---- emulate (always full speed; render clock is separate) ----
        match (link.as_mut(), ls.as_mut()) {
            (Some(l), Some(session)) => {
                // Lockstep: the emulation clock is the network clock. Frame F
                // executes only when both slots' inputs for F are known; a
                // stall never guesses input, it just waits (contract §4.3.1).
                let mut catchup = 0;
                while Instant::now() >= sim_deadline && catchup < 12 {
                    let mask = lockstep::mask_from_held(&held);
                    let (to_send, step) = session.tick(mask);
                    if let Some(m) = to_send {
                        l.mp.send_session_input(m.frame, m.slot, m.buttons);
                    }
                    match step {
                        Some((p0, p1)) => {
                            replay_log.push((p0, p1));
                            emu.step_frame(&lockstep::masks_to_door_inputs(p0, p1));
                            if let Some(a) = apc.as_mut() {
                                a.push_samples(&emu.audio.samples);
                            }
                            emu.audio.samples.clear();
                            let f = session.current_frame().saturating_sub(1);
                            if f > 0 && lockstep::Lockstep::crc_due(f) {
                                if let Ok(crc) = emu.state_crc32() {
                                    l.mp.send_session_crc(f, crc);
                                    if let Some(d) = session.note_our_crc(f, crc) {
                                        end_reason = Some(format!(
                                            "DESYNC at frame {}: {:08x} vs {:08x}",
                                            d.frame, d.ours, d.theirs
                                        ));
                                    }
                                }
                            }
                            sim_deadline += frame_duration;
                            catchup += 1;
                        }
                        None => {
                            // Stalled on the peer: shed the timing debt so the
                            // resume doesn't burst-run frames.
                            sim_deadline = Instant::now() + frame_duration;
                            break;
                        }
                    }
                }
                if catchup == 12 {
                    sim_deadline = Instant::now();
                }
                if end_reason.is_some() {
                    break 'game;
                }
            }
            _ => {
                let mut catchup = 0;
                while Instant::now() >= sim_deadline && catchup < 12 {
                    emu.step_frame(&DoorInputs::solo(held));
                    if let Some(a) = apc.as_mut() {
                        a.push_samples(&emu.audio.samples);
                    }
                    emu.audio.samples.clear();
                    sim_deadline += frame_duration;
                    catchup += 1;
                }
                if catchup == 12 {
                    sim_deadline = Instant::now(); // fell too far behind: drop the debt
                }
            }
        }

        // ---- audio transmit ----
        let now_ms = audio_clock.elapsed().as_millis() as u64;
        if input.take_audio_drain().is_some() {
            if let Some(a) = apc.as_mut() {
                a.notify_drain(&mut *term, now_ms)?;
            }
        }
        if let (Some(a), Some(iv)) = (apc.as_mut(), resync_interval) {
            if resync_timer.elapsed() >= iv {
                resync_timer = Instant::now();
                a.resync(&mut *term, now_ms)?;
            }
        }
        if let Some(a) = apc.as_mut() {
            a.emit_ready(&mut *term, now_ms)?;
        }

        // ---- resize probe ----
        if let Some((r, c)) = input.take_cursor() {
            cur_size = (c, r);
            let before = renderer.image_rect();
            renderer.update_dimensions(c, r);
            renderer.set_cell_pixels(input.cell_pixels());
            renderer.set_text_area_px(input.pixel_area(c, r));
            if renderer.image_rect() != before {
                status_dirty = true;
                if mode == RenderMode::Sixel {
                    log_sixel_geometry("refit", &renderer, input, c, r);
                }
            }
        }
        if last_probe.elapsed() > Duration::from_secs(1) {
            let _ = send_size_probe(&mut *term, !input.caps_resolved());
            last_probe = Instant::now();
        }
        if last_keyframe.elapsed() > Duration::from_secs(7) {
            renderer.request_repaint();
            last_keyframe = Instant::now();
            status_dirty = true;
        }

        // ---- render (transmit-capped, congestion-skipped) ----
        if chat_hist {
            // The transcript overlay owns the screen. The game keeps
            // simulating (a linked session must), frames just don't
            // transmit; closing forces a full repaint.
            if chat_hist_dirty {
                chat_hist_dirty = false;
                let (title, lines): (String, Vec<String>) = if let Some(l) = link.as_ref() {
                    (
                        format!("CONSOLE CHAT - you & {}  (any key closes)", l.peer),
                        session_chat.clone(),
                    )
                } else {
                    (
                        "GLOBAL CHAT  (any key closes)".to_string(),
                        lobby
                            .as_ref()
                            .map(|lb| {
                                lb.mp
                                    .chat_log()
                                    .map(|m| format!("{}: {}", m.from, m.text))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    )
                };
                draw_chat_overlay(term, cur_size.0, cur_size.1, depth, &title, &lines)?;
            }
        } else if emu.frame_count > 0 && last_render.elapsed() >= render_interval && !pace.skip_frame() {
            let changed = {
                let frame = emu.frame();
                if frame.width == 0 {
                    false
                } else {
                    fb.update_from(&frame.pixels, frame.width, frame.height)
                }
            };
            if changed {
                status_dirty = true;
            }
            if fb.width > 0 {
                let write_start = Instant::now();
                // Sixel frames are full-image replaces and the status bar is
                // erase-then-rewrite: while the terminal is busy decoding the
                // DCS it happily presents mid-transaction, which reads as the
                // bar flashing against the graphic. One DEC 2026 synchronized
                // update around graphic + bar makes the whole frame atomic
                // (terminals without 2026 ignore the wrapper). The cell modes
                // send small deltas and never needed it.
                let sync = mode == RenderMode::Sixel;
                if sync {
                    term.write_all(b"\x1b[?2026h")?;
                }
                renderer.render(&fb, term)?;
                if let Some(buf) = chat_compose.as_ref() {
                    // Composing: the chat line owns the status row.
                    if status_dirty {
                        draw_chat_compose(term, &renderer, depth, buf)?;
                        status_dirty = false;
                    }
                } else if status_dirty || note.is_some() {
                    // Link cell: green = linked session, gray = lobby up,
                    // red = connection lost, absent = multiplayer off.
                    let link_cell = if let Some(l) = link.as_ref() {
                        Some(if l.mp.is_alive() { BAR_GREEN } else { BAR_RED })
                    } else if let Some(lb) = lobby.as_ref() {
                        Some(if lb.mp.is_alive() { BAR_GRAY } else { BAR_RED })
                    } else {
                        None
                    };
                    // Right-docked persistent tag: a pending knock outranks
                    // the 2P port state; linked sessions show the peer.
                    let right_tag: Option<(String, (u8, u8, u8))> =
                        if let Some((l, s)) = link.as_ref().zip(ls.as_ref()) {
                            Some((format!("LINK {} D{}", l.peer, s.delay), BAR_GREEN))
                        } else if let Some(lb) = lobby.as_ref() {
                            match lb.mp.incoming().first() {
                                Some(inc) => {
                                    Some((format!("{} KNOCKS 2/0", inc.from), BAR_YELLOW))
                                }
                                None if joinable => Some(if *lb.port_open {
                                    ("2P:OPEN".to_string(), BAR_CYAN)
                                } else {
                                    ("2P:closed".to_string(), BAR_DARK)
                                }),
                                None => None,
                            }
                        } else {
                            None
                        };
                    draw_status(
                        term,
                        &renderer,
                        sys_index,
                        depth,
                        fps_val,
                        render_fps as f32,
                        link_cell,
                        &mut note,
                        right_tag,
                        chat_capable,
                        attract.then_some(status_game.as_str()),
                    )?;
                    status_dirty = false;
                }
                if sync {
                    term.write_all(b"\x1b[?2026l")?;
                    term.flush()?;
                }
                fps_frames += 1;
                pace.note(write_start.elapsed());
                last_render = Instant::now();
            }
        }

        // FPS readout: rolling one-second window of transmitted frames.
        if fps_tick.elapsed() >= Duration::from_secs(1) {
            fps_val = fps_frames as f32 / fps_tick.elapsed().as_secs_f32();
            fps_frames = 0;
            fps_tick = Instant::now();
            status_dirty = true;
        }

        // ---- pace the loop ----
        let until_sim = sim_deadline.saturating_duration_since(Instant::now());
        if !until_sim.is_zero() {
            std::thread::sleep(until_sim.min(Duration::from_millis(3)));
        }
    }

    if let Some(a) = apc.as_mut() {
        let _ = a.stop(&mut *term);
    }
    if let Some(l) = link.as_mut() {
        l.mp.send_session_end(end_reason.as_deref().unwrap_or("peer quit"));
        l.mp.abort();
        // Persist the session replay (rom sha + input stream + end-state CRC).
        if !replay_log.is_empty() {
            if let Some(s) = ls.as_ref() {
                write_replay(rom_path, &rom, s, shape, &replay_log, &emu);
            }
        }
        if let Some(reason) = &end_reason {
            // The other player unplugging isn't OUR game ending: promote the
            // survivor to P1 and power-cycle the same cartridge solo. Errors
            // that indicate something wrong (desync, rom mismatch) still drop
            // to the menu so the caller sees the reason.
            let peer_gone = matches!(
                reason.as_str(),
                "peer quit" | "aborted" | "peer left" | "connection lost"
            );
            if peer_gone {
                exit = GameExit::PeerLeft;
                let _ = write!(
                    term,
                    "\x1b[2J\x1b[1;1H\r\n  {} unplugged - restarting the console with you on P1\r\n",
                    l.peer
                );
            } else {
                let _ = write!(term, "\x1b[2J\x1b[1;1H\r\n  link session ended: {reason}\r\n");
            }
            let _ = term.flush();
            std::thread::sleep(Duration::from_secs(2));
        }
    }
    // Restore the pre-game terminal size — but only if the size we're seeing
    // is the one we asked for. A caller who resized mid-game keeps their
    // choice.
    if resize_best && cur_size == (opt_cols, opt_rows) && cur_size != (cols, rows) {
        let _ = resize_terminal(&mut *term, rows, cols);
    }
    if mode == RenderMode::Sixel {
        let _ = term.write_all(b"\x1b[?25h"); // cursor back on
    }
    let _ = disable_physical_keys(&mut *term);
    if input.keyboard_mode() == KeyboardMode::Kitty {
        let _ = disable_kitty_keys(&mut *term);
        input.set_kitty_active(false);
    }
    Ok(exit)
}
/// Save (`save=true`) or load the quick state; returns the status-bar flash
/// message, or None when state persistence is off (netplay/attract).
fn state_io(save: bool, state_path: &Option<PathBuf>, emu: &mut Emu) -> Option<String> {
    let p = state_path.as_ref()?;
    Some(if save {
        match emu.save_state().map(|b| std::fs::write(p, b)) {
            Ok(Ok(())) => "state saved".to_string(),
            Ok(Err(e)) => format!("save failed: {e}"),
            Err(e) => format!("save failed: {e}"),
        }
    } else {
        match std::fs::read(p) {
            Ok(bytes) => match emu.load_state(&bytes) {
                Ok(()) => "state loaded".to_string(),
                Err(e) => format!("load failed: {e}"),
            },
            Err(_) => "no saved state".to_string(),
        }
    })
}

/// Persist a linked session's replay: `<roms>/.replays/<stem>-<epoch>.lgr`.
/// Header carries the ROM SHA-256 and negotiated delay; body is one line of
/// hex `p0 p1` masks per executed frame; footer records the end-state CRC so
/// `--replay` can assert bit-exact reproduction.
fn write_replay(
    rom_path: &Path,
    rom: &[u8],
    session: &lockstep::Lockstep,
    shape: multiplayer::SessionShape,
    log: &[(u16, u16)],
    emu: &Emu,
) {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let Some(dir) = rom_path.parent().map(|d| d.join(".replays")) else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let stem = rom_path.file_stem().and_then(|s| s.to_str()).unwrap_or("session");
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let sha: String = Sha256::digest(rom).iter().map(|b| format!("{b:02x}")).collect();
    let mut text = format!(
        "LGR1 sha={} delay={} slot={} shape={} rom={}\n",
        sha,
        session.delay,
        session.local_slot,
        match shape {
            multiplayer::SessionShape::GearToGear => "gear-to-gear",
            multiplayer::SessionShape::SharedConsole => "shared-console",
        },
        rom_path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
    );
    for &(p0, p1) in log {
        let _ = writeln!(text, "{p0:x} {p1:x}");
    }
    let crc = emu.state_crc32().unwrap_or(0);
    let _ = writeln!(text, "end crc={crc:08x} frames={}", log.len());
    let _ = std::fs::write(dir.join(format!("{stem}-{epoch}.lgr")), text);
}

// ---- in-game status bar (modeled on lameboy's draw_status_bar) ----
// A solid BLACK row so content never fights a colored strip for contrast:
// FPS docked left (health-colored), one link-cable cell, the system name as
// a theme-accent badge, centered content spans with a strict priority
// (chat cyan / alert yellow / notice white / three-tone key hints), and a
// compact right-docked tag for persistent state (knock pending, 2P port,
// link peer).

/// What kind of message currently owns the bar's center.
#[derive(Clone, Copy, PartialEq)]
enum BarKind {
    Chat,   // bright cyan — someone said something
    Alert,  // yellow — needs a decision (knock, server error)
    Notice, // white — confirmations (state saved, port toggled)
}

type BarNote = Option<(BarKind, String, Instant)>;

const BAR_LIGHT: (u8, u8, u8) = (170, 170, 170); // key labels
const BAR_DARK: (u8, u8, u8) = (85, 85, 85); // separators
const BAR_WHITE: (u8, u8, u8) = (255, 255, 255); // actions / notices
const BAR_YELLOW: (u8, u8, u8) = (255, 214, 0); // alerts (knocks)
const BAR_CYAN: (u8, u8, u8) = (85, 255, 255); // chat
const BAR_GREEN: (u8, u8, u8) = (85, 255, 85); // healthy / linked
const BAR_RED: (u8, u8, u8) = (255, 85, 85); // starved / lost
const BAR_GRAY: (u8, u8, u8) = (130, 130, 130); // lobby-connected cell

/// Messages stay long enough to read; confirmations just acknowledge.
fn bar_ttl(kind: BarKind) -> Duration {
    match kind {
        BarKind::Chat | BarKind::Alert => Duration::from_secs(8),
        BarKind::Notice => Duration::from_secs(3),
    }
}

fn hint_pair(v: &mut Vec<(String, (u8, u8, u8))>, k: &str, a: &str) {
    v.push(("  ".into(), BAR_DARK));
    v.push((k.into(), BAR_LIGHT));
    v.push(("-".into(), BAR_DARK));
    v.push((a.into(), BAR_WHITE));
}

/// Default center content: the machine's key hints in lameboy's three-tone
/// scheme (keys light gray, separators dark, actions white; chat in its own
/// cyan so the hint matches the message color it announces). When `avail` is
/// tight, the most obvious hints go first (the d-pad, then save states) so
/// what remains stays whole instead of clipping mid-hint.
fn hint_spans(machine: Machine, chat: bool, avail: usize) -> Vec<(String, (u8, u8, u8))> {
    let build = |dpad: bool, states: bool| {
        let mut v: Vec<(String, (u8, u8, u8))> = Vec::new();
        if dpad {
            v.push(("ARROWS".into(), BAR_LIGHT));
            v.push(("-".into(), BAR_DARK));
            v.push(("D-Pad".into(), BAR_WHITE));
        }
        for tok in input::game_key_hint(machine).split_whitespace() {
            if let Some((k, a)) = tok.split_once(':') {
                if v.is_empty() {
                    v.push((k.into(), BAR_LIGHT));
                    v.push(("-".into(), BAR_DARK));
                    v.push((a.into(), BAR_WHITE));
                } else {
                    hint_pair(&mut v, k, a);
                }
            }
        }
        if states {
            hint_pair(&mut v, "5/8", "state");
        }
        if chat {
            v.push(("  ".into(), BAR_DARK));
            v.push(("`".into(), BAR_CYAN));
            v.push(("-".into(), BAR_DARK));
            v.push(("chat".into(), (0, 170, 170)));
        }
        hint_pair(&mut v, "Q", "quit");
        v
    };
    let len = |v: &Vec<(String, (u8, u8, u8))>| -> usize {
        v.iter().map(|(t, _)| t.chars().count()).sum()
    };
    for (dpad, states) in [(true, true), (false, true), (false, false)] {
        let v = build(dpad, states);
        if len(&v) <= avail {
            return v;
        }
    }
    build(false, false)
}

#[allow(clippy::too_many_arguments)]
fn draw_status(
    term: &mut dyn Term,
    renderer: &Renderer,
    sys_index: usize,
    depth: color::ColorDepth,
    fps: f32,
    fps_target: f32,
    link_cell: Option<(u8, u8, u8)>,
    note: &mut BarNote,
    right_tag: Option<(String, (u8, u8, u8))>,
    chat_hint: bool,
    attract_title: Option<&str>,
) -> io::Result<()> {
    let sys = &SYSTEMS[sys_index];
    let row = renderer.fps_row() + 1; // 1-based
    let w = renderer.term_cols().max(40) as usize;

    if note.as_ref().is_some_and(|(k, _, at)| at.elapsed() >= bar_ttl(*k)) {
        *note = None;
    }

    let mut buf: Vec<u8> = Vec::with_capacity(w + 96);
    // Solid black base first (\x1b[K fills with the active bg; autowrap is
    // off session-wide so the bottom row can't scroll).
    let _ = write!(buf, "\x1b[{row};1H\x1b[0;40m\x1b[K");

    // FPS docked left, health-colored against the transmit target.
    let fps_rgb = if fps >= fps_target * 0.85 {
        BAR_GREEN
    } else if fps >= fps_target * 0.5 {
        BAR_YELLOW
    } else {
        BAR_RED
    };
    let _ = write!(
        buf,
        "\x1b[{}mFPS {:>2.0}",
        color::fg_sgr(depth, fps_rgb.0, fps_rgb.1, fps_rgb.2),
        fps.min(99.0)
    );
    // Link-cable cell: green = linked session live, gray = lobby connected,
    // red = connection lost. Absent when multiplayer is off.
    if let Some((r, g, b)) = link_cell {
        let _ = write!(buf, " \x1b[{}m█", color::fg_sgr(depth, r, g, b));
    }
    // System badge in its theme accent — per-machine identity without
    // painting the whole bar in it.
    let th = &sys.theme;
    let _ = write!(
        buf,
        " \x1b[{}m{}",
        color::fg_sgr(depth, th.accent.0, th.accent.1, th.accent.2),
        sys.name
    );
    let left_len = 6 + if link_cell.is_some() { 2 } else { 0 } + 1 + sys.name.chars().count();

    // Right-docked persistent tag (knock pending / 2P port / link peer).
    let right_len = right_tag.as_ref().map(|(t, _)| t.chars().count() + 1).unwrap_or(0);
    if let Some((t, (r, g, b))) = &right_tag {
        let col = w.saturating_sub(t.chars().count() + 1).max(left_len + 2);
        let _ = write!(buf, "\x1b[{row};{}H\x1b[{}m{}", col + 1, color::fg_sgr(depth, *r, *g, *b), t);
    }

    // Centered content by priority: message > attract title > key hints,
    // clipped to the space between the left dock and the right tag (never
    // the last column).
    let avail_left = left_len + 2;
    let avail = w.saturating_sub(right_len + 1).saturating_sub(avail_left);
    let spans: Vec<(String, (u8, u8, u8))> = if let Some((kind, text, _)) = note.as_ref() {
        let color = match kind {
            BarKind::Chat => BAR_CYAN,
            BarKind::Alert => BAR_YELLOW,
            BarKind::Notice => BAR_WHITE,
        };
        vec![(text.clone(), color)]
    } else if let Some(t) = attract_title {
        vec![(t.to_string(), BAR_WHITE), (" - any key plays, Q menu".into(), BAR_LIGHT)]
    } else {
        hint_spans(sys.machine, chat_hint, avail)
    };
    let content_len: usize = spans.iter().map(|(t, _)| t.chars().count()).sum();
    let start = if content_len < avail { avail_left + (avail - content_len) / 2 } else { avail_left };
    let _ = write!(buf, "\x1b[{row};{}H", start + 1);
    let mut used = 0usize;
    'spans: for (t, (r, g, b)) in &spans {
        let _ = write!(buf, "\x1b[{}m", color::fg_sgr(depth, *r, *g, *b));
        for ch in t.chars() {
            if used >= avail {
                break 'spans;
            }
            let mut tmp = [0u8; 4];
            buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
            used += 1;
        }
    }
    buf.extend_from_slice(b"\x1b[0m");
    let mut wtr = cp437::Cp437Writer::new(&mut *term);
    wtr.write_all(&buf)?;
    wtr.flush()
}
