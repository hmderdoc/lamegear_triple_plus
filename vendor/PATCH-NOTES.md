# Vendored jgenesis crates — patch notes

Crates vendored from https://github.com/jsgroth/jgenesis (GPL-3.0 license, see
`LICENSE-jgenesis`), at the upstream commit recorded in `UPSTREAM-COMMIT`.

Vendored crates (path-dependency closure of `smsgg-core`, headless only —
`jgenesis-native-driver`, `jgenesis-renderer`, CLI and GUI frontends are
deliberately left behind, as they pull SDL3 and wgpu):

- `smsgg-core` — SMS / Game Gear / SG-1000 emulation backend
- `smsgg-config` — config types for the above
- `z80-emu` — Z80 CPU core
- `ym-opll` — YM2413 FM sound (SMS FM expansion)
- `jgenesis-common` — shared frontend/backend traits (EmulatorTrait, Renderer, …)
- `jgenesis-proc-macros` — derive macros used by the above
- `dsp` — audio resampling / filtering

Second vendoring round (same upstream commit) added the 16-bit/NES closure:

- `genesis-core`, `genesis-config`, `m68000-emu` — Sega Genesis / Mega Drive
- `nes-core`, `nes-config`, `mos6502-emu` — Nintendo Entertainment System

## Local patches

### nes-core: deterministic power-on RAM (`src/bus.rs`)

Upstream fills the 2KB CPU internal RAM with per-byte `rand::random()`
`0x00`/`0xFF` at construction (`Bus::from_cartridge`). That violates the
door's determinism contract — netplay lockstep and the CRC32 selftest need
an identical power-on RAM pattern on every boot on every machine (design
spec §4.3.5). Patched to a fixed 4-on/4-off `0x00`/`0xFF` fill (similar to
well-known NES power-up states, so games that peek at uninitialized RAM
still see plausible dirt — just the same dirt everywhere). The upstream
unit test asserting two boots differ now asserts the opposite, and the
now-unused `rand` dependency was removed from `nes-core/Cargo.toml`.

Third vendoring round (same upstream commit) added the SNES/GBA closure:

- `snes-core`, `snes-config`, `snes-coprocessors`, `wdc65816-emu`,
  `spc700-emu` — Super Nintendo
- `gba-core`, `gba-config`, `arm7tdmi-emu` — Game Boy Advance
- `gb-core`, `gb-config` — vendored only because gba-core reuses the GB
  APU channel types (`gb_core::apu::*`); the door does not emulate GB
  (lameboy covers it)

### snes-core: deterministic power-on RAM (`src/memory.rs`)

Same patch as nes-core, same reason: upstream fills the 128KB main RAM
with `rand::random()` bytes at power-on. Replaced with the fixed
4-on/4-off `0x00`/`0xFF` fill; `rand` removed from `snes-core/Cargo.toml`.

### smsgg-core: Game Gear serial port (Gear-to-Gear) + I/O port trace

Files: `src/memory/serial.rs` (new), `src/memory.rs`, `src/bus.rs`,
`src/api.rs`, `src/lib.rs`.

Reason: the door needs (a) real Gear-to-Gear link-cable emulation so two
players' emulator instances can play GG link games over the BBS, and (b) a
cheap I/O access trace to classify a ROM's multiplayer shape (shared-console
vs. gear-to-gear vs. single-player). Upstream stubs the GG serial ports:
reads of $04/$06 returned 0xFF, $03/$05 returned 0x00, and writes to
$03-$05 were dropped.

Behavior (register semantics verified against the official *Sega Game Gear
Hardware Reference Manual*, "System control port" I/O ports 00H-06H —
https://segaretro.org/images/1/16/Sega_Game_Gear_Hardware_Reference_Manual.pdf ,
cross-checked against SMS Power's development docs
(https://www.smspower.org/Development/GearToGearCable , smstech notes) and
MEKA's `commport` documentation):

- Port $03 (R/W) TX data, $04 (R) RX data, $05 (R/W) serial control/status:
  `BS1 BS0 RON TON INT | FRER RXRD TXFL` (D7..D0; D2-D0 read-only status).
  Baud select BS1:BS0 = 00/01/10/11 -> 4800/2400/1200/300 bps. TON forces
  PC4 output (send enable), RON forces PC5 input (receive enable), INT
  raises an NMI when a byte is received.
- Transmission and reception are timed in emulated Z80 T-cycles only
  (3,579,545 Hz, 10-bit 8N1 frames): 7,457 / 14,915 / 29,830 / 119,318
  cycles per byte at 4800/2400/1200/300 bps. The engine is ticked from
  `SmsGgEmulator::tick` alongside the VDP/PSG (GG mode only). No wall
  clock, no rand — determinism contract preserved.
- Pluggable cable on `SmsGgEmulator`: `serial_take_tx() -> Option<u8>`
  (byte finished transmitting), `serial_deliver_rx(byte)` (byte from peer;
  lands after one frame time at the local baud, sets RXRD, optional NMI),
  `serial_active() -> bool` (game touched $03-$05). With no cable attached
  the behavior matches standalone hardware: TX drains into the void after
  the baud delay, RX never becomes ready.
- Overrun (unread RXRD when the next byte completes) overwrites the RX
  buffer; the hardware has no overrun flag and FRER is strictly a framing
  error, which we raise only when a delivery starts while another frame is
  mid-reception. Reading $04 clears RXRD/FRER and acks the NMI (the manual
  does not document the clear mechanism; standard UART semantics assumed).
- `PortTrace` (exported from `lib.rs`): O(1) OR-only bitflags — READ_DC,
  READ_DD, SERIAL_PORTS ($03-$05), PARALLEL_PORTS ($01-$02) — accumulated
  in the bus I/O dispatch, readable via `SmsGgEmulator::port_trace()`.
  Included in save states (it lives inside `Memory`).
- Save-state format note: `Memory` gained bincode fields (`GgSerial`,
  `PortTrace`), so save states from the unpatched build do not load.
- SMS/SG-1000 behavior unchanged; GG games that never touch $03-$05 see
  bit-identical behavior (initial readback values match the old stubs), so
  the bundled determinism selftest results are unaffected.

Unit tests in `src/memory/serial.rs` cover back-to-back exchange at all
four baud rates, TX timing/double-buffering, overrun/framing behavior,
RON/TON gating, receive-NMI, and trace flags.

Fourth vendoring round (same upstream commit) added the PC Engine closure:
`pce-core`, `pce-config`, `huc6280-emu`.

### pce-core + huc6280-emu: deterministic power-on state

Same contract as the NES/SNES patches, three sites:

- `huc6280-emu/src/lib.rs`: `Flags::random()` and `Registers::random()`
  replaced with fixed plausible power-on values (I=1, A/X/Y/S=0xFF,
  MPR=0xFF; the reset sequence overwrites PC and MPR7 regardless). The
  now-unused `rand` dependency was removed from its Cargo.toml.
- `pce-core/src/memory.rs`: working RAM `BoxedByteArray::new_random()` →
  fixed 4-on/4-off 0x00/0xFF fill.
- `pce-core/src/video/vdc.rs`: VRAM `BoxedWordArray::new_random()` →
  fixed 0x0000/0xFFFF fill.

### Known, unpatched nondeterminism (out of scope)

- `snes-coprocessors` S-RTC and SPC7110 RTC read wall-clock time — this
  only affects the two obscure Japanese RTC cartridges, and coprocessor
  carts are design-spec non-goals. Netplay with such a cart would desync.
- `gb-core`'s RAM/palette/HuC3 randomization is left pristine: the GBA
  path only uses its APU types, which never construct those.

Everything else builds unmodified on stable Rust >= 1.97. `genesis-core`,
`m68000-emu`, `arm7tdmi-emu`, `wdc65816-emu`, and `spc700-emu` audited
clean: zero-filled or patched RAM, no rand, no wall-clock in emulation
paths, deterministic open-bus latching.

## Workspace shims

The vendored crates declare `version = { workspace = true }`,
`[lints] workspace = true`, and `{ workspace = true }` dependencies. The root
`Cargo.toml` of lamegear supplies those workspace tables, mirroring upstream
versions. Upstream's clippy lint set is not replicated (we don't fail our
build on upstream pedantic lints).
