#!/usr/bin/env bash
# Package one built target into dist/ for the release.
# Usage: package.sh <label> <rust-target> <version>
set -eu

LABEL="$1"
TARGET="$2"
VERSION="$3"

# Keep the release tag and Cargo.toml in lockstep: the binary reports
# CARGO_PKG_VERSION, so a tag that disagrees would ship a build claiming the
# wrong version. Fail the release early (bump Cargo.toml before tagging).
TAG_VERSION="${VERSION#v}"
CARGO_VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
if [ -n "$TAG_VERSION" ] && [ "$TAG_VERSION" != "$CARGO_VERSION" ]; then
  echo "release version mismatch: tag ${VERSION} (=> ${TAG_VERSION}) != Cargo.toml ${CARGO_VERSION}" >&2
  echo "bump Cargo.toml (and Cargo.lock) to ${TAG_VERSION}, commit, then re-tag." >&2
  exit 1
fi

EXT=""
case "$TARGET" in
  *windows*) EXT=".exe" ;;
esac

DOOR="target/${TARGET}/release/lamegear${EXT}"
RELAY="target/${TARGET}/release/gg-link-server${EXT}"
if [ ! -f "$DOOR" ]; then
  echo "build artifact not found: $DOOR" >&2
  exit 1
fi

NAME="lamegear_${VERSION}_${LABEL}"
OUT="dist/${NAME}"
mkdir -p "$OUT"

cp "$DOOR" "$OUT/"
# The netplay relay ships prebuilt alongside the door (one relay can serve
# a whole network of BBSes — and lameboy doors too, same wire protocol).
[ -f "$RELAY" ] && cp "$RELAY" "$OUT/" || true

# Docs, sample config, and the splash screen the door loads at runtime.
for f in README.md LICENSE NOTICE lamegear.ini.example lamegear_splash.bin; do
  [ -f "$f" ] && cp "$f" "$OUT/" || true
done

# Runtime-populated directories + sysop tooling.
mkdir -p "$OUT/roms" "$OUT/art" "$OUT/tools"
[ -f roms/CREDITS.txt ] && cp roms/CREDITS.txt "$OUT/roms/" || true
[ -f art/README.md ] && cp art/README.md "$OUT/art/" || true
cp tools/*.py tools/*.sh tools/*.manifest "$OUT/tools/" 2>/dev/null || true

# Relay source + unit for sysops building their own.
if [ -d link-server ]; then
  mkdir -p "$OUT/link-server/src"
  for f in link-server/Cargo.toml link-server/Cargo.lock link-server/gg-link-server.service; do
    [ -f "$f" ] && cp "$f" "$OUT/link-server/" || true
  done
  cp link-server/src/*.rs "$OUT/link-server/src/"
fi

cd dist
case "$TARGET" in
  *windows*) zip -r "${NAME}.zip" "$NAME" ;;
  *)         tar czf "${NAME}.tar.gz" "$NAME" ;;
esac
echo "packaged dist/${NAME}.*"
