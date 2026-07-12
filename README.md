# LameGear+ — multi-system BBS door

A BBS door emulator for **Sega Game Gear**, **Master System**, **SG-1000**,
**Genesis / Mega Drive**, **NES**, **Super NES**, **Game Boy Advance**, and
**PC Engine / TurboGrafx-16** — sibling to
[lameboy](https://github.com/hmderdoc/lameboy) (same door architecture,
different emulation backend). The backends are vendored
[jgenesis](https://github.com/jsgroth/jgenesis) cores (GPL-3.0) — see
`vendor/PATCH-NOTES.md` for the local patches (deterministic power-on RAM,
Gear-to-Gear serial cable).

| System | Extensions | Native | Notes |
|---|---|---|---|
| Game Gear | `.gg` | 160×144 | pixel-perfect at 162×74; `V` toggles full-frame view |
| Master System | `.sms` | 256×192 | 1-column clip fits SyncTERM's 255-col cap |
| SG-1000 | `.sg` | 256×192 | |
| Genesis | `.md` `.gen` `.smd` | 320×224 (H40) | **sysop-gated** (`genesis = 1`), H32↔H40 handled live, 6-button pad |
| NES | `.nes` | 256×224 | NTSC overscan-cropped; deterministic-RAM vendor patch |
| Super NES | `.sfc` `.smc` | 256×224 | **sysop-gated** (`snes = 1`); coprocessor carts (SA-1, Super FX) emulated |
| GBA | `.gba` | 240×160 | **sysop-gated** (`gba = 1`); commercial games need `gba_bios.bin` (16KB) beside the binary — homebrew runs without. Single-player. |
| PC Engine | `.pce` | 256×224 | **sysop-gated** (`pce = 1`); HuCards only (no CD), single-player; copier headers auto-stripped |

## Sysop quick start

1. **Install**: grab the release archive for your platform and unpack it
   into your externals directory (e.g. `/sbbs/xtrn/lamegear_plus`), or build
   from source (see Build below). The archive contains the door
   (`lamegear`), the netplay relay (`gg-link-server`), the splash screen,
   a sample config, and the sysop tools.
2. **ROMs**: drop them in `roms/` next to the binary. Extensions select the
   system (see the table). Homebrew credits for anything you bundle belong
   in `roms/CREDITS.txt`; good sources: <https://www.smspower.org/Homebrew/>,
   <https://pdroms.de/>.
3. **Config**: `cp lamegear.ini.example lamegear.ini` and edit. Every key is
   documented in the example file. The heavy systems (Genesis / SNES / GBA /
   PCE) are **off by default** — enable them after budgeting CPU (one
   Genesis caller costs ~10x an SMS caller).
4. **Synchronet**: register in `ctrl/xtrn.ini` (or via SCFG → External
   Programs):

   ```ini
   [prog:GAMES:LAMEGEAR]
   name = LameGear+
   type = 12                          ; DOOR32.SYS
   settings = 0x1004005
   cmd = ./lamegear --user %4 --dropfile %f
   startup_dir = ../xtrn/lamegear_plus
   ```

   Other BBS packages: anything that writes a DOOR32.SYS dropfile and hands
   the door the socket works; without a dropfile the door falls back to
   stdio.
5. **Netplay (optional)**: run the relay and point the ini at it:

   ```sh
   ./gg-link-server 9998 &          # or install link-server/gg-link-server.service
   # lamegear.ini:  link_server = 127.0.0.1:9998
   ```

   The GAME ROOM (multiplayer lobby, chat) appears whenever the relay is
   reachable. **InterBBS**: host the relay on a port other BBSes can reach
   and have each sysop set the same `link_server =` — callers from every
   connected board share one game room (verified over WAN-latency
   simulation; lockstep auto-adapts its input delay to the measured RTT).
   The relay speaks plaintext with no auth: firewall it to the boards you
   trust, like an FTN hub. Players must own byte-identical ROM files
   (SHA-256 checked per session) and boards should run the same door
   version.
6. **GBA BIOS (optional)**: commercial GBA games need the real 16KB BIOS as
   `gba_bios.bin` beside the binary. It is Nintendo's copyrighted code —
   dump it from your own console; it is never bundled.
7. **Box art (optional)**: `python3 tools/fetch_game_art.py --roms roms
   --output art` populates the menu's box-art previews from the Libretro
   thumbnail repos.

**Per-user data** lives under `roms/.saves/<user>/` (cartridge SRAM + one
save-state slot per game) and `~/.config/lamegear/config-<user>` (render /
color / sound / aspect / 2P-port preferences, last game). Cheat codes are
per-user sidecars next to the saves.

**Troubleshooting**: sixel geometry decisions are logged to
`sixel-debug.log` beside the binary (terminal size, every pixel-geometry
report the caller's terminal gave, and the chosen fit) — read it when a
caller reports a wrong-looking picture.

## Features

- **DOOR32.SYS** dropfile + inherited socket, with stdio fallback
- **CP437 half-block rendering** (1 column/pixel, 1 row/2 pixels, `0xDF`),
  truecolor/256/16-color with auto-probing, ASCII mode fallback
- **Sixel render mode** (strictly opt-in): real pixel graphics as one DCS per
  frame with frame de-duplication (an unchanged picture transmits nothing),
  DEC 2026 synchronized updates, and display-aspect awareness — auto targets
  the **4:3 a real console TV showed** (3:2 native for GBA), pre-widened on
  CRT-aspect-corrected terminals (SyncTERM) so the on-screen shape is right;
  callers can override to square-pixel or fill in settings. Offered on the
  settings page only when the terminal's DA reply advertises sixel; never
  auto-selected — the door's native tongue stays CP437.
- **Fit rules** (in order): native 1:1 → lossless edge clip (≤8 px columns,
  ≤16 px overscan rows) → aspect-preserving scale. Game Gear is pixel-perfect
  on a 162×74 terminal; SMS clips one column onto SyncTERM's 255-col cap.
- **Live resize** by cursor-position-report probing (a door gets no SIGWINCH)
- **Transmit fps cap** (`--fps`, default 20) with congestion frame-skipping;
  emulation never skips a frame
- **Netplay**: input-mirroring delay-based lockstep (see below), with a
  console-metaphor GAME ROOM lobby and global + per-console chat
- **Themed system carousel menu** — every console has its own color scheme
  and chrome. TAB/arrows cycle systems; BACKSPACE walks back through the
  navigation stack; type-ahead search; box-art previews (SIXEL on capable
  terminals, ANSI half-block elsewhere); per-user preferences with a full
  settings page (`S`).
- **APC streamed audio** (base64 PCM to the caller's terminal; auto-on for
  SyncTERM-class terminals, killed globally by `--mute`)
- **Cheat codes** per game (`G` in the menu), persisted per user: SMS/GG
  Game Genie `XXX-XXX-XXX` with compare-byte support; Genesis Game Genie
  `XXXX-XXXX`, Action Replay, and `ADDR:VAL` memory overrides
- **Splash screen** (80×32 CP437 `.bin`, regenerate with
  `tools/make_splash.py`) and **attract mode** (idle menu → unattended demo)
- **Per-user saves**: cartridge SRAM to `roms/.saves/<user>/`, one save-state
  slot per game (5 = save, 8 = load)
- Pure-Rust build, no native dependencies

## Netplay

**How a caller plays multiplayer** (needs the relay running — see quick
start): the **GAME ROOM** is the menu's entry view — the door always opens
onto the club's machines, not a user list (offline it's the same rack,
powered off and solo-only; `L` returns to it from any shelf). Every console
is in one of three states:

1. **Powered off** — "insert cartridge": `ENTER` walks up to that system's
   game shelf; pick a game and play. You're now P1 on that machine.
2. **P1 playing** — the row shows the cartridge and who's on pad 1:
   - `[2P OPEN]`: P1 left the second controller port open. `ENTER` sits you
     down as P2 — **the console power-cycles for both of you** (lockstep
     needs a fresh boot; it's also what real hardware made you do).
   - `[solo]`: the port is closed. `ENTER` knocks; P1 sees the knock on
     their status bar mid-game and presses `2` to let you in (`0` declines).
     Nobody can reset your game without your keypress.
3. **Both pads taken** (`[FULL]`) — pick another machine; each system shows
   a free console whenever the club still has capacity (`consoles =` ini key).

Your own 2P port is **closed by default**; toggle it with `P` in the menu or
`2` mid-game. If P1 quits a linked game, P2 inherits the machine: the same
cartridge power-cycles solo with them on pad 1. Both sides must own the same
ROM file, byte-for-byte. Callers browsing the menus sit "in the lounge" under
the machine list and can still be challenged classic-style (`ENTER` on their
name proposes your last-played game).

**Chat** (spectre conventions: `` ` `` composes, `~` shows the transcript):

- **In the menu**: `` ` `` opens the GLOBAL CHAT modal — who's here, the last
  50 lines (replayed by the relay to new connections), and a compose line.
  ESC closes; with the modal closed, new messages surface on the notice row.
- **In a solo game**: chat stays slim — incoming lines show inline on the
  bottom status row; `` ` `` opens a compose line there (ENTER sends,
  `` ` `` again stashes the draft, ESC cancels), `~` or `/history` (`/h`,
  `/last`, `/l`) opens the transcript overlay; any key closes it. The game
  keeps running underneath.
- **In a linked 2P game**: chat is **console-scoped** — only the players on
  this machine hear it (it rides the session relay like the pad inputs);
  global chat never interrupts a match.

Design follows the spec: **the Master System has no link port** — two-player
means two pads on one console — so netplay is *input mirroring*, not a cable
bridge. Both peers boot the same ROM fresh and simulate every frame; only
`{frame, slot, buttons}` messages cross the wire.

- **Two session shapes**, negotiated in the handshake: *shared console*
  (slot 0 = P1 pad, slot 1 = P2 pad — SMS/Genesis/NES/SNES two-player) and
  *Gear-to-Gear* (each peer simulates BOTH Game Gears joined by an
  in-process cable — a vendored-core UART patch implements the real serial
  hardware, verified against the official Sega hardware manual; see
  `vendor/PATCH-NOTES.md`). The shape comes from the sysop's port-trace
  cache (`roms/.link-shapes`, lines of `file<TAB>shape`); without a cache
  entry, `.gg` ROMs default to Gear-to-Gear and the rest to shared console.
- Console-centric lobby: open ports JOIN instantly, closed ports knock
  (challenge/accept underneath); games name themselves by friendly title and
  both sides must own the same ROM byte-for-byte (**SHA-256 checked in the
  session handshake**)
- Input delay `D = ceil(rtt/frame) + 1`, negotiated from a measured RTT at
  session start (typ. 2–4 frames on a LAN, 4–10 across BBSes)
- The emulation clock is the network clock: a frame executes only when both
  players' inputs for it are known; congestion skips *rendering*, never
  emulation
- Full-state **CRC32 every 60 frames**; any mismatch aborts loudly with the
  frame number and both CRCs — never diverges silently
- Sessions boot fresh with empty SRAM and no cheats; the initiator is P1
- The relay (`link-server/`, dependency-free, prebuilt in releases, systemd
  unit included) is payload-agnostic and speaks the same wire protocol as
  lameboy's, so one relay can serve both doors
- Validation: `--selftest-link` runs the two-machine gear-to-gear
  determinism gate; `--port-trace <rom>` classifies a ROM's multiplayer
  shape from its live I/O footprint; a unit test proves machine A's state
  depends on machine B's inputs *through the cable*

## Keys

| Context | Key | Action |
|---|---|---|
| menu | ←/→ / TAB | system carousel; BACKSPACE walks the nav stack back |
| menu | ↑/↓/PgUp/PgDn | choose game / console; type to search |
| menu | ENTER | insert cartridge / join / knock |
| menu | S | settings page (render, color, sound, screen, GG view, gfx aspect, 2P port) |
| menu | L / P / A / G | game room / 2P port toggle / full-screen box art / Game Genie |
| menu | `` ` `` / ~ | global chat compose / transcript |
| game | arrows | d-pad |
| game | Z / X | SMS 1/2, NES B/A, SNES B/A, GBA B/A, Genesis A/B |
| game | V | Genesis C, SNES R, GBA R |
| game | A / S / C | Genesis X/Y/Z, SNES Y/X/L (Street Fighter II lines up across systems) |
| game | SPACE | Select (NES/SNES/GBA), Genesis Mode |
| game | ENTER | Start / Pause |
| game | 5 / 8 | save / load state (solo only) |
| game | 2 / 0 | accept knock or toggle 2P port / decline knock |
| game | `` ` `` / ~ | chat compose / transcript |
| both | Q / Esc | back / quit |

## Build

```sh
cargo build --release        # builds the door AND the relay (workspace)
cp target/release/lamegear .
# relay binary: target/release/gg-link-server
```

Pure Rust, no native dependencies; release binaries are static musl builds
on Linux.

## Validation harness

```sh
./lamegear --selftest "roms/Test Cart.md" 3600    # determinism gate, per system
./lamegear --golden "roms/Test Cart.gg" 300       # framebuffer CRC at frame N
tools/golden_check.sh                    # all test carts vs tools/golden.manifest
./lamegear --replay <rom> <file.lgr>     # verify a recorded netplay session
python3 tools/latency_matrix.py          # netplay at RTT 0/50/150/300ms
./lamegear --dump "roms/Test Cart.gg" 120 /tmp/frame.ppm  # eyeball a frame
cargo test                               # unit tests
```

The selftest boots the ROM twice against an identical scripted input stream
and CRC32s the full serialized machine state every frame; any divergence
fails. Run it twice (two processes) and compare the printed vector digest for
the cross-process check. The same CRC machinery guards live netplay.

Every linked session automatically records a replay (`roms/.replays/*.lgr`:
ROM SHA-256 + per-frame input masks + end-state CRC); `--replay` re-runs it
from a fresh boot and asserts the same end state. `tools/golden_check.sh`
guards vendored-core bumps against silent rendering changes. The latency
matrix drives real door pairs through a delay-injecting relay proxy and
asserts zero desync per cell.

## Sysop tools

- `tools/fetch_game_art.py --roms roms --output art` — populate the box-art
  cache from the Libretro thumbnail repos (all eight systems)
- `tools/make_splash.py` — regenerate `lamegear_splash.bin`
- `tools/make_test_rom_md.py` / `_gba.py` / `_gglink.py` — regenerate test
  cartridges (hand-assembled 68000 / ARM7 / Z80 link-cable)
- `tools/golden_check.sh` — rendering regression gate
- `tools/latency_matrix.py` — netplay latency/jitter sweep

## Not implemented (by design or deferred)

- ANSI-music (deliberately skipped; APC covers streamed sound)
- Cheat codes for NES/SNES/GBA (their cores expose no cheat hook upstream;
  SMS/GG and Genesis are supported)
- GBA link cable (out of scope: GB↔GBA Pokémon trading never existed on
  real hardware, which removed the motivating use case — and
  [lameboy](https://github.com/hmderdoc/lameboy) already covers GB↔GB)
- RTC cartridges are non-deterministic by nature (SNES S-RTC/SPC7110, GBA
  Pokémon RSE / Boktai read wall-clock) — they play fine solo but would
  desync in netplay

## License

GPL-3.0 (see `LICENSE` and `NOTICE`): the vendored jgenesis emulation cores
are GPL-3.0, so the combined work is too. The complete corresponding source
for every release binary is this repository at the release tag. The GBA BIOS
is Nintendo's copyrighted code and is never bundled.
