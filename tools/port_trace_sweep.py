#!/usr/bin/env python3
"""Sweep the ROM library through `--port-trace` and write the session-shape
cache (`roms/.link-shapes`) that the door consults when a challenge starts
(design spec 4.4.7 — replaces static scanning, which can't see bank-switched
or computed-address code).

Usage: python3 tools/port_trace_sweep.py [--frames 900] [--roms roms]
Prints a compatibility table and writes <roms>/.link-shapes (file<TAB>shape).
"""
import argparse, subprocess, sys
from pathlib import Path

BASE = Path(__file__).resolve().parent.parent


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--roms", default=str(BASE / "roms"))
    ap.add_argument("--frames", type=int, default=900,
                    help="frames per ROM (900 = 15 emulated seconds)")
    ap.add_argument("--binary", default=str(BASE / "lamegear"))
    args = ap.parse_args()

    roms_dir = Path(args.roms)
    roms = sorted(
        p for p in roms_dir.iterdir()
        if p.suffix.lower() in (".gg", ".sms", ".sg") and p.is_file()
    )
    if not roms:
        print("no SMS/GG/SG ROMs found", file=sys.stderr)
        return 1

    results = {}
    counts = {"gear-to-gear": 0, "shared-console": 0, "single-player": 0, "error": 0}
    for i, rom in enumerate(roms, 1):
        try:
            out = subprocess.run(
                [args.binary, "--port-trace", str(rom), str(args.frames)],
                capture_output=True, text=True, timeout=120,
            )
            shape = "error"
            for line in out.stdout.splitlines():
                if line.startswith("SHAPE "):
                    shape = line.split()[1]
                    break
        except subprocess.TimeoutExpired:
            shape = "error"
        results[rom.name] = shape
        counts[shape] = counts.get(shape, 0) + 1
        print(f"[{i}/{len(roms)}] {shape:<14} {rom.name}", flush=True)

    cache = roms_dir / ".link-shapes"
    with open(cache, "w") as f:
        for name, shape in sorted(results.items()):
            if shape != "error":
                f.write(f"{name}\t{shape}\n")

    print("\n=== summary ===")
    for shape, n in sorted(counts.items()):
        print(f"{shape:<16} {n}")
    print(f"\nwrote {cache}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
