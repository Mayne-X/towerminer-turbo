#!/usr/bin/env bash
# Build dist/towerminer-gui-<version>-windows-x86_64.zip and record it in
# dist/SHA256SUMS (the line of the same file name is replaced, others kept).
#
# Zip content (flat): towerminer-gui.exe (gui/out/, from build-windows.sh),
# towerminer.exe and README-GUI.txt.
# towerminer.exe comes from $TOWERMINER_EXE, else from
# dist/towerminer-<version>-windows-x86_64.zip. Without it the zip is not built
# unless ALLOW_NO_MINER=1.
set -euo pipefail

GUI="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST="$GUI/../dist"
VER="$(grep -m1 '^version' "$GUI/Cargo.toml" | cut -d'"' -f2)"
NAME="towerminer-gui-$VER-windows-x86_64"
MINER_ZIP="$DIST/towerminer-$VER-windows-x86_64.zip"

[ -f "$GUI/out/towerminer-gui.exe" ] || { echo "missing gui/out/towerminer-gui.exe (run build-windows.sh)"; exit 1; }
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

if [ -n "${TOWERMINER_EXE:-}" ]; then
  cp "$TOWERMINER_EXE" "$stage/towerminer.exe"
elif [ -f "$MINER_ZIP" ]; then
  member="$(unzip -Z1 "$MINER_ZIP" | grep -E '(^|/)towerminer\.exe$' | head -1)"
  [ -n "$member" ] || { echo "no towerminer.exe inside $MINER_ZIP"; exit 1; }
  unzip -p "$MINER_ZIP" "$member" > "$stage/towerminer.exe"
elif [ "${ALLOW_NO_MINER:-0}" = "1" ]; then
  echo "WARNING: packaging without towerminer.exe"
else
  echo "no towerminer.exe: set TOWERMINER_EXE or provide $MINER_ZIP (or ALLOW_NO_MINER=1)"; exit 1
fi

cp "$GUI/out/towerminer-gui.exe" "$stage/"
sed 's/$/\r/' "$GUI/README-GUI.txt" > "$stage/README-GUI.txt"   # CRLF for Notepad
mkdir -p "$DIST"
rm -f "$DIST/$NAME.zip"
(cd "$stage" && zip -X -9 "$DIST/$NAME.zip" ./*)

sum="$(cd "$DIST" && sha256sum "$NAME.zip")"
touch "$DIST/SHA256SUMS"
{ grep -v "  $NAME.zip\$" "$DIST/SHA256SUMS" || true; echo "$sum"; } > "$DIST/SHA256SUMS.tmp"
mv "$DIST/SHA256SUMS.tmp" "$DIST/SHA256SUMS"
unzip -l "$DIST/$NAME.zip"
echo "$sum"
