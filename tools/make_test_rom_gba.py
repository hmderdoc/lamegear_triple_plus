#!/usr/bin/env python3
"""Generate a tiny deterministic GBA test ROM (ARM7, mode 3 bitmap).

Runs with the door's `skip_bios_animation` boot (entry at 0x08000000, file
offset 0 — no header/logo needed, and no BIOS SWI calls so the zeroed dummy
BIOS works). Each frame: waits for a VBlank edge on DISPSTAT, reads KEYINPUT,
folds it into a running hash, and fills the top 16 bitmap rows with the hash
color. CPU/VRAM state therefore depends on the exact per-frame input stream.

Output: roms/test.gba
"""
import sys
from pathlib import Path

words = [
    0xE3A00404,  # mov  r0, #0x04000000      (MMIO base)
    0xE3A01B01,  # mov  r1, #0x400
    0xE3811003,  # orr  r1, r1, #3
    0xE5801000,  # str  r1, [r0]             DISPCNT = mode 3 | BG2
    0xE3A04000,  # mov  r4, #0               (hash)
    # loop (0x14):
    0xE5901004,  # ldr  r1, [r0, #4]         DISPSTAT
    0xE3110001,  # tst  r1, #1
    0x1AFFFFFC,  # bne  loop                 (wait until NOT vblank)
    0xE5901004,  # w2: ldr r1, [r0, #4]
    0xE3110001,  # tst  r1, #1
    0x0AFFFFFC,  # beq  w2                   (wait for vblank)
    0xE5901130,  # ldr  r1, [r0, #0x130]     KEYINPUT (active low)
    0xE0844001,  # add  r4, r4, r1
    0xE3C43C80,  # bic  r3, r4, #0x8000      (keep BGR555 bit15 clear)
    0xE1833803,  # orr  r3, r3, r3, lsl #16  (two pixels per word)
    0xE3A02406,  # mov  r2, #0x06000000      VRAM
    0xE1A06002,  # mov  r6, r2
    0xE3A05D1E,  # mov  r5, #0x780           (1920 words = 16 rows)
    # fill (0x48):
    0xE4863004,  # str  r3, [r6], #4
    0xE2555001,  # subs r5, r5, #1
    0x1AFFFFFC,  # bne  fill
    0xEAFFFFEE,  # b    loop
]

rom = bytearray()
for w in words:
    rom.extend(w.to_bytes(4, "little"))
rom.extend(b"\x00" * (0x1000 - len(rom)))  # pad to 4KB

outdir = Path(sys.argv[1] if len(sys.argv) > 1 else "roms")
outdir.mkdir(parents=True, exist_ok=True)
(outdir / "test.gba").write_bytes(rom)
print(f"wrote {outdir}/test.gba ({len(words) * 4} bytes of code)")
