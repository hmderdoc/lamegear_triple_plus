#!/bin/sh
# Golden-frame regression check (design spec 4.4.6). Run after ANY vendored
# core change: every test cart's frame-300 framebuffer CRC must match
# tools/golden.manifest exactly. Regenerate the manifest deliberately (and
# eyeball --dump output) when a core bump intentionally changes rendering:
#   for c in roms/Test\ Cart.*; do ./lamegear --golden "$c" 300; done > tools/golden.manifest
cd "$(dirname "$0")/.." || exit 2
BIN=${1:-./lamegear}
FAIL=0
for cart in "Test Cart.sms" "Test Cart.gg" "Test Cart.md" "Test Cart.nes" "Test Cart.sfc" "Test Cart.gba" "Test Cart.pce"; do
    "$BIN" --golden "roms/$cart" 300
done > /tmp/golden.check.$$ 2>&1
if diff -u tools/golden.manifest /tmp/golden.check.$$; then
    echo "golden frames OK"
else
    echo "GOLDEN FRAME MISMATCH — a core change altered rendering" >&2
    FAIL=1
fi
rm -f /tmp/golden.check.$$
exit $FAIL
