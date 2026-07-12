#!/usr/bin/env python3
"""Generate a tiny deterministic NES test ROM (mapper 0 / NROM-128).

Each frame: waits for VBlank, reads joypad 1 ($4016 strobe + 8 serial reads),
folds the byte into a running hash in zero page, and writes the hash to the
universal background color ($3F00). CPU/PPU/RAM state therefore depends on
the exact per-frame input stream — what the determinism selftest needs.

Output: roms/test.nes
"""
import sys
from pathlib import Path

PRG_SIZE = 0x4000  # 16KB, loaded at $8000 and mirrored at $C000
CHR_SIZE = 0x2000  # 8KB CHR ROM (blank patterns)

prg = bytearray([0xEA] * PRG_SIZE)  # NOP padding
code = bytearray()


def emit(*b):
    code.extend(b)


# --- reset entry at $8000 ---
emit(0x78)              # SEI
emit(0xD8)              # CLD
emit(0xA2, 0xFF)        # LDX #$FF
emit(0x9A)              # TXS
emit(0xA9, 0x00)        # LDA #$00
emit(0x8D, 0x00, 0x20)  # STA $2000  (NMI off)
emit(0x8D, 0x01, 0x20)  # STA $2001  (rendering off)
emit(0x85, 0x00)        # STA $00    (last pad byte)
emit(0x85, 0x01)        # STA $01    (running hash)

# Wait two VBlanks so the PPU is warmed up.
for _ in range(2):
    # wait: BIT $2002 / BPL wait   (loop until bit 7 set)
    emit(0x2C, 0x02, 0x20)
    emit(0x10, 0xFB)

# Palette: universal background = $21 (light blue) at $3F00.
emit(0xA9, 0x3F)        # LDA #$3F
emit(0x8D, 0x06, 0x20)  # STA $2006
emit(0xA9, 0x00)        # LDA #$00
emit(0x8D, 0x06, 0x20)  # STA $2006
emit(0xA9, 0x21)        # LDA #$21
emit(0x8D, 0x07, 0x20)  # STA $2007

# --- main per-frame loop ---
loop_addr = 0x8000 + len(code)
emit(0x2C, 0x02, 0x20)  # BIT $2002
emit(0x10, 0xFB)        # BPL -5     (wait for VBlank flag)
# Strobe joypad 1.
emit(0xA9, 0x01)        # LDA #$01
emit(0x8D, 0x16, 0x40)  # STA $4016
emit(0xA9, 0x00)        # LDA #$00
emit(0x8D, 0x16, 0x40)  # STA $4016
# Shift 8 buttons into $00.
emit(0xA2, 0x08)        # LDX #$08
emit(0xAD, 0x16, 0x40)  # rb: LDA $4016
emit(0x4A)              # LSR A       (bit 0 -> carry)
emit(0x26, 0x00)        # ROL $00
emit(0xCA)              # DEX
emit(0xD0, 0xF7)        # BNE rb
# hash += pad byte
emit(0xA5, 0x00)        # LDA $00
emit(0x18)              # CLC
emit(0x65, 0x01)        # ADC $01
emit(0x85, 0x01)        # STA $01
# Backdrop color <- hash (masked to a safe palette index range).
emit(0xA9, 0x3F)        # LDA #$3F
emit(0x8D, 0x06, 0x20)  # STA $2006
emit(0xA9, 0x00)        # LDA #$00
emit(0x8D, 0x06, 0x20)  # STA $2006
emit(0xA5, 0x01)        # LDA $01
emit(0x29, 0x33)        # AND #$33   (avoid $0D "blacker than black")
emit(0x8D, 0x07, 0x20)  # STA $2007
emit(0x4C, loop_addr & 0xFF, loop_addr >> 8)  # JMP loop

assert len(code) < PRG_SIZE - 6, len(code)
prg[0 : len(code)] = code

# RTI stub for NMI/IRQ.
rti_addr = 0x8000 + PRG_SIZE - 8
prg[PRG_SIZE - 8] = 0x40  # RTI
# Vectors: NMI, RESET, IRQ.
for off, vec in [(0x3FFA, rti_addr), (0x3FFC, 0x8000), (0x3FFE, rti_addr)]:
    prg[off] = vec & 0xFF
    prg[off + 1] = vec >> 8

header = bytearray(16)
header[0:4] = b"NES\x1a"
header[4] = 1  # 1 x 16KB PRG
header[5] = 1  # 1 x 8KB CHR
# flags 6/7 = 0: mapper 0, horizontal mirroring, no battery

rom = bytes(header) + bytes(prg) + bytes([0x00] * CHR_SIZE)
outdir = Path(sys.argv[1] if len(sys.argv) > 1 else "roms")
outdir.mkdir(parents=True, exist_ok=True)
(outdir / "test.nes").write_bytes(rom)
print(f"wrote {outdir}/test.nes ({len(code)} bytes of code)")
