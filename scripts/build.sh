#!/bin/bash
# Release build + release gate for towerminer. Run from anywhere on the build
# machine; writes RELEASE.txt next to the binary.
#
#   scripts/build.sh            # cargo from PATH or ~/.cargo/bin
#
# Refuses: a vendor/ tree that differs from VENDOR.sha256, a mutant build,
# a gate or nonce-accounting failure, a failing unit test.
set -euo pipefail
export LC_ALL=C
cd "$(dirname "$0")/.."
CARGO=${CARGO:-$(command -v cargo || echo "$HOME/.cargo/bin/cargo")}
TARGET=${CARGO_TARGET_DIR:-target}
BIN=$TARGET/release/towerminer
OUT=$TARGET/release/RELEASE.txt

(cd vendor && sha256sum -c --quiet ../VENDOR.sha256) || { echo "FATAL: vendor/ differs from VENDOR.sha256"; exit 1; }
"$CARGO" build --release
VER=$("$BIN" --version)
case "$VER" in *mutant*) echo "FATAL: $VER is a mutant build; refusing to release it"; exit 1 ;; esac
SHA=$(sha256sum "$BIN" | cut -d' ' -f1)
GLIBC=$(objdump -T "$BIN" | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' | sort -Vu | tail -1)

echo "== gate"
"$BIN" --gate --gate-random 300 > "$TARGET/release/gate.log" 2>&1 || { tail -5 "$TARGET/release/gate.log"; echo "FATAL: gate"; exit 1; }
GATE=$(grep -E '^GATE' "$TARGET/release/gate.log")
echo "== nonce accounting (both policies of this machine)"
NC1=$("$BIN" --no-tune-file --policy hashrate --check-nonces 5 2>/dev/null | grep CHECK-NONCES) || { echo "FATAL: check-nonces hashrate"; exit 1; }
NC2=$("$BIN" --no-tune-file --policy efficiency --check-nonces 5 2>/dev/null | grep CHECK-NONCES) || { echo "FATAL: check-nonces efficiency"; exit 1; }
echo "== unit tests"
TESTS=$("$CARGO" test --release -p towerminer 2>&1 | grep -E '^test result' | tail -1)
case "$TESTS" in *" 0 failed"*) ;; *) echo "FATAL: tests: $TESTS"; exit 1 ;; esac

cat > "$OUT" <<EOT
$VER
sha256   $SHA
glibc    $GLIBC (highest versioned symbol the binary needs; it runs on this glibc or newer)
rustc    $(rustc --version 2>/dev/null || "$HOME/.cargo/bin/rustc" --version)
built    $(date -u +%FT%TZ) on $(hostname) ($(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ *//'))
gate     $GATE (256 golden + 300 random, every kernel shape, pipe chain)
nonces   $NC1
nonces   $NC2
tests    $TESTS
EOT
cat "$OUT"
