#!/usr/bin/env python3
"""Generate a tiny deterministic SNES test ROM (LoROM, 32KB, 65816).

Each frame: waits for a VBlank edge on $4212, waits for the auto-joypad read
to finish, folds $4218/$4219 (pad 1) into a running hash in WRAM, and writes
the hash to CGRAM entry 0 — the backdrop color, so the whole screen changes
with input. First opcode is SEI so jgenesis's LoROM reset-vector heuristic
scores the header; the checksum is not verified.

Output: roms/test.sfc
"""
import sys
from pathlib import Path

ROM_SIZE = 0x8000
ENTRY = 0x8000  # CPU address; file offset 0 maps to $00:8000 in LoROM

rom = bytearray(ROM_SIZE)
code = bytearray()


def emit(*b):
    code.extend(b)


emit(0x78)              # SEI  (first opcode: header-detection heuristic)
emit(0x18)              # CLC
emit(0xFB)              # XCE  (native mode)
emit(0xE2, 0x30)        # SEP #$30 (A/X/Y 8-bit)
emit(0xA9, 0x8F)        # LDA #$8F
emit(0x8D, 0x00, 0x21)  # STA $2100  (force blank)
# CGRAM entry 0 <- red ($001F BGR555)
emit(0x9C, 0x21, 0x21)  # STZ $2121
emit(0xA9, 0x1F)        # LDA #$1F
emit(0x8D, 0x22, 0x21)  # STA $2122  (low byte)
emit(0x9C, 0x22, 0x21)  # STZ $2122  (high byte)
emit(0xA9, 0x0F)        # LDA #$0F
emit(0x8D, 0x00, 0x21)  # STA $2100  (blank off, full brightness)
emit(0xA9, 0x01)        # LDA #$01
emit(0x8D, 0x00, 0x42)  # STA $4200  (auto-joypad on, NMI off)
emit(0x64, 0x00)        # STZ $00    (hash)

loop = ENTRY + len(code)
# Wait until OUT of vblank, then IN: exactly one iteration per frame.
emit(0xAD, 0x12, 0x42)  # w1: LDA $4212
emit(0x29, 0x80)        # AND #$80
emit(0xD0, 0xF9)        # BNE w1
emit(0xAD, 0x12, 0x42)  # w2: LDA $4212
emit(0x29, 0x80)        # AND #$80
emit(0xF0, 0xF9)        # BEQ w2
emit(0xAD, 0x12, 0x42)  # w3: LDA $4212
emit(0x29, 0x01)        # AND #$01
emit(0xD0, 0xF9)        # BNE w3 (auto-joypad read in progress)
# hash += $4218 + $4219
emit(0xAD, 0x18, 0x42)  # LDA $4218
emit(0x18)              # CLC
emit(0x65, 0x00)        # ADC $00
emit(0x85, 0x00)        # STA $00
emit(0xAD, 0x19, 0x42)  # LDA $4219
emit(0x65, 0x00)        # ADC $00
emit(0x85, 0x00)        # STA $00
# CGRAM entry 0 <- hash (high byte masked to keep BGR555 bit15 clear)
emit(0x9C, 0x21, 0x21)  # STZ $2121
emit(0xA5, 0x00)        # LDA $00
emit(0x8D, 0x22, 0x21)  # STA $2122
emit(0xA5, 0x00)        # LDA $00
emit(0x29, 0x7F)        # AND #$7F
emit(0x8D, 0x22, 0x21)  # STA $2122
emit(0x4C, loop & 0xFF, (loop >> 8) & 0xFF)  # JMP loop

assert len(code) < 0x7FC0, len(code)
rom[0 : len(code)] = code

# LoROM header at file offset $7FC0.
rom[0x7FC0:0x7FD5] = b"LAMEGEAR TEST CART   "
rom[0x7FD5] = 0x20  # map: LoROM, slow
rom[0x7FD6] = 0x00  # chipset: ROM only
rom[0x7FD7] = 0x05  # size: 32KB
rom[0x7FD8] = 0x00  # no SRAM
rom[0x7FD9] = 0x00  # region: Japan/NTSC
rom[0x7FDA] = 0x33
rom[0x7FDC:0x7FDE] = b"\xFF\xFF"  # checksum complement (unverified)
rom[0x7FDE:0x7FE0] = b"\x00\x00"  # checksum (unverified)

# Emulation-mode vectors ($FFE0-$FFFF -> file $7FE0-$7FFF): point everything
# at the entry; only RESET ($7FFC) matters with interrupts disabled.
for off in range(0x7FE0, 0x8000, 2):
    rom[off] = ENTRY & 0xFF
    rom[off + 1] = (ENTRY >> 8) & 0xFF

outdir = Path(sys.argv[1] if len(sys.argv) > 1 else "roms")
outdir.mkdir(parents=True, exist_ok=True)
(outdir / "test.sfc").write_bytes(rom)
print(f"wrote {outdir}/test.sfc ({len(code)} bytes of code)")
