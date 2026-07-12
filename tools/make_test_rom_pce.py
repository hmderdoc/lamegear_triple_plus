#!/usr/bin/env python3
"""Generate a tiny deterministic PC Engine test ROM (HuC6280, 8KB HuCard).

Boot: reset vector at ROM offset $1FFE -> logical $E000 (MPR7=$00 maps
logical $E000-$FFFF to the first 8KB of the HuCard). Each frame: waits for
the VDC VBlank status bit (enabled via CR bit 3 — the core only latches the
status flag when the IRQ enable is set), strobes the joypad at the I/O bank
($5000 via MPR2=$FF), folds both read phases into a running hash in work RAM
(MPR1=$F8), and writes the hash to VCE CRAM entry $100 — the overscan/border
color, so the frame changes with input.

Output: roms/test.pce
"""
import sys
from pathlib import Path

ROM_SIZE = 0x2000  # 8KB, power of two (the core mirrors it across the window)
rom = bytearray(ROM_SIZE)
code = bytearray()


def emit(*b):
    code.extend(b)


emit(0x78)              # SEI
emit(0xD4)              # CSH (7.16 MHz)
emit(0xA9, 0xFF)        # LDA #$FF
emit(0x53, 0x04)        # TAM #%00000100  (MPR2 = $FF: I/O at logical $4000)
emit(0xA9, 0xF8)        # LDA #$F8
emit(0x53, 0x02)        # TAM #%00000010  (MPR1 = $F8: work RAM at logical $2000)
emit(0x9C, 0x00, 0x44)  # STZ $4400       (VCE control: div-4, 256px)
emit(0x03, 0x05)        # ST0 #$05        (VDC register 5 = CR)
emit(0x13, 0x08)        # ST1 #$08        (CR lo: VBlank IRQ-flag enable)
emit(0x23, 0x00)        # ST2 #$00
emit(0x64, 0x00)        # STZ $00         (hash, zero page in work RAM)

loop = 0xE000 + len(code)
emit(0xAD, 0x00, 0x40)  # w1: LDA $4000   (VDC status; read clears flags)
emit(0x29, 0x20)        # AND #$20        (VBlank)
emit(0xF0, 0xF9)        # BEQ w1
# Joypad: CLR strobe, latch, read both SEL phases.
emit(0xA9, 0x03)        # LDA #$03
emit(0x8D, 0x00, 0x50)  # STA $5000       (SEL=1, CLR=1)
emit(0xA9, 0x01)        # LDA #$01
emit(0x8D, 0x00, 0x50)  # STA $5000       (CLR 1->0 latches; directions phase)
emit(0xAD, 0x00, 0x50)  # LDA $5000
emit(0x18)              # CLC
emit(0x65, 0x00)        # ADC $00
emit(0x85, 0x00)        # STA $00
emit(0xA9, 0x00)        # LDA #$00
emit(0x8D, 0x00, 0x50)  # STA $5000       (SEL=0: buttons phase)
emit(0xAD, 0x00, 0x50)  # LDA $5000
emit(0x18)              # CLC
emit(0x65, 0x00)        # ADC $00
emit(0x85, 0x00)        # STA $00
# VCE CRAM[$100] (overscan color) <- hash.
emit(0xA9, 0x00)        # LDA #$00
emit(0x8D, 0x02, 0x44)  # STA $4402       (CTA lo)
emit(0xA9, 0x01)        # LDA #$01
emit(0x8D, 0x03, 0x44)  # STA $4403       (CTA hi = $01 -> index $100)
emit(0xA5, 0x00)        # LDA $00
emit(0x8D, 0x04, 0x44)  # STA $4404       (color lo, GRB333 low bits)
emit(0xA9, 0x00)        # LDA #$00
emit(0x8D, 0x05, 0x44)  # STA $4405       (color hi)
emit(0x4C, loop & 0xFF, loop >> 8)  # JMP loop

assert len(code) < 0x1FFE, len(code)
rom[0 : len(code)] = code
# Reset vector (logical $FFFE with MPR7=$00 = ROM offset $1FFE) -> $E000.
rom[0x1FFE] = 0x00
rom[0x1FFF] = 0xE0

outdir = Path(sys.argv[1] if len(sys.argv) > 1 else "roms")
outdir.mkdir(parents=True, exist_ok=True)
(outdir / "test.pce").write_bytes(bytes(rom))
print(f"wrote {outdir}/test.pce ({len(code)} bytes of code)")
