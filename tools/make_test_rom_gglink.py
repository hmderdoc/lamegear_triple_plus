#!/usr/bin/env python3
"""Generate a deterministic Gear-to-Gear LINK test ROM.

Like the plain GG test cart, but each frame it also:
  - transmits its input hash over the serial port ($03, 4800 baud, TON|RON)
  - polls RXRD ($05 bit 1) and folds any received byte into a second hash,
    written to CRAM entry 17

So each machine's state depends on the OTHER machine's input stream — the
cable itself is inside the determinism/netplay test loop.

Output: roms/test-link.gg
"""
import sys
from pathlib import Path

ROM_SIZE = 0x8000
rom = bytearray([0x00] * ROM_SIZE)
code = bytearray()


def emit(*b):
    code.extend(b)


def out_bf(val):
    emit(0x3E, val, 0xD3, 0xBF)


def out_be(val):
    emit(0x3E, val, 0xD3, 0xBE)


boot = bytes([0xF3, 0xED, 0x56, 0x31, 0xF0, 0xDF, 0xC3, 0x80, 0x00])
rom[0 : len(boot)] = boot
rom[0x0038:0x003A] = bytes([0xED, 0x4D])
rom[0x0066:0x0068] = bytes([0xED, 0x45])

# VDP init (same as the plain test cart).
for val, reg in [
    (0x04, 0), (0x40, 1), (0xFF, 2), (0xFF, 3), (0xFF, 4), (0xFF, 5),
    (0xFF, 6), (0x00, 7), (0x00, 8), (0x00, 9), (0xFF, 10),
]:
    out_bf(val)
    out_bf(0x80 | reg)

PALETTE = [0x00, 0x03, 0x0C, 0x30, 0x0F, 0x33, 0x3C, 0x3F]
out_bf(0x00)
out_bf(0xC0)
for i in range(32):
    out_be(PALETTE[i % 8])
out_bf(0x00)
out_bf(0x40)
for tile in range(8):
    for _row in range(8):
        for plane in range(4):
            out_be(0xFF if (tile >> plane) & 1 else 0x00)
out_bf(0x00)
out_bf(0x78)
for _row in range(24):
    for col in range(32):
        out_be(col & 7)
        out_be(0x00)

# Serial: 4800 baud (BS=00), RON|TON (receive + send enable), no NMI.
emit(0x3E, 0x30, 0xD3, 0x05)  # LD A,$30 / OUT ($05),A

loop_top = 0x0080 + len(code)
emit(
    # once per frame (VBlank flag poll clears it)
    0xDB, 0xBF,        # IN A,($BF)
    0xE6, 0x80,        # AND $80
    0x28, 0xFA,        # JR Z,-6
    # joypad -> hash at $C001
    0xDB, 0xDC,        # IN A,($DC)
    0x32, 0x00, 0xC0,  # LD ($C000),A
    0x47,              # LD B,A
    0x3A, 0x01, 0xC0,  # LD A,($C001)
    0x80,              # ADD A,B
    0x32, 0x01, 0xC0,  # LD ($C001),A
    # transmit the hash to the peer
    0xD3, 0x03,        # OUT ($03),A
    # receive: RXRD set?
    0xDB, 0x05,        # IN A,($05)
    0xE6, 0x02,        # AND $02
    0x28, 0x0A,        # JR Z,+10 (skip the receive fold below)
    0xDB, 0x04,        # IN A,($04)   (clears RXRD)
    0x47,              # LD B,A
    0x3A, 0x02, 0xC0,  # LD A,($C002)
    0x80,              # ADD A,B
    0x32, 0x02, 0xC0,  # LD ($C002),A
    # CRAM entry 16 <- own hash
    0x3E, 0x20, 0xD3, 0xBF,  # LD A,$20 / OUT ($BF),A   (byte address 32)
    0x3E, 0xC0, 0xD3, 0xBF,  # LD A,$C0 / OUT ($BF),A
    0x3A, 0x01, 0xC0,        # LD A,($C001)
    0xE6, 0x3F,              # AND $3F
    0xD3, 0xBE,              # OUT ($BE),A
    # CRAM entry 17 <- peer hash (cross-machine state coupling)
    0x3E, 0x22, 0xD3, 0xBF,  # LD A,$22 / OUT ($BF),A
    0x3E, 0xC0, 0xD3, 0xBF,
    0x3A, 0x02, 0xC0,        # LD A,($C002)
    0xE6, 0x3F,
    0xD3, 0xBE,
)
emit(0xC3, loop_top & 0xFF, loop_top >> 8)

assert 0x0080 + len(code) < 0x7FF0
rom[0x0080 : 0x0080 + len(code)] = code

r = bytearray(rom)
r[0x7FF0:0x7FF8] = b"TMR SEGA"
r[0x7FF8:0x7FFA] = b"  "
r[0x7FFF] = 0x6C  # GG export, 32KB
checksum = sum(r[0:0x7FF0]) & 0xFFFF
r[0x7FFA] = checksum & 0xFF
r[0x7FFB] = checksum >> 8

outdir = Path(sys.argv[1] if len(sys.argv) > 1 else "roms")
outdir.mkdir(parents=True, exist_ok=True)
(outdir / "test-link.gg").write_bytes(bytes(r))
print(f"wrote {outdir}/test-link.gg ({len(code)} bytes of code)")
