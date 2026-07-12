#!/usr/bin/env python3
"""Generate a tiny deterministic Genesis / Mega Drive test ROM (68000).

Each frame: waits for a VBlank edge on the VDP status port, reads controller
1 in both TH phases ($A10003), folds the bytes into a running hash, and
writes the hash to CRAM entry 0 — which is the backdrop color, so the whole
screen changes with input. CPU/VDP/CRAM state therefore depends on the exact
per-frame input stream, which is what the determinism selftest needs.

jgenesis enforces no TMSS and no checksum; only the reset vectors matter.

Output: roms/test.md
"""
import sys
from pathlib import Path

ROM_SIZE = 0x1000  # 4KB (core mirror-pads anything >= 1KB as-is)
ENTRY = 0x200

rom = bytearray(ROM_SIZE)
code = bytearray()


def emit_w(*words):
    for w in words:
        code.extend(w.to_bytes(2, "big"))


def emit_l(value):
    code.extend(value.to_bytes(4, "big"))


VDP_CTRL = 0x00C00004
VDP_DATA = 0x00C00000
PAD1_DATA = 0x00A10003
PAD1_CTRL = 0x00A10009


def move_w_imm_absl(imm, addr):  # MOVE.W #imm,(addr).L
    emit_w(0x33FC, imm)
    emit_l(addr)


def move_l_imm_absl(imm, addr):  # MOVE.L #imm,(addr).L
    emit_w(0x23FC)
    emit_l(imm)
    emit_l(addr)


def move_b_imm_absl(imm, addr):  # MOVE.B #imm,(addr).L
    emit_w(0x13FC, imm & 0xFF)
    emit_l(addr)


# --- program at ENTRY ---
# VDP registers (control-port writes $8rvv):
move_w_imm_absl(0x8004, VDP_CTRL)  # reg 0: mode 5 flags, no H-int
move_w_imm_absl(0x8144, VDP_CTRL)  # reg 1: display ON (bit6) + mode 5 (bit2), V28
move_w_imm_absl(0x8C00, VDP_CTRL)  # reg 12: H32 (256 wide)
move_w_imm_absl(0x8F02, VDP_CTRL)  # reg 15: auto-increment 2
move_w_imm_absl(0x8700, VDP_CTRL)  # reg 7: backdrop = CRAM entry 0

# CRAM entry 0 <- white (VRAM is zeroed so tile pixels are transparent and
# the whole active area shows the backdrop).
move_l_imm_absl(0xC0000000, VDP_CTRL)  # CRAM write, address 0
move_w_imm_absl(0x0EEE, VDP_DATA)

move_b_imm_absl(0x40, PAD1_CTRL)  # TH as output
emit_w(0x7400)  # MOVEQ #0,D2 (running hash)

loop = ENTRY + len(code)

# wait for VBlank flag (status bit 3) to SET...
emit_w(0x3039)  # MOVE.W (VDP_CTRL).L,D0
emit_l(VDP_CTRL)
emit_w(0x0800, 0x0003)  # BTST #3,D0
emit_w(0x67F4)          # BEQ  -12 (back to the MOVE.W)
# ...then to CLEAR, so the loop runs exactly once per frame.
emit_w(0x3039)
emit_l(VDP_CTRL)
emit_w(0x0800, 0x0003)  # BTST #3,D0
emit_w(0x66F4)          # BNE  -12

# Controller: TH=1 phase (bits: -1CBRLDU), then TH=0 phase (-0SA00DU).
move_b_imm_absl(0x40, PAD1_DATA)
emit_w(0x1039)  # MOVE.B (PAD1_DATA).L,D0
emit_l(PAD1_DATA)
emit_w(0xD400)  # ADD.B D0,D2
move_b_imm_absl(0x00, PAD1_DATA)
emit_w(0x1039)
emit_l(PAD1_DATA)
emit_w(0xD400)  # ADD.B D0,D2

# CRAM entry 0 <- hash masked to a valid 9-bit BGR color.
emit_w(0x3202)          # MOVE.W D2,D1
emit_w(0x0241, 0x0EEE)  # ANDI.W #$0EEE,D1
move_l_imm_absl(0xC0000000, VDP_CTRL)
emit_w(0x33C1)          # MOVE.W D1,(VDP_DATA).L
emit_l(VDP_DATA)

emit_w(0x4EF9)  # JMP (loop).L
emit_l(loop)

assert ENTRY + len(code) <= ROM_SIZE, len(code)
rom[ENTRY : ENTRY + len(code)] = code

# Vector table: SSP at $0, PC at $4; point every other vector at the entry
# too (nothing raises interrupts — VINT is disabled).
rom[0:4] = (0x00FFFE00).to_bytes(4, "big")
for vec in range(1, 64):
    rom[vec * 4 : vec * 4 + 4] = ENTRY.to_bytes(4, "big")

# Header: system name (used only for byteswap detection) + region.
rom[0x100:0x110] = b"SEGA MEGA DRIVE "
rom[0x120:0x150] = b"LAMEGEAR PLUS DETERMINISM TEST CART".ljust(48)[:48]
rom[0x1F0:0x1F3] = b"JUE"

outdir = Path(sys.argv[1] if len(sys.argv) > 1 else "roms")
outdir.mkdir(parents=True, exist_ok=True)
(outdir / "test.md").write_bytes(rom)
print(f"wrote {outdir}/test.md ({len(code)} bytes of code)")
