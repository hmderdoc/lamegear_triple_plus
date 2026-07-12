#!/usr/bin/env python3
"""Generate lamegear_splash.bin — the 80x32 CP437 startup splash.

Format (what src/splash.rs consumes): 80x32 cells, two bytes per cell
(CP437 char, then attribute), row-major. Attribute = (bg << 4) | fg with
fg 0-15 and bg 0-7 (classic VGA text attributes, no blink bit used).
A standard SAUCE trailer (DataType 5 BinaryText, FileType 40) is appended;
the loader tolerates and ignores it.

Design notes:
  - The whole canvas is Sega-blue (VGA 1) so it blends with splash.rs's
    surrounding BLUE fill on terminals larger than 80x32.
  - Terminals shorter than 32 rows show the BOTTOM rows (splash.rs
    bottom-aligns), so rows 0-7 are decorative garnish only; the wordmark,
    console panels and the press-any-key hint all live in rows 8-31.

Usage:
  tools/make_splash.py            # writes lamegear_splash.bin next to Cargo.toml
  tools/make_splash.py --preview  # also print the result as ANSI to stdout
  tools/make_splash.py --out PATH # explicit output path
"""

import argparse
import os
import random
import struct
import sys

W, H = 80, 32

# VGA attribute palette indices (attribute order: 1=blue, 4=red).
BLACK, BLUE, GREEN, CYAN, RED, MAGENTA, BROWN, GRAY = range(8)
DGRAY, BBLUE, BGREEN, BCYAN, BRED, BMAG, YELLOW, WHITE = range(8, 16)

# Canvas: (char, fg, bg) per cell, all Sega-blue to start.
cells = [[(' ', GRAY, BLUE) for _ in range(W)] for _ in range(H)]


def put(r, c, ch, fg, bg=None):
    if 0 <= r < H and 0 <= c < W:
        cells[r][c] = (ch, fg, cells[r][c][2] if bg is None else bg)


def text(r, c, s, fg, bg=None):
    for i, ch in enumerate(s):
        put(r, c + i, ch, fg, bg)


def ctext(r, s, fg, bg=None):
    text(r, (W - len(s)) // 2, s, fg, bg)


def fill(r0, c0, r1, c1, ch, fg, bg):
    for r in range(r0, r1 + 1):
        for c in range(c0, c1 + 1):
            put(r, c, ch, fg, bg)


# ---------------------------------------------------------------------------
# Block-letter font (5 rows). '#' becomes a full block 0xDB; the wordmark then
# gets half-block top/bottom trims (0xDC/0xDF) for a chamfered look.
# ---------------------------------------------------------------------------
FONT = {
    'L': ["##.....",
          "##.....",
          "##.....",
          "##.....",
          "#######"],
    'A': [".#####.",
          "##...##",
          "#######",
          "##...##",
          "##...##"],
    'M': ["##...##",
          "###.###",
          "##.#.##",
          "##...##",
          "##...##"],
    'E': ["#######",
          "##.....",
          "#####..",
          "##.....",
          "#######"],
    'G': [".######",
          "##.....",
          "##..###",
          "##...##",
          ".#####."],
    'R': ["######.",
          "##...##",
          "######.",
          "##.##..",
          "##..##."],
    '+': ["......",
          "..##..",
          "######",
          "..##..",
          "......"],
}

FULL, UP, DOWN = '█', '▀', '▄'   # 0xDB, 0xDF, 0xDC


def draw_word(r0, word, color_of):
    """Render `word` centered at row r0, 5 rows tall, plus a floor shadow on
    row r0+5. color_of(index, letter) -> fg color for that letter."""
    widths = [len(FONT[ch][0]) for ch in word]
    total = sum(widths) + (len(word) - 1)  # 1 col gap between letters
    c = (W - total) // 2
    for i, ch in enumerate(word):
        glyph = FONT[ch]
        fg = color_of(i, ch)
        for gr, row in enumerate(glyph):
            for gc, px in enumerate(row):
                if px != '#':
                    continue
                above = gr > 0 and glyph[gr - 1][gc] == '#'
                below = gr < 4 and glyph[gr + 1][gc] == '#'
                # Chamfer isolated top/bottom stroke ends with half blocks.
                if not above and below:
                    g = DOWN
                elif above and not below:
                    g = UP
                else:
                    g = FULL
                put(r0 + gr, c + gc, g, fg)
        # Floor shadow: a shade line offset one to the right, under the
        # letter's bottom-row pixels (keeps letter counters clean).
        for gc, px in enumerate(glyph[4]):
            if px == '#':
                put(r0 + 5, c + gc + 1, '░', BLACK)  # 0xB0 light shade
        c += widths[i] + 1


# ---------------------------------------------------------------------------
# The console shelf: all eight machines as mini 6x3 boxes in their systems.rs
# theme colors, with a slug label underneath — the club's whole rack.
# ---------------------------------------------------------------------------
# (label, frame color, 2-char interior badge, badge color)
SHELF = [
    ("GG",   BBLUE,  "▒▒", BCYAN),   # blue handheld, bright screen
    ("SMS",  BRED,   "▪▪", WHITE),   # red-on-white grid
    ("SG1K", YELLOW, "▬▬", YELLOW),  # navy + gold, 1983
    ("GEN",  BRED,   "16", YELLOW),  # black shell, gold 16-BIT badge
    ("NES",  GRAY,   "▪▪", BRED),    # grey box, red logo
    ("SNES", BMAG,   "()", WHITE),   # purple buttons
    ("GBA",  BMAG,   "▒▒", BCYAN),   # indigo shell, teal accent
    ("PCE",  BROWN,  "∙─", YELLOW),  # black shell, orange badge
]


def draw_shelf(r0):
    """Eight mini machines (6 wide, 3 tall) + a label row at r0+3."""
    n = len(SHELF)
    box_w, gap = 6, 2
    total = n * box_w + (n - 1) * gap
    c0 = (W - total) // 2
    for i, (label, frame, badge, badge_fg) in enumerate(SHELF):
        c = c0 + i * (box_w + gap)
        text(r0, c, "┌────┐", frame, BLACK)
        text(r0 + 1, c, "│    │", frame, BLACK)
        text(r0 + 1, c + 2, badge, badge_fg, BLACK)
        text(r0 + 2, c, "└────┘", frame, BLACK)
        text(r0 + 3, c + (box_w - len(label)) // 2, label, frame)


# ---------------------------------------------------------------------------
# Compose the screen.
# ---------------------------------------------------------------------------

# Rows 0-6: starfield garnish (hidden on 24/25-row terminals, that's fine).
rng = random.Random(1989)
for _ in range(60):
    r, c = rng.randrange(0, 7), rng.randrange(1, W - 1)
    ch = rng.choice(['∙', '·', '.', '∙'])
    fg = rng.choice([BBLUE, BCYAN, CYAN, DGRAY])
    if cells[r][c][0] == ' ':
        put(r, c, ch, fg)

ctext(6, "H M D E R D O K   P R E S E N T S", BCYAN)

# Rows 8-12: the wordmark. LAME white, GEAR bright blue, + bright red.
def word_color(i, ch):
    if ch == '+':
        return BRED
    return WHITE if i < 4 else BBLUE

draw_word(8, "LAMEGEAR+", word_color)

# Row 14: tagline, flanked by rule lines out to the edges.
TAG = "◄ SEGA ∙ NINTENDO ∙ NEC ∙ ONE BBS DOOR ►"
tx = (W - len(TAG)) // 2
text(14, 2, "─" * (tx - 3), BBLUE)
ctext(14, TAG, BCYAN)
text(14, tx + len(TAG) + 1, "─" * (W - 2 - (tx + len(TAG) + 1)), BBLUE)

# Rows 16-19: the whole rack — all eight machines, labels underneath.
draw_shelf(16)

# Row 21: feature strip.
ctext(21, "8 consoles ∙ 2P netplay game room ∙ per-user saves ∙ cheats", CYAN)

# Row 23: how multiplayer works, in one line.
ctext(23, "walk up to a machine ∙ open your 2P port ∙ friends plug in", DGRAY)

# Rows 25-27: press-any-key box (35 wide: the 25-char message pads 4+4).
BW = 35
bx = (W - BW) // 2
text(25, bx, "╔" + "═" * (BW - 2) + "╗", BBLUE)
text(26, bx, "║" + " " * (BW - 2) + "║", BBLUE)
text(27, bx, "╚" + "═" * (BW - 2) + "╝", BBLUE)
ctext(26, "► PRESS ANY KEY TO PLAY ◄", WHITE)
put(26, (W - 25) // 2, '►', YELLOW)
put(26, (W - 25) // 2 + 24, '◄', YELLOW)

# Row 29: auto-continue hint.
ctext(29, "continues automatically in 10 seconds", CYAN)

# Row 31: signature.
ctext(31, "── lamegear+ ∙ github.com/hmderdoc ──", CYAN)


# ---------------------------------------------------------------------------
# Encode + SAUCE + preview.
# ---------------------------------------------------------------------------

# Glyphs Python's cp437 codec maps to C0 controls (or lacks); mirrors the
# special cases in src/cp437.rs.
EXTRA = {'►': 0x10, '◄': 0x11, '▲': 0x1E, '▼': 0x1F, '▬': 0x16,
         '▪': 0xFE, '■': 0xFE}


def encode():
    body = bytearray()
    for r in range(H):
        for c in range(W):
            ch, fg, bg = cells[r][c]
            body.append(EXTRA.get(ch) or ch.encode('cp437')[0])
            body.append(((bg & 7) << 4) | (fg & 15))
    return bytes(body)


def sauce(body_len):
    rec = bytearray(b'\x1aSAUCE00')
    rec += b'LameGear+ splash'.ljust(35)[:35]          # Title
    rec += b'hmderdoc'.ljust(20)[:20]                  # Author
    rec += b''.ljust(20)[:20]                          # Group
    rec += b'20260709'                                 # Date
    rec += struct.pack('<I', body_len)                 # FileSize
    rec += bytes([5, 40])                              # DataType 5, FileType 40 (80 wide)
    rec += struct.pack('<HHHH', 0, 0, 0, 0)            # TInfo1-4
    rec += bytes([0, 0])                               # Comments, TFlags
    rec += b''.ljust(22, b'\x00')                      # TInfoS
    assert len(rec) == 129
    return bytes(rec)


ANSI_FG = [30, 34, 32, 36, 31, 35, 33, 37]


def preview(data):
    out = []
    for r in range(H):
        line, last = [], None
        for c in range(W):
            o = (r * W + c) * 2
            ch, attr = data[o], data[o + 1]
            fg, bg = attr & 15, (attr >> 4) & 7
            sgr = ('1;' if fg >= 8 else '0;') + str(ANSI_FG[fg & 7]) + ';' + str(ANSI_FG[bg] + 10)
            if sgr != last:
                line.append('\x1b[' + sgr + 'm')
                last = sgr
            rev = {v: k for k, v in EXTRA.items()}
            if ch in rev:
                line.append(rev[ch])
            else:
                line.append(bytes([ch if ch else 32]).decode('cp437'))
        out.append(''.join(line) + '\x1b[0m')
    print('\n'.join(out))


def main():
    default_out = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                               '..', 'lamegear_splash.bin')
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--out', default=os.path.normpath(default_out))
    ap.add_argument('--preview', action='store_true',
                    help='print the generated screen as ANSI to stdout')
    args = ap.parse_args()

    body = encode()
    assert len(body) == W * H * 2
    with open(args.out, 'wb') as f:
        f.write(body)
        f.write(sauce(len(body)))
    if args.preview:
        preview(body)
    print(f'wrote {args.out} ({len(body)} body + SAUCE)', file=sys.stderr)


if __name__ == '__main__':
    main()
