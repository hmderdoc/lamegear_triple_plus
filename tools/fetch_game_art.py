#!/usr/bin/env python3
"""Fetch only the Libretro artwork matching locally installed GG/SMS/SG-1000 ROMs.

The runtime door is deliberately offline. This sysop tool scans ROM filenames,
downloads matching PNGs into art/gg, art/sms and art/sg, and records misses.
Existing files are left alone unless --force is supplied.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import fnmatch
import os
from pathlib import Path
import re
import sys
import time
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen


# Anchor the ROM and art directories to the door root (this file lives in
# <door>/tools/), NOT the current working directory. Running from tools/ used to
# send art into tools/art — a dead end the door never reads — so the defaults are
# resolved from __file__ and are correct no matter where the script is invoked.
DOOR_ROOT = Path(__file__).resolve().parent.parent

# ROM extension -> libretro-thumbnails repository. The canonical form of the
# key doubles as the art/<system> output subdirectory that src/art.rs looks in.
REPOSITORIES = {
    "gg": "libretro-thumbnails/Sega_-_Game_Gear",
    "sms": "libretro-thumbnails/Sega_-_Master_System_-_Mark_III",
    "sg": "libretro-thumbnails/Sega_-_SG-1000",
    "md": "libretro-thumbnails/Sega_-_Mega_Drive_-_Genesis",
    "gen": "libretro-thumbnails/Sega_-_Mega_Drive_-_Genesis",
    "nes": "libretro-thumbnails/Nintendo_-_Nintendo_Entertainment_System",
    "sfc": "libretro-thumbnails/Nintendo_-_Super_Nintendo_Entertainment_System",
    "smc": "libretro-thumbnails/Nintendo_-_Super_Nintendo_Entertainment_System",
    "gba": "libretro-thumbnails/Nintendo_-_Game_Boy_Advance",
    "pce": "libretro-thumbnails/NEC_-_PC_Engine_-_TurboGrafx_16",
}
# Alias extensions that share another extension's art directory.
CANONICAL = {"gen": "md", "smc": "sfc"}
KINDS = {
    "boxart": "Named_Boxarts",
    "snap": "Named_Snaps",
    "title": "Named_Titles",
}
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
INVALID_THUMBNAIL_CHARS = frozenset('&*/:`"<>?\\|')


def thumbnail_name(title: str) -> str:
    """Apply RetroArch's invalid-title-character substitution."""
    return "".join("_" if c in INVALID_THUMBNAIL_CHARS else c for c in title)


def discover_roms(roms_dir: Path, system: str, pattern: str | None) -> list[tuple[str, Path]]:
    found: list[tuple[str, Path]] = []
    if not roms_dir.is_dir():
        raise FileNotFoundError(f"ROM directory does not exist: {roms_dir}")
    for path in roms_dir.iterdir():
        ext = path.suffix.lower().removeprefix(".")
        if not path.is_file() or ext not in REPOSITORIES:
            continue
        if system != "all" and ext != system:
            continue
        if pattern and not fnmatch.fnmatch(path.stem.lower(), pattern.lower()):
            continue
        found.append((ext, path))
    return sorted(found, key=lambda item: item[1].name.lower())


def source_url(system: str, kind_dir: str, title: str) -> str:
    filename = quote(f"{thumbnail_name(title)}.png", safe="")
    repo = REPOSITORIES[system]
    return f"https://raw.githubusercontent.com/{repo}/master/{kind_dir}/{filename}"


# Region tokens that identify the "region group" of a No-Intro name, and the
# common region tags to probe when a ROM's exact region set has no thumbnail.
_REGIONS = frozenset(
    {"USA", "Europe", "Japan", "World", "Australia", "Korea", "Brazil", "France", "Germany", "Spain", "Italy"}
)
_REGION_SETS = ("(USA)", "(Europe)", "(World)", "(USA, Europe)", "(Japan, USA)", "(Japan)", "(Brazil)")
_PAREN = re.compile(r"\s*\(([^()]*)\)")


def candidate_titles(title: str) -> list[str]:
    """Ordered, de-duplicated title variants to try when the exact No-Intro name
    has no thumbnail. Covers the common libretro naming gaps: fewer/no revision &
    enhancement tags, and a different region tag than the local ROM carries."""
    out: list[str] = []

    def add(candidate: str) -> None:
        candidate = candidate.strip()
        if candidate and candidate not in out:
            out.append(candidate)

    add(title)  # exact first — the fast path for the vast majority
    groups = [m.group(1) for m in _PAREN.finditer(title)]
    base = _PAREN.sub("", title).strip()

    # Progressive trailing-tag strip: "... (USA) (Rev 1) (Card Catcher)"
    # -> "... (USA) (Rev 1)" -> ... -> "... (USA)".
    for keep in range(len(groups) - 1, 0, -1):
        add(base + " " + " ".join(f"({g})" for g in groups[:keep]))

    # Region variants: if the first tag looks like a region set, try each single
    # region and World, then a base with common region sets.
    if groups:
        parts = [p.strip() for p in groups[0].split(",")]
        if any(p in _REGIONS for p in parts):
            for part in parts:
                add(f"{base} ({part})")
            add(f"{base} (World)")
    for region in _REGION_SETS:
        add(f"{base} {region}")
    add(base)  # bare title — a few libretro entries carry no region at all

    return out[:16]  # bound the per-miss request fan-out


def build_candidates(system: str, title: str, strict: bool) -> list[tuple[str, str]]:
    """(repo_system, title) attempts in priority order. Each title is tried in the
    ROM's own system repo first, then the other Sega repos (many Master System
    games shipped Game Gear ports under the same name and vice versa, and art is
    sometimes filed only under one system)."""
    if strict:
        return [(system, title)]
    others = [s for s in REPOSITORIES if s != system]
    seen: set[tuple[str, str]] = set()
    ordered: list[tuple[str, str]] = []
    for cand in candidate_titles(title):
        for repo_system in (system, *others):
            key = (repo_system, cand)
            if key not in seen:
                seen.add(key)
                ordered.append(key)
    return ordered


def download_png(url: str, timeout: float) -> tuple[str, object]:
    """Fetch one thumbnail URL. Returns ("png", bytes) on success, ("missing",
    None) on 404, or ("error", message) otherwise. Follows libretro's Git-symlink
    aliases (a 200 whose body is just the real filename)."""
    current_url = url
    for _alias_depth in range(4):
        request = Request(current_url, headers={"User-Agent": "lamegear-art-fetcher/1.0"})
        last_error = "download failed"
        data: bytes | None = None
        for attempt in range(3):
            try:
                with urlopen(request, timeout=timeout) as response:
                    data = response.read()
                break
            except HTTPError as error:
                if error.code == 404:
                    return ("missing", None)
                last_error = f"HTTP {error.code}"
            except (URLError, TimeoutError, OSError) as error:
                last_error = str(error)
            if attempt < 2:
                time.sleep(0.5 * (attempt + 1))
        if data is None:
            return ("error", last_error)
        if data.startswith(PNG_SIGNATURE):
            return ("png", data)

        # Some Libretro entries are Git symlinks whose raw payload is the target
        # filename (usually a region/revision-neutral box). Follow that local
        # alias explicitly; urllib cannot infer it from a 200 text response.
        try:
            alias = data.decode("utf-8").strip()
        except UnicodeDecodeError:
            alias = ""
        if (
            alias
            and Path(alias).name == alias
            and alias.lower().endswith(".png")
            and len(alias) <= 255
        ):
            current_url = f"{current_url.rsplit('/', 1)[0]}/{quote(alias, safe='')}"
            continue
        return ("error", "response was neither a PNG nor a local alias")
    return ("error", "too many artwork aliases")


def fetch_one(
    item: tuple[str, Path],
    output_dir: Path,
    kind_dir: str,
    force: bool,
    timeout: float,
    dry_run: bool,
    strict: bool,
) -> tuple[str, str, str]:
    system, rom = item
    title = rom.stem
    # The art is always saved under the ROM's OWN system + exact stem, so the door
    # finds it — even when the bytes came from a title variant or another repo.
    # Alias extensions (.gen) share their canonical system's directory (.md).
    destination = output_dir / CANONICAL.get(system, system) / f"{thumbnail_name(title)}.png"
    if destination.is_file() and not force:
        return ("cached", rom.name, str(destination))

    candidates = build_candidates(system, title, strict)
    if dry_run:
        repo_system, cand_title = candidates[0]
        return ("planned", rom.name, source_url(repo_system, kind_dir, cand_title))

    saw_error = False
    last_detail = "no matching thumbnail (exact or fallback)"
    for repo_system, cand_title in candidates:
        url = source_url(repo_system, kind_dir, cand_title)
        status, payload = download_png(url, timeout)
        if status == "png":
            assert isinstance(payload, bytes)
            destination.parent.mkdir(parents=True, exist_ok=True)
            temporary = destination.with_suffix(".png.part")
            temporary.write_bytes(payload)
            os.replace(temporary, destination)
            detail = str(destination)
            if (repo_system, cand_title) != (system, title):
                detail += f"  (via {repo_system}:{cand_title})"
            return ("fetched", rom.name, detail)
        if status == "missing":
            last_detail = url
        else:  # transient/other error — remember it, keep trying other candidates
            saw_error = True
            last_detail = str(payload)

    if saw_error:
        return ("error", rom.name, last_detail)
    return ("missing", rom.name, last_detail)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Fetch Libretro art matching the ROMs installed for LameGear+."
    )
    parser.add_argument(
        "--roms", type=Path, default=DOOR_ROOT / "roms", help="ROM directory"
    )
    parser.add_argument(
        "--output", type=Path, default=DOOR_ROOT / "art", help="local art cache"
    )
    parser.add_argument(
        "--kind", choices=KINDS, default="boxart", help="artwork type (default: boxart)"
    )
    parser.add_argument(
        "--system",
        choices=("all", "gg", "sms", "sg", "md", "gen", "nes"),
        default="all",
        help="limit platform",
    )
    parser.add_argument(
        "--match", metavar="GLOB", help="case-insensitive ROM-title glob, e.g. '*Sonic*'"
    )
    parser.add_argument("--workers", type=int, default=8, help="parallel downloads (1-32)")
    parser.add_argument("--timeout", type=float, default=20.0, help="per-request timeout")
    parser.add_argument("--limit", type=int, help="process only the first N matching ROMs")
    parser.add_argument("--force", action="store_true", help="replace cached files")
    parser.add_argument("--dry-run", action="store_true", help="print URLs without downloading")
    parser.add_argument(
        "--strict",
        action="store_true",
        help="exact name only; disable the cross-system + name-variant fallback",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if not 1 <= args.workers <= 32:
        print("error: --workers must be between 1 and 32", file=sys.stderr)
        return 2
    if args.limit is not None and args.limit < 1:
        print("error: --limit must be positive", file=sys.stderr)
        return 2

    try:
        roms = discover_roms(args.roms, args.system, args.match)
    except FileNotFoundError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    if args.limit is not None:
        roms = roms[: args.limit]
    if not roms:
        print("No matching .gg/.sms/.sg ROMs found.")
        return 0

    print(
        f"Scanning {len(roms)} ROMs; source={KINDS[args.kind]}, "
        f"cache={args.output}"
    )
    results: list[tuple[str, str, str]] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as executor:
        futures = [
            executor.submit(
                fetch_one,
                item,
                args.output,
                KINDS[args.kind],
                args.force,
                args.timeout,
                args.dry_run,
                args.strict,
            )
            for item in roms
        ]
        for future in concurrent.futures.as_completed(futures):
            result = future.result()
            results.append(result)
            status, name, detail = result
            # Name every download and miss so a run over freshly-added ROMs is
            # visibly doing work; only already-cached hits stay quiet.
            if status in {"fetched", "missing", "error", "planned"}:
                print(f"{status:7} {name}: {detail}")

    results.sort(key=lambda result: result[1].lower())
    missing = [name for status, name, _ in results if status == "missing"]
    errors = [name for status, name, _ in results if status == "error"]
    if not args.dry_run:
        args.output.mkdir(parents=True, exist_ok=True)
        manifest = args.output / "missing.txt"
        manifest.write_text("\n".join(missing) + ("\n" if missing else ""), encoding="utf-8")
        error_manifest = args.output / "errors.txt"
        error_manifest.write_text("\n".join(errors) + ("\n" if errors else ""), encoding="utf-8")

    counts = {status: 0 for status in ("fetched", "cached", "missing", "error", "planned")}
    for status, _, _ in results:
        counts[status] += 1
    print(" ".join(f"{key}={value}" for key, value in counts.items() if value))
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
