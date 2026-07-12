//! Headless validation harness (spec 4.4).
//!
//! `--selftest <rom> [frames]`
//!   Runs the ROM twice from fresh boots against an identical scripted input
//!   stream, CRC32ing the full serialized state every frame. Any per-frame
//!   mismatch is a determinism violation and disqualifies the core. Prints a
//!   final digest over the whole CRC vector: run the command twice (separate
//!   processes) and compare digests for the cross-process variant.
//!
//! `--dump <rom> <frames> <out.ppm>`
//!   Runs N frames (no input) and writes the final framebuffer as a PPM, for
//!   eyeballing viewport, color expansion, and window crops.

use crate::emu::{DoorInputs, Emu, EmuOptions, Machine};
use crate::input::BUTTON_COUNT;
use std::path::Path;

/// Deterministic pseudo-random input stream: LCG keyed only by frame index.
fn scripted_inputs(frame: u64) -> DoorInputs {
    let mut s = frame.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    s ^= s >> 33;
    let mut p1 = [false; BUTTON_COUNT];
    p1[0] = s & 1 != 0; // up
    p1[1] = s & 2 != 0 && s & 1 == 0; // down (no opposing directions)
    p1[2] = s & 4 != 0; // left
    p1[3] = s & 8 != 0 && s & 4 == 0; // right
    p1[4] = s & 16 != 0; // button 1
    p1[5] = s & 32 != 0; // button 2
    p1[7] = s & 64 != 0; // button 3 (Genesis A / SNES Y)
    p1[8] = s & 128 != 0 && (s & 0xF00) == 0x300; // select, rarely
    p1[6] = (s & 0xFF00) == 0x4200; // start/pause, rare (exercises NMI paths)
    p1[9] = s & 256 != 0; // button 4 (SNES X)
    p1[10] = s & 512 != 0; // L shoulder
    p1[11] = s & 1024 != 0; // R shoulder
    DoorInputs::solo(p1)
}

fn machine_for(path: &Path) -> Machine {
    path.extension()
        .and_then(|e| e.to_str())
        .and_then(Machine::from_extension)
        .unwrap_or(Machine::MasterSystem)
}

fn crc_vector(rom: &[u8], machine: Machine, frames: u64) -> Result<Vec<u32>, String> {
    let mut emu = Emu::new(rom.to_vec(), machine, EmuOptions::default())?;
    let mut crcs = Vec::with_capacity(frames as usize);
    for frame in 0..frames {
        emu.step_frame(&scripted_inputs(frame));
        crcs.push(emu.state_crc32()?);
    }
    Ok(crcs)
}

/// Two scripted players (distinct LCG phases) for the gear-to-gear gate.
fn scripted_inputs_2p(frame: u64) -> DoorInputs {
    let p1 = scripted_inputs(frame).p1;
    let p2 = scripted_inputs(frame ^ 0x5A5A_5A5A).p1;
    DoorInputs { p1, p2 }
}

fn crc_vector_link(rom: &[u8], frames: u64) -> Result<Vec<u32>, String> {
    let mut emu = Emu::new_gg_link(rom.to_vec(), 0, &EmuOptions::default())?;
    let mut crcs = Vec::with_capacity(frames as usize);
    for frame in 0..frames {
        emu.step_frame(&scripted_inputs_2p(frame));
        crcs.push(emu.state_crc32()?);
    }
    Ok(crcs)
}

/// Determinism gate for the two-machine Gear-to-Gear session: both cabled
/// Game Gears, dual fresh boots, combined-state CRC per frame. Also asserts
/// the cable actually carried traffic (a link test cart that never received
/// a byte would pass vacuously).
pub fn run_selftest_link(rom_path: &Path, frames: u64) -> Result<(), String> {
    let rom = std::fs::read(rom_path).map_err(|e| format!("{}: {e}", rom_path.display()))?;
    eprintln!(
        "gear-to-gear determinism selftest: {}, {frames} frames x 2 runs",
        rom_path.display()
    );

    let a = crc_vector_link(&rom, frames)?;
    let b = crc_vector_link(&rom, frames)?;
    for (i, (ca, cb)) in a.iter().zip(b.iter()).enumerate() {
        if ca != cb {
            return Err(format!(
                "DESYNC at frame {i}: run A crc32={ca:08x}, run B crc32={cb:08x}"
            ));
        }
    }

    // Cable liveness: the game must have touched the serial ports.
    let mut emu = Emu::new_gg_link(rom, 0, &EmuOptions::default())?;
    for frame in 0..120.min(frames) {
        emu.step_frame(&scripted_inputs_2p(frame));
    }
    match emu.gg_port_trace() {
        Some(t) if t.contains(smsgg_core::PortTrace::SERIAL_PORTS) => {}
        _ => return Err("cable never used: ROM did not touch the serial ports".into()),
    }

    const CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
    let mut digest = CRC.digest();
    for c in &a {
        digest.update(&c.to_le_bytes());
    }
    println!("PASS {} frames (gear-to-gear), vector digest {:08x}", frames, digest.finalize());
    Ok(())
}

/// Port-trace classification (spec 4.4.7): run headless with scripted input
/// and report which multiplayer shape the ROM's I/O footprint implies.
/// Replaces static scanning, which can't see bank-switched or
/// computed-address code.
pub fn run_port_trace(rom_path: &Path, frames: u64) -> Result<(), String> {
    let rom = std::fs::read(rom_path).map_err(|e| format!("{}: {e}", rom_path.display()))?;
    let machine = machine_for(rom_path);
    if !matches!(
        machine,
        Machine::MasterSystem | Machine::GameGear | Machine::GameGearExpanded | Machine::Sg1000
    ) {
        return Err("port-trace applies to SMS/GG/SG-1000 ROMs".into());
    }
    let mut emu = Emu::new(rom, machine, EmuOptions::default())?;
    for frame in 0..frames {
        emu.step_frame(&scripted_inputs(frame));
    }
    let trace = emu.gg_port_trace().unwrap_or_default();
    let shape = if trace.contains(smsgg_core::PortTrace::SERIAL_PORTS)
        || trace.contains(smsgg_core::PortTrace::PARALLEL_PORTS)
    {
        "gear-to-gear"
    } else if trace.contains(smsgg_core::PortTrace::READ_DD) {
        "shared-console"
    } else {
        "single-player"
    };
    println!(
        "SHAPE {shape} file={} flags={:#06x}",
        rom_path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
        trace.bits()
    );
    Ok(())
}

pub fn run_selftest(rom_path: &Path, frames: u64) -> Result<(), String> {
    let rom = std::fs::read(rom_path).map_err(|e| format!("{}: {e}", rom_path.display()))?;
    let machine = machine_for(rom_path);
    eprintln!(
        "determinism selftest: {} ({:?}), {frames} frames x 2 runs",
        rom_path.display(),
        machine
    );

    let a = crc_vector(&rom, machine, frames)?;
    let b = crc_vector(&rom, machine, frames)?;

    for (i, (ca, cb)) in a.iter().zip(b.iter()).enumerate() {
        if ca != cb {
            return Err(format!(
                "DESYNC at frame {i}: run A crc32={ca:08x}, run B crc32={cb:08x}"
            ));
        }
    }

    // Digest over the whole vector — compare across separate processes.
    const CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
    let mut digest = CRC.digest();
    for c in &a {
        digest.update(&c.to_le_bytes());
    }
    println!("PASS {} frames, vector digest {:08x}", frames, digest.finalize());
    Ok(())
}

/// Golden-frame check (spec 4.4.6): CRC32 of the framebuffer at frame N with
/// no input. Guards against silent render regressions when a vendored core
/// is bumped — record the value, re-check after any vendor change.
pub fn golden_frame(rom_path: &Path, frame_n: u64) -> Result<(), String> {
    let rom = std::fs::read(rom_path).map_err(|e| format!("{}: {e}", rom_path.display()))?;
    let machine = machine_for(rom_path);
    let mut emu = Emu::new(rom, machine, EmuOptions::default())?;
    for _ in 0..frame_n {
        emu.step_frame(&DoorInputs::default());
    }
    let frame = emu.frame();
    const CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
    let mut digest = CRC.digest();
    for px in &frame.pixels {
        digest.update(&[px.r, px.g, px.b]);
    }
    println!(
        "GOLDEN {:08x} frame={} size={}x{} machine={:?}",
        digest.finalize(),
        frame_n,
        frame.width,
        frame.height,
        machine
    );
    Ok(())
}

/// Like `run_replay`, but also writes a PPM snapshot of the framebuffer
/// every `every` frames into `out_dir` — for eyeballing what a recorded
/// netplay session actually showed the players.
pub fn run_replay_dump(
    rom_path: &Path,
    replay_path: &Path,
    out_dir: &Path,
    every: u64,
) -> Result<(), String> {
    std::fs::create_dir_all(out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    run_replay_inner(rom_path, replay_path, Some((out_dir, every.max(1))))
}

/// Replay verification (spec 4.4.5): a netplay session records its executed
/// input stream + final state CRC; this replays it headless from a fresh
/// boot and asserts the same end state.
pub fn run_replay(rom_path: &Path, replay_path: &Path) -> Result<(), String> {
    run_replay_inner(rom_path, replay_path, None)
}

fn run_replay_inner(
    rom_path: &Path,
    replay_path: &Path,
    dump: Option<(&Path, u64)>,
) -> Result<(), String> {
    let rom = std::fs::read(rom_path).map_err(|e| format!("{}: {e}", rom_path.display()))?;
    let text = std::fs::read_to_string(replay_path)
        .map_err(|e| format!("{}: {e}", replay_path.display()))?;
    let machine = machine_for(rom_path);

    let mut lines = text.lines();
    let header = lines.next().ok_or("empty replay")?;
    if !header.starts_with("LGR1 ") {
        return Err("not a lamegear replay (LGR1)".into());
    }
    let field = |key: &str| {
        header
            .split_whitespace()
            .find_map(|t| t.strip_prefix(&format!("{key}=")))
            .map(str::to_string)
    };
    let want_sha = field("sha").ok_or("replay missing sha")?;
    use sha2::{Digest, Sha256};
    let got_sha = hex(&Sha256::digest(&rom));
    if got_sha != want_sha {
        return Err(format!("ROM mismatch: replay wants sha {want_sha}, have {got_sha}"));
    }

    // Netplay sessions boot volatile (fresh SRAM) — replays must too. A
    // gear-to-gear session replays on the same two-machine topology.
    let mut emu = if field("shape").as_deref() == Some("gear-to-gear") {
        Emu::new_gg_link(rom, 0, &EmuOptions::default())?
    } else {
        Emu::new(rom, machine, EmuOptions::default())?
    };
    let mut frames = 0u64;
    let mut end_crc: Option<u32> = None;
    for line in lines {
        if let Some(rest) = line.strip_prefix("end ") {
            end_crc = rest
                .split_whitespace()
                .find_map(|t| t.strip_prefix("crc="))
                .and_then(|v| u32::from_str_radix(v, 16).ok());
            break;
        }
        let mut parts = line.split_whitespace();
        let (p0, p1) = (
            u16::from_str_radix(parts.next().unwrap_or("0"), 16)
                .map_err(|e| e.to_string())?,
            u16::from_str_radix(parts.next().unwrap_or("0"), 16)
                .map_err(|e| e.to_string())?,
        );
        emu.step_frame(&crate::lockstep::masks_to_door_inputs(p0, p1));
        frames += 1;
        if let Some((dir, every)) = dump {
            if frames % every == 0 {
                let f = emu.frame();
                let mut ppm = format!("P6\n{} {}\n255\n", f.width, f.height).into_bytes();
                for px in &f.pixels {
                    ppm.extend_from_slice(&[px.r, px.g, px.b]);
                }
                let name = format!("frame-{frames:06}-p0_{p0:03x}-p1_{p1:03x}.ppm");
                let _ = std::fs::write(dir.join(name), ppm);
            }
        }
    }
    let final_crc = emu.state_crc32()?;
    match end_crc {
        Some(want) if want == final_crc => {
            println!("REPLAY PASS {frames} frames, state crc {final_crc:08x}");
            Ok(())
        }
        Some(want) => Err(format!(
            "REPLAY DIVERGED after {frames} frames: recorded {want:08x}, got {final_crc:08x}"
        )),
        None => {
            println!("REPLAY ran {frames} frames, state crc {final_crc:08x} (no recorded end crc)");
            Ok(())
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn dump_ppm(rom_path: &Path, frames: u64, out_path: &Path) -> Result<(), String> {
    let rom = std::fs::read(rom_path).map_err(|e| format!("{}: {e}", rom_path.display()))?;
    let machine = machine_for(rom_path);
    let mut emu = Emu::new(rom, machine, EmuOptions::default())?;
    for _ in 0..frames {
        emu.step_frame(&DoorInputs::default());
    }
    let frame = emu.frame();
    let mut ppm = format!("P6\n{} {}\n255\n", frame.width, frame.height).into_bytes();
    for px in &frame.pixels {
        ppm.extend_from_slice(&[px.r, px.g, px.b]);
    }
    std::fs::write(out_path, ppm).map_err(|e| format!("{}: {e}", out_path.display()))?;
    println!(
        "wrote {} ({}x{}, {:?}, {} frames)",
        out_path.display(),
        frame.width,
        frame.height,
        machine,
        frames
    );
    Ok(())
}
