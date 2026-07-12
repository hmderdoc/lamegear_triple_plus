//! Adapter around the vendored jgenesis cores (smsgg-core, genesis-core,
//! nes-core) for headless, deterministic, frame-at-a-time emulation.
//!
//! Every core is instruction-stepped behind the same `EmulatorTrait`; this
//! wrapper ticks until exactly one video frame has been rendered, captures
//! the RGBA framebuffer, and exposes save-state / state-CRC hooks for the
//! determinism harness and netplay. Door-level inputs (`DoorInputs`, two
//! players x the button array in input.rs order) fan out into each core's
//! own input struct here.

use std::fmt;
use std::num::NonZeroU32;
use std::path::PathBuf;

use jgenesis_common::frontend::{
    AudioOutput, Color, EmulatorTrait, FrameSize, InputPoller, Renderer, RenderFrameOptions,
    SaveWriter, TickEffect, TimingMode,
};
use smsgg_config::{GgAspectRatio, SmsAspectRatio, SmsGgInputs, SmsModel};
use smsgg_core::{SmsGgEmulator, SmsGgEmulatorConfig, SmsGgHardware};

use gba_core::api::{GameBoyAdvanceEmulator, GbaEmulatorConfig};
use genesis_core::{GenesisEmulator, GenesisEmulatorConfig};
use nes_core::api::{NesEmulator, NesEmulatorConfig};
use pce_core::api::{PcEngineEmulator, PceEmulatorConfig};
use snes_core::api::{CoprocessorRoms, SnesEmulator, SnesEmulatorConfig};

use crate::input::BUTTON_COUNT;

/// Which console the ROM runs on. `GameGearExpanded` is the GG VDP's full
/// 256x192 picture instead of the 160x144 LCD window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Machine {
    MasterSystem,
    GameGear,
    GameGearExpanded,
    Sg1000,
    Genesis,
    Nes,
    Snes,
    Gba,
    Pce,
}

impl Machine {
    fn smsgg_hardware(self) -> Option<SmsGgHardware> {
        match self {
            Machine::MasterSystem => Some(SmsGgHardware::MasterSystem),
            Machine::GameGear | Machine::GameGearExpanded => Some(SmsGgHardware::GameGear),
            Machine::Sg1000 => Some(SmsGgHardware::Sg1000),
            Machine::Genesis | Machine::Nes | Machine::Snes | Machine::Gba | Machine::Pce => None,
        }
    }

    pub fn from_extension(ext: &str) -> Option<Machine> {
        match ext.to_ascii_lowercase().as_str() {
            "sms" => Some(Machine::MasterSystem),
            "gg" => Some(Machine::GameGear),
            "sg" => Some(Machine::Sg1000),
            "md" | "gen" | "smd" => Some(Machine::Genesis),
            "nes" => Some(Machine::Nes),
            "sfc" | "smc" => Some(Machine::Snes),
            "gba" => Some(Machine::Gba),
            "pce" => Some(Machine::Pce),
            _ => None,
        }
    }
}

/// Both players' held buttons in input.rs `button_index` order.
#[derive(Clone, Copy, Default)]
pub struct DoorInputs {
    pub p1: [bool; BUTTON_COUNT],
    pub p2: [bool; BUTTON_COUNT],
}

impl DoorInputs {
    pub fn solo(p1: [bool; BUTTON_COUNT]) -> DoorInputs {
        DoorInputs { p1, p2: [false; BUTTON_COUNT] }
    }
}

// button_index order: 0=up 1=down 2=left 3=right 4=one 5=two 6=pause/start
//                     7=three 8=select

fn smsgg_inputs(d: &DoorInputs) -> SmsGgInputs {
    let pad = |p: &[bool; BUTTON_COUNT]| smsgg_config::SmsGgJoypadState {
        up: p[0],
        down: p[1],
        left: p[2],
        right: p[3],
        button1: p[4],
        button2: p[5],
    };
    let mut inputs = SmsGgInputs::default();
    inputs.p1 = pad(&d.p1);
    inputs.p2 = pad(&d.p2);
    // The pause/start button is on the console, not the pad.
    inputs.pause = d.p1[6] || d.p2[6];
    inputs
}

fn genesis_inputs(d: &DoorInputs) -> genesis_config::GenesisInputs {
    // Six-button pad, keyed so the same physical key throws the same move on
    // Genesis and SNES — Street Fighter II is the reference: kicks
    // (SNES B/A/R) = Genesis A/B/C on Z/X/V, punches (SNES Y/X/L) =
    // Genesis X/Y/Z on A/S/C. SPACE = the pad's MODE button.
    let pad = |p: &[bool; BUTTON_COUNT]| genesis_config::GenesisJoypadState {
        up: p[0],
        down: p[1],
        left: p[2],
        right: p[3],
        a: p[4],  // Z key (SNES B  - SF2 short kick)
        b: p[5],  // X key (SNES A  - SF2 forward kick)
        c: p[11], // V key (SNES R  - SF2 roundhouse kick)
        x: p[7],  // A key (SNES Y  - SF2 jab punch)
        y: p[9],  // S key (SNES X  - SF2 strong punch)
        z: p[10], // C key (SNES L  - SF2 fierce punch)
        start: p[6],
        mode: p[8],
        ..Default::default()
    };
    // The core's auto_3_button_mode (default on) drops back to a 3-button
    // pad for the few games its database marks 6-button-incompatible.
    genesis_config::GenesisInputs {
        p1: genesis_config::GenesisController::SixButton(pad(&d.p1)),
        p2: genesis_config::GenesisController::SixButton(pad(&d.p2)),
    }
}

fn snes_inputs(d: &DoorInputs) -> snes_core::input::SnesInputs {
    let pad = |p: &[bool; BUTTON_COUNT]| snes_config::SnesJoypadState {
        up: p[0],
        down: p[1],
        left: p[2],
        right: p[3],
        b: p[4],  // Z = primary (B is the SNES jump button)
        a: p[5],  // X
        y: p[7],  // A key
        x: p[9],  // S key
        l: p[10], // C key
        r: p[11], // V key
        start: p[6],
        select: p[8],
    };
    snes_core::input::SnesInputs {
        p1: pad(&d.p1),
        p2: snes_core::input::SnesInputDevice::Controller(pad(&d.p2)),
    }
}

fn gba_inputs(d: &DoorInputs) -> gba_config::GbaInputs {
    gba_config::GbaInputs {
        joypad: gba_config::GbaJoypadInputs {
            up: d.p1[0],
            down: d.p1[1],
            left: d.p1[2],
            right: d.p1[3],
            a: d.p1[4], // Z = primary
            b: d.p1[5], // X
            l: d.p1[10],
            r: d.p1[11],
            start: d.p1[6],
            select: d.p1[8],
        },
        solar: Default::default(),
    }
}

fn pce_inputs(d: &DoorInputs) -> pce_config::PceInputs {
    // Single-player (the door doesn't emulate a Turbo Tap): pad 1 only.
    let mut inputs = pce_config::PceInputs::default();
    inputs.p1 = pce_config::PceJoypadState {
        up: d.p1[0],
        down: d.p1[1],
        left: d.p1[2],
        right: d.p1[3],
        button1: d.p1[4], // Z = I
        button2: d.p1[5], // X = II
        run: d.p1[6],     // ENTER
        select: d.p1[8],  // SPACE
    };
    inputs
}

fn nes_inputs(d: &DoorInputs) -> nes_core::input::NesInputs {
    let pad = |p: &[bool; BUTTON_COUNT]| nes_config::NesJoypadState {
        up: p[0],
        down: p[1],
        left: p[2],
        right: p[3],
        a: p[4],
        b: p[5],
        start: p[6],
        select: p[8],
    };
    nes_core::input::NesInputs {
        p1: pad(&d.p1),
        p2: nes_core::input::NesInputDevice::Controller(pad(&d.p2)),
    }
}

/// Captured video frame: tightly packed RGBA at `width` x `height`.
#[derive(Default)]
pub struct Frame {
    pub pixels: Vec<Color>,
    pub width: u32,
    pub height: u32,
    /// Square-pixel aspect hint from the core (width multiplier), if any.
    pub pixel_aspect_ratio: Option<f64>,
}

struct FrameSink {
    frame: Frame,
    rendered: bool,
}

impl Renderer for FrameSink {
    type Err = std::convert::Infallible;

    fn render_frame(
        &mut self,
        frame_buffer: &[Color],
        frame_size: FrameSize,
        _target_fps: f64,
        options: RenderFrameOptions,
    ) -> Result<(), Self::Err> {
        let len = (frame_size.width * frame_size.height) as usize;
        self.frame.pixels.clear();
        self.frame.pixels.extend_from_slice(&frame_buffer[..len]);
        self.frame.width = frame_size.width;
        self.frame.height = frame_size.height;
        self.frame.pixel_aspect_ratio = options.pixel_aspect_ratio.map(f64::from);
        self.rendered = true;
        Ok(())
    }
}

/// Collects core audio for the streaming-audio path; bounded so an unused
/// sink never grows.
pub struct AudioSink {
    pub samples: Vec<(f64, f64)>,
    pub enabled: bool,
}

impl AudioOutput for AudioSink {
    type Err = std::convert::Infallible;

    fn push_sample(&mut self, sample_l: f64, sample_r: f64) -> Result<(), Self::Err> {
        if self.enabled && self.samples.len() < 48_000 {
            self.samples.push((sample_l, sample_r));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct SaveError(String);

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Persists cartridge SRAM ("sav") under a per-user path; `None` disables
/// persistence entirely (netplay sessions boot with empty SRAM).
struct DiskSaveWriter {
    base: Option<PathBuf>,
}

impl DiskSaveWriter {
    fn path(&self, extension: &str) -> Option<PathBuf> {
        self.base.as_ref().map(|b| b.with_extension(extension))
    }
}

impl SaveWriter for DiskSaveWriter {
    type Err = SaveError;

    fn load_bytes(&mut self, extension: &str) -> Result<Vec<u8>, Self::Err> {
        let path = self.path(extension).ok_or_else(|| SaveError("saves disabled".into()))?;
        std::fs::read(&path).map_err(|e| SaveError(format!("{}: {e}", path.display())))
    }

    fn persist_bytes(&mut self, extension: &str, bytes: &[u8]) -> Result<(), Self::Err> {
        let Some(path) = self.path(extension) else { return Ok(()) };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&path, bytes).map_err(|e| SaveError(format!("{}: {e}", path.display())))
    }

    fn load_serialized<D: bincode::Decode<()>>(
        &mut self,
        extension: &str,
    ) -> Result<D, Self::Err> {
        let bytes = self.load_bytes(extension)?;
        bincode::decode_from_slice(&bytes, bincode::config::standard())
            .map(|(v, _)| v)
            .map_err(|e| SaveError(e.to_string()))
    }

    fn persist_serialized<E: bincode::Encode>(
        &mut self,
        extension: &str,
        data: E,
    ) -> Result<(), Self::Err> {
        let bytes = bincode::encode_to_vec(data, bincode::config::standard())
            .map_err(|e| SaveError(e.to_string()))?;
        self.persist_bytes(extension, &bytes)
    }
}

struct HeldInputs<T>(T);

impl<T> InputPoller<T> for HeldInputs<T> {
    fn poll(&mut self) -> &T {
        &self.0
    }
}

pub struct EmuOptions {
    pub timing: TimingMode,
    pub fm_sound_unit: bool,
    pub remove_sprite_limit: bool,
    /// Crop the SMS vertical borders so the frame is the active picture only.
    pub crop_vertical_border: bool,
    /// `.sav` persistence base path (without extension); `None` = volatile.
    pub save_base: Option<PathBuf>,
    /// Game Genie / cheat overrides (SMS/GG machines).
    pub cheat_codes: Vec<jgenesis_common::cheats::ByteCheatCodeU16Address>,
    /// Genesis cheat overrides: (address, value) pairs.
    pub genesis_cheats: Vec<(u32, u16)>,
    /// GBA BIOS image (16KB). `None` uses a zeroed dummy: fine for homebrew
    /// that avoids BIOS SWI calls; commercial games need the real BIOS.
    pub gba_bios: Option<Vec<u8>>,
}

impl Default for EmuOptions {
    fn default() -> Self {
        EmuOptions {
            timing: TimingMode::Ntsc,
            fm_sound_unit: false,
            remove_sprite_limit: false,
            crop_vertical_border: true,
            save_base: None,
            cheat_codes: Vec::new(),
            genesis_cheats: Vec::new(),
            gba_bios: None,
        }
    }
}

fn smsgg_config(machine: Machine, opts: &EmuOptions) -> SmsGgEmulatorConfig {
    SmsGgEmulatorConfig {
        sms_timing_mode: opts.timing,
        sms_model: SmsModel::default(),
        forced_psg_version: None,
        sms_aspect_ratio: SmsAspectRatio::default(),
        gg_aspect_ratio: GgAspectRatio::default(),
        remove_sprite_limit: opts.remove_sprite_limit,
        forced_region: None,
        sms_crop_vertical_border: opts.crop_vertical_border,
        sms_crop_left_border: false,
        gg_frame_blending: false,
        gg_use_sms_resolution: machine == Machine::GameGearExpanded,
        fm_sound_unit_enabled: opts.fm_sound_unit,
        z80_divider: NonZeroU32::new(smsgg_core::NATIVE_Z80_DIVIDER).unwrap(),
        allow_opposing_joypad_directions: false,
        cheat_codes: opts.cheat_codes.clone(),
    }
}

fn genesis_cfg(opts: &EmuOptions) -> GenesisEmulatorConfig {
    GenesisEmulatorConfig {
        forced_timing_mode: Some(opts.timing),
        remove_sprite_limits: opts.remove_sprite_limit,
        cheat_codes: opts.genesis_cheats.clone(),
        ..Default::default()
    }
}

fn nes_cfg(opts: &EmuOptions) -> NesEmulatorConfig {
    NesEmulatorConfig {
        forced_timing_mode: Some(opts.timing),
        aspect_ratio: nes_config::NesAspectRatio::Ntsc,
        palette: nes_config::NesPalette::default(),
        // Built-in NTSC overscan crop: top+bottom 8 rows -> 256x224, which is
        // both what a CRT showed and what fits a 255-col terminal after the
        // 1-column clip rule.
        ntsc_crop_vertical_overscan: true,
        overscan: nes_config::Overscan::NONE,
        // Real RGB888 out of the palette, not packed 6-bit color for a shader.
        emulate_ntsc_output: false,
        remove_sprite_limit: opts.remove_sprite_limit,
        pal_black_border: false,
        silence_ultrasonic_triangle_output: false,
        audio_resampler: nes_config::NesAudioResampler::default(),
        audio_refresh_rate_adjustment: false,
        allow_opposing_joypad_directions: false,
        dma_dummy_joy_reads: false,
    }
}

fn snes_cfg(opts: &EmuOptions) -> SnesEmulatorConfig {
    SnesEmulatorConfig { forced_timing_mode: Some(opts.timing), ..Default::default() }
}

fn pce_cfg(opts: &EmuOptions) -> PceEmulatorConfig {
    PceEmulatorConfig {
        region: pce_config::PceRegion::default(),
        // Square pixels: the half-block renderer never aspect-corrects.
        aspect_ratio: pce_config::PceAspectRatio::SquarePixels,
        palette: pce_config::PcePaletteType::default(),
        crop_overscan: opts.crop_vertical_border,
        remove_sprite_limits: opts.remove_sprite_limit,
        audio_resampler: pce_config::PceAudioResampler::default(),
        input_device: pce_config::PceInputDevice::default(),
        turbo_tap_connected: [false; 5],
        allow_opposing_joypad_directions: false,
        allow_simultaneous_run_select: false,
    }
}

fn gba_cfg() -> GbaEmulatorConfig {
    GbaEmulatorConfig {
        // Boot the cart entry directly: deterministic (no BIOS animation),
        // and lets a dummy BIOS work for SWI-free homebrew.
        skip_bios_animation: true,
        ..Default::default()
    }
}

enum Core {
    SmsGg(Box<SmsGgEmulator>),
    Genesis(Box<GenesisEmulator>),
    Nes(Box<NesEmulator>),
    Snes(Box<SnesEmulator>),
    Gba(Box<GameBoyAdvanceEmulator>),
    Pce(Box<PcEngineEmulator>),
    /// Two Game Gears joined by an in-process Gear-to-Gear cable (spec §4.2:
    /// each peer simulates the WHOLE session and renders machines[local]).
    /// The cable is pumped at instruction granularity — far finer than the
    /// ~2.1ms/byte the 4800-baud UART needs.
    GgLink {
        a: Box<SmsGgEmulator>,
        b: Box<SmsGgEmulator>,
        local: u8,
        remote_sink: FrameSink,
        remote_audio: AudioSink,
    },
}

pub struct Emu {
    core: Core,
    sink: FrameSink,
    pub audio: AudioSink,
    saves: DiskSaveWriter,
    pub machine: Machine,
    pub frame_count: u64,
}

impl Emu {
    pub fn new(mut rom: Vec<u8>, machine: Machine, opts: EmuOptions) -> Result<Emu, String> {
        // PCE dumps sometimes carry a 512-byte copier header; the core does
        // no header detection and its power-of-two mirroring would turn a
        // headered image into garbage — strip it here.
        if machine == Machine::Pce && rom.len() % 1024 == 512 {
            rom.drain(..512);
        }
        let mut saves = DiskSaveWriter { base: opts.save_base.clone() };
        let core = match machine {
            Machine::Genesis => Core::Genesis(Box::new(GenesisEmulator::create(
                rom,
                genesis_cfg(&opts),
                &mut saves,
            ))),
            Machine::Nes => Core::Nes(Box::new(
                NesEmulator::create(rom, nes_cfg(&opts), &mut saves)
                    .map_err(|e| e.to_string())?,
            )),
            Machine::Snes => Core::Snes(Box::new(
                SnesEmulator::create(rom, snes_cfg(&opts), CoprocessorRoms::none(), &mut saves)
                    .map_err(|e| e.to_string())?,
            )),
            Machine::Gba => {
                let bios = opts.gba_bios.clone().unwrap_or_else(|| vec![0u8; 16 * 1024]);
                Core::Gba(Box::new(
                    GameBoyAdvanceEmulator::create(rom, bios, gba_cfg(), &mut saves)
                        .map_err(|e| e.to_string())?,
                ))
            }
            Machine::Pce => Core::Pce(Box::new(PcEngineEmulator::create(
                rom,
                pce_cfg(&opts),
                &mut saves,
            ))),
            _ => Core::SmsGg(Box::new(SmsGgEmulator::create(
                Some(rom),
                None,
                machine.smsgg_hardware().unwrap(),
                smsgg_config(machine, &opts),
                &mut saves,
            ))),
        };
        Ok(Emu {
            core,
            sink: FrameSink { frame: Frame::default(), rendered: false },
            audio: AudioSink { samples: Vec::new(), enabled: false },
            saves,
            machine,
            frame_count: 0,
        })
    }

    /// Two Game Gears + cable: `local_slot` picks which machine this caller
    /// sees. Netplay contract enforced: volatile saves, no cheats.
    pub fn new_gg_link(rom: Vec<u8>, local_slot: u8, opts: &EmuOptions) -> Result<Emu, String> {
        let base = EmuOptions {
            timing: opts.timing,
            crop_vertical_border: opts.crop_vertical_border,
            ..EmuOptions::default()
        };
        let cfg = smsgg_config(Machine::GameGear, &base);
        let mut saves = DiskSaveWriter { base: None };
        let a = SmsGgEmulator::create(
            Some(rom.clone()),
            None,
            SmsGgHardware::GameGear,
            cfg.clone(),
            &mut saves,
        );
        let b =
            SmsGgEmulator::create(Some(rom), None, SmsGgHardware::GameGear, cfg, &mut saves);
        Ok(Emu {
            core: Core::GgLink {
                a: Box::new(a),
                b: Box::new(b),
                local: local_slot.min(1),
                remote_sink: FrameSink { frame: Frame::default(), rendered: false },
                remote_audio: AudioSink { samples: Vec::new(), enabled: false },
            },
            sink: FrameSink { frame: Frame::default(), rendered: false },
            audio: AudioSink { samples: Vec::new(), enabled: false },
            saves,
            machine: Machine::GameGear,
            frame_count: 0,
        })
    }

    /// I/O port trace for session-shape classification (SMS/GG cores only).
    pub fn gg_port_trace(&self) -> Option<smsgg_core::PortTrace> {
        match &self.core {
            Core::SmsGg(c) => Some(c.port_trace()),
            Core::GgLink { a, .. } => Some(a.port_trace()),
            _ => None,
        }
    }

    /// Run the core until exactly one video frame has been rendered.
    /// `inputs` is held constant for the whole frame (sampled once at the
    /// frame boundary, per the determinism contract).
    pub fn step_frame(&mut self, inputs: &DoorInputs) -> &Frame {
        self.sink.rendered = false;
        // Errors can only come from the save writer (disk); rendering and
        // audio sinks are infallible. A failed SRAM flush shouldn't kill the
        // session, but a tick that can't advance must not spin forever.
        match &mut self.core {
            Core::SmsGg(core) => {
                let mut poll = HeldInputs(smsgg_inputs(inputs));
                while !self.sink.rendered {
                    if let Err(e) =
                        core.tick(&mut self.sink, &mut self.audio, &mut poll, &mut self.saves)
                    {
                        log::warn!("core tick error (continuing): {e}");
                        if !self.sink.rendered {
                            break;
                        }
                    }
                }
            }
            Core::Genesis(core) => {
                let mut poll = HeldInputs(genesis_inputs(inputs));
                while !self.sink.rendered {
                    if let Err(e) =
                        core.tick(&mut self.sink, &mut self.audio, &mut poll, &mut self.saves)
                    {
                        log::warn!("core tick error (continuing): {e}");
                        if !self.sink.rendered {
                            break;
                        }
                    }
                }
            }
            Core::Nes(core) => {
                let mut poll = HeldInputs(nes_inputs(inputs));
                while !self.sink.rendered {
                    if let Err(e) =
                        core.tick(&mut self.sink, &mut self.audio, &mut poll, &mut self.saves)
                    {
                        log::warn!("core tick error (continuing): {e}");
                        if !self.sink.rendered {
                            break;
                        }
                    }
                }
            }
            Core::Snes(core) => {
                let mut poll = HeldInputs(snes_inputs(inputs));
                while !self.sink.rendered {
                    if let Err(e) =
                        core.tick(&mut self.sink, &mut self.audio, &mut poll, &mut self.saves)
                    {
                        log::warn!("core tick error (continuing): {e}");
                        if !self.sink.rendered {
                            break;
                        }
                    }
                }
            }
            Core::Gba(core) => {
                let mut poll = HeldInputs(gba_inputs(inputs));
                while !self.sink.rendered {
                    if let Err(e) =
                        core.tick(&mut self.sink, &mut self.audio, &mut poll, &mut self.saves)
                    {
                        log::warn!("core tick error (continuing): {e}");
                        if !self.sink.rendered {
                            break;
                        }
                    }
                }
            }
            Core::Pce(core) => {
                let mut poll = HeldInputs(pce_inputs(inputs));
                while !self.sink.rendered {
                    if let Err(e) =
                        core.tick(&mut self.sink, &mut self.audio, &mut poll, &mut self.saves)
                    {
                        log::warn!("core tick error (continuing): {e}");
                        if !self.sink.rendered {
                            break;
                        }
                    }
                }
            }
            Core::GgLink { a, b, local, remote_sink, remote_audio } => {
                // Each machine has ONE player: slot 0's pad drives machine A,
                // slot 1's drives machine B.
                let pad = |p: &[bool; BUTTON_COUNT]| {
                    smsgg_inputs(&DoorInputs { p1: *p, p2: [false; BUTTON_COUNT] })
                };
                let mut poll_a = HeldInputs(pad(&inputs.p1));
                let mut poll_b = HeldInputs(pad(&inputs.p2));
                remote_sink.rendered = false;
                let (sink_a, sink_b, audio_a, audio_b) = if *local == 0 {
                    (&mut self.sink, &mut *remote_sink, &mut self.audio, &mut *remote_audio)
                } else {
                    (&mut *remote_sink, &mut self.sink, &mut *remote_audio, &mut self.audio)
                };
                let mut a_done = false;
                let mut b_done = false;
                let mut guard = 0u32;
                while !(a_done && b_done) {
                    if !a_done {
                        if let Err(e) = a.tick(&mut *sink_a, &mut *audio_a, &mut poll_a, &mut self.saves) {
                            log::warn!("gg-link A tick error: {e}");
                        }
                        a_done = sink_a.rendered;
                    }
                    if let Some(byte) = a.serial_take_tx() {
                        b.serial_deliver_rx(byte);
                    }
                    if !b_done {
                        if let Err(e) = b.tick(&mut *sink_b, &mut *audio_b, &mut poll_b, &mut self.saves) {
                            log::warn!("gg-link B tick error: {e}");
                        }
                        b_done = sink_b.rendered;
                    }
                    if let Some(byte) = b.serial_take_tx() {
                        a.serial_deliver_rx(byte);
                    }
                    guard += 1;
                    if guard > 2_000_000 {
                        log::warn!("gg-link frame guard tripped");
                        break;
                    }
                }
            }
        }
        self.frame_count += 1;
        &self.sink.frame
    }

    pub fn frame(&self) -> &Frame {
        &self.sink.frame
    }

    pub fn target_fps(&self) -> f64 {
        match &self.core {
            Core::SmsGg(c) => c.target_fps(),
            Core::Genesis(c) => c.target_fps(),
            Core::Nes(c) => c.target_fps(),
            Core::Snes(c) => c.target_fps(),
            Core::Gba(c) => c.target_fps(),
            Core::Pce(c) => c.target_fps(),
            Core::GgLink { a, .. } => a.target_fps(),
        }
    }

    /// Serialize the full machine state (minus ROM) to bytes.
    pub fn save_state(&self) -> Result<Vec<u8>, String> {
        let cfg = bincode::config::standard();
        match &self.core {
            Core::SmsGg(c) => bincode::encode_to_vec(c.to_save_state(), cfg),
            Core::Genesis(c) => bincode::encode_to_vec(c.to_save_state(), cfg),
            Core::Nes(c) => bincode::encode_to_vec(c.to_save_state(), cfg),
            Core::Snes(c) => bincode::encode_to_vec(c.to_save_state(), cfg),
            Core::Gba(c) => bincode::encode_to_vec(c.to_save_state(), cfg),
            Core::Pce(c) => bincode::encode_to_vec(c.to_save_state(), cfg),
            Core::GgLink { a, b, .. } => {
                bincode::encode_to_vec((a.to_save_state(), b.to_save_state()), cfg)
            }
        }
        .map_err(|e| e.to_string())
    }

    /// Restore state saved by `save_state` (must be the same ROM + build).
    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), String> {
        let cfg = bincode::config::standard();
        match &mut self.core {
            Core::SmsGg(c) => {
                let (state, _): (SmsGgEmulator, _) =
                    bincode::decode_from_slice(bytes, cfg).map_err(|e| e.to_string())?;
                c.load_state(state);
            }
            Core::Genesis(c) => {
                let (state, _): (GenesisEmulator, _) =
                    bincode::decode_from_slice(bytes, cfg).map_err(|e| e.to_string())?;
                c.load_state(state);
            }
            Core::Nes(c) => {
                let (state, _): (NesEmulator, _) =
                    bincode::decode_from_slice(bytes, cfg).map_err(|e| e.to_string())?;
                c.load_state(state);
            }
            Core::Snes(c) => {
                let (state, _): (SnesEmulator, _) =
                    bincode::decode_from_slice(bytes, cfg).map_err(|e| e.to_string())?;
                c.load_state(state);
            }
            Core::Gba(c) => {
                let (state, _): (GameBoyAdvanceEmulator, _) =
                    bincode::decode_from_slice(bytes, cfg).map_err(|e| e.to_string())?;
                c.load_state(state);
            }
            Core::Pce(c) => {
                let (state, _): (PcEngineEmulator, _) =
                    bincode::decode_from_slice(bytes, cfg).map_err(|e| e.to_string())?;
                c.load_state(state);
            }
            Core::GgLink { a, b, .. } => {
                let ((sa, sb), _): ((SmsGgEmulator, SmsGgEmulator), _) =
                    bincode::decode_from_slice(bytes, cfg).map_err(|e| e.to_string())?;
                a.load_state(sa);
                b.load_state(sb);
            }
        }
        Ok(())
    }

    /// CRC32 of the serialized state — the per-frame desync check.
    pub fn state_crc32(&self) -> Result<u32, String> {
        let bytes = self.save_state()?;
        const CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
        Ok(CRC.checksum(&bytes))
    }

    #[cfg(test)]
    fn gg_link_machine_a_state(&self) -> Vec<u8> {
        match &self.core {
            Core::GgLink { a, .. } => {
                bincode::encode_to_vec(a.to_save_state(), bincode::config::standard()).unwrap()
            }
            _ => panic!("not a gg-link emu"),
        }
    }

    pub fn soft_reset(&mut self) {
        match &mut self.core {
            Core::SmsGg(c) => c.soft_reset(),
            Core::Genesis(c) => c.soft_reset(),
            Core::Nes(c) => c.soft_reset(),
            Core::Snes(c) => c.soft_reset(),
            Core::Gba(c) => c.soft_reset(),
            Core::Pce(c) => c.soft_reset(),
            Core::GgLink { a, b, .. } => {
                a.soft_reset();
                b.soft_reset();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE cable proof: machine A's own serialized state must change when
    /// only machine B's inputs change — i.e. bytes really cross the
    /// Gear-to-Gear cable and land in A's receive path. (The determinism
    /// gate alone can't distinguish a working cable from a silently-dead RX.)
    #[test]
    fn gg_link_cable_couples_machines() {
        let Ok(rom) = std::fs::read("roms/Test Link.gg") else {
            eprintln!("skipping: roms/Test Link.gg not generated");
            return;
        };
        let run = |p2_button: bool| {
            let mut emu = Emu::new_gg_link(rom.clone(), 0, &EmuOptions::default()).unwrap();
            let mut d = DoorInputs::default();
            d.p1[4] = true; // A's player holds button 1 in both runs
            d.p2[3] = p2_button; // only B's player differs
            for _ in 0..300 {
                emu.step_frame(&d);
            }
            emu.gg_link_machine_a_state()
        };
        assert_ne!(
            run(false),
            run(true),
            "machine A's state must depend on machine B's inputs via the cable"
        );
        // And the same inputs reproduce the same state (sanity).
        assert_eq!(run(true), run(true));
    }
}
