#!/usr/bin/env python3
"""Generate a tiny deterministic SMS/GG test ROM (no assembler required).

The ROM:
  - initializes the VDP in mode 4, display on, no interrupts
  - writes 8 distinct CRAM colors and 8 solid tiles
  - fills the name table with vertical stripes (tile = column & 7)
  - each frame: polls VBlank, reads joypad port $DC, folds it into a running
    hash in RAM, writes the hash to CRAM entry 16 and to the PSG ($7F)

State (CPU regs, RAM, VRAM, CRAM, PSG) therefore depends on the exact
per-frame input stream, which is what the determinism selftest needs.

Outputs roms/test.sms and roms/test.gg (identical code, different region
byte in the TMR SEGA header).
"""
import sys
from pathlib import Path

ROM_SIZE = 0x8000

rom = bytearray([0x00] * ROM_SIZE)  # NOP padding
code = bytearray()


def emit(*b):
    code.extend(b)


def out_bf(val):  # LD A,val / OUT ($BF),A
    emit(0x3E, val, 0xD3, 0xBF)


def out_be(val):  # LD A,val / OUT ($BE),A
    emit(0x3E, val, 0xD3, 0xBE)


# --- reset vector at 0x0000 ---
boot = bytes([
    0xF3,              # DI
    0xED, 0x56,        # IM 1
    0x31, 0xF0, 0xDF,  # LD SP,$DFF0
    0xC3, 0x80, 0x00,  # JP $0080
])
rom[0:len(boot)] = boot
rom[0x0038:0x003A] = bytes([0xED, 0x4D])  # RETI (INT, never enabled)
rom[0x0066:0x0068] = bytes([0xED, 0x45])  # RETN (NMI = pause button)

# --- main program, assembled into `code`, placed at 0x0080 ---

# 1. VDP registers: (value, register)
for val, reg in [
    (0x04, 0),   # mode 4
    (0x40, 1),   # display on, 192 lines, no vblank IRQ
    (0xFF, 2),   # name table $3800
    (0xFF, 3), (0xFF, 4),  # legacy color/pattern (mode 2 relics)
    (0xFF, 5),   # sprite attribute table $3F00
    (0xFF, 6),   # sprite patterns $2000
    (0x00, 7),   # backdrop color 0
    (0x00, 8), (0x00, 9),  # scroll x/y
    (0xFF, 10),  # line IRQ off
]:
    out_bf(val)
    out_bf(0x80 | reg)

# 2. CRAM: 32 entries, 8 distinct colors repeated (SMS format --BBGGRR)
PALETTE = [0x00, 0x03, 0x0C, 0x30, 0x0F, 0x33, 0x3C, 0x3F]
out_bf(0x00)
out_bf(0xC0)  # CRAM address 0
for i in range(32):
    out_be(PALETTE[i % 8])

# 3. Pattern table, VRAM $0000: tiles 0..7, tile i solid color index i.
#    Mode 4 row = 4 bitplane bytes.
out_bf(0x00)
out_bf(0x40)  # VRAM address 0, write mode
for tile in range(8):
    for _row in range(8):
        for plane in range(4):
            out_be(0xFF if (tile >> plane) & 1 else 0x00)

# 4. Name table, VRAM $3800: 24 rows x 32 cols, entry = (col & 7), flags 0.
out_bf(0x00)
out_bf(0x78)  # $3800 | write
for _row in range(24):
    for col in range(32):
        out_be(col & 7)   # pattern index low byte
        out_be(0x00)      # flags/high byte

# 5. Per-frame loop.
loop_top = 0x0080 + len(code)
emit(
    # wait for VBlank flag (reading $BF clears it -> once per frame)
    0xDB, 0xBF,        # IN A,($BF)
    0xE6, 0x80,        # AND $80
    0x28, 0xFA,        # JR Z,-6 (back to IN)
    # sample joypad
    0xDB, 0xDC,        # IN A,($DC)
    0x32, 0x00, 0xC0,  # LD ($C000),A
    0x47,              # LD B,A
    0x3A, 0x01, 0xC0,  # LD A,($C001)
    0x80,              # ADD A,B
    0x32, 0x01, 0xC0,  # LD ($C001),A
    # CRAM entry 16 <- hash & $3F
    0x3E, 0x10,        # LD A,$10
    0xD3, 0xBF,        # OUT ($BF),A
    0x3E, 0xC0,        # LD A,$C0
    0xD3, 0xBF,        # OUT ($BF),A
    0x3A, 0x01, 0xC0,  # LD A,($C001)
    0xE6, 0x3F,        # AND $3F
    0xD3, 0xBE,        # OUT ($BE),A
    # PSG channel 0 tone latch <- hash & $0F ; volume 0
    0x3A, 0x01, 0xC0,  # LD A,($C001)
    0xE6, 0x0F,        # AND $0F
    0xF6, 0x80,        # OR $80
    0xD3, 0x7F,        # OUT ($7F),A
    0x3E, 0x90,        # LD A,$90
    0xD3, 0x7F,        # OUT ($7F),A
)
# JP loop_top (absolute, avoids relative-range concerns)
emit(0xC3, loop_top & 0xFF, loop_top >> 8)

assert 0x0080 + len(code) < 0x7FF0, f"code too big: {len(code)}"
rom[0x0080:0x0080 + len(code)] = code

def finalize(region_size: int) -> bytes:
    r = bytearray(rom)
    r[0x7FF0:0x7FF8] = b"TMR SEGA"
    r[0x7FF8:0x7FFA] = b"  "
    r[0x7FFC:0x7FFF] = bytes([0x00, 0x00, 0x00])
    r[0x7FFF] = region_size
    checksum = sum(r[0:0x7FF0]) & 0xFFFF
    r[0x7FFA] = checksum & 0xFF
    r[0x7FFB] = checksum >> 8
    return bytes(r)

outdir = Path(sys.argv[1] if len(sys.argv) > 1 else "roms")
outdir.mkdir(parents=True, exist_ok=True)
(outdir / "test.sms").write_bytes(finalize(0x4C))  # SMS export, 32KB
(outdir / "test.gg").write_bytes(finalize(0x6C))   # GG export, 32KB
print(f"wrote {outdir}/test.sms and {outdir}/test.gg ({len(code)} bytes of code)")
