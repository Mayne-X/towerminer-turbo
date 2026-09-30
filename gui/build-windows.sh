#!/usr/bin/env bash
# Cross-compile towerminer-gui.exe (x86_64-pc-windows-gnu) in Docker.
#
#   gui/build-windows.sh            release exe      -> gui/out/towerminer-gui.exe
#   gui/build-windows.sh --test     + unit-test exe  -> gui/out/gui-tests.exe
#                                   + test double    -> gui/out/fake-towerminer.exe
#
# Needs Docker and a rustup toolchain on the host (mounted into the container;
# the windows-gnu target is added to it if missing).
# Env: JOBS (cargo -j, default 8), DOCKER (default "docker", e.g. "sudo -n docker"),
#      CACHE (cargo registry + target dir, default gui/.docker-cache),
#      RUSTUP_HOME / CARGO_BIN (host toolchain, default ~/.rustup, ~/.cargo/bin).
set -euo pipefail

GUI="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMG="towerminer-gui-builder:1"
TARGET="x86_64-pc-windows-gnu"
JOBS="${JOBS:-8}"
DOCKER="${DOCKER:-docker}"
CACHE="${CACHE:-$GUI/.docker-cache}"
HOST_RUSTUP="${RUSTUP_HOME:-$HOME/.rustup}"
HOST_CARGO_BIN="${CARGO_BIN:-$HOME/.cargo/bin}"
TEST=0
[ "${1:-}" = "--test" ] && TEST=1

[ -x "$HOST_CARGO_BIN/cargo" ] || { echo "no cargo in $HOST_CARGO_BIN (install rustup)"; exit 1; }
mkdir -p "$CACHE" "$GUI/out"
if ! $DOCKER image inspect "$IMG" >/dev/null 2>&1; then
  echo "==> building image $IMG"
  $DOCKER build -t "$IMG" - < "$GUI/Dockerfile.windows"
fi

LOCKED=""
[ -f "$GUI/Cargo.lock" ] && LOCKED="--locked"

$DOCKER run --rm -u "$(id -u):$(id -g)" \
  -v "$GUI":/src -v "$CACHE":/cache \
  -v "$HOST_RUSTUP":/rustup -v "$HOST_CARGO_BIN":/cargo-bin:ro \
  -e RUSTUP_HOME=/rustup -e CARGO_HOME=/cache/cargo -e CARGO_TARGET_DIR=/cache/target -e HOME=/cache \
  -e PATH=/cargo-bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
  -w /src "$IMG" bash -euo pipefail -c "
    rustup target list --installed | grep -qx $TARGET || rustup target add $TARGET
    rustc --version
    cargo build --release $LOCKED -j $JOBS --target $TARGET
    install -m755 /cache/target/$TARGET/release/towerminer-gui.exe out/towerminer-gui.exe
    if [ $TEST = 1 ]; then
      cargo build --release $LOCKED -j $JOBS --target $TARGET --example fake_towerminer
      install -m755 /cache/target/$TARGET/release/examples/fake_towerminer.exe out/fake-towerminer.exe
      exe=\$(cargo test --release $LOCKED -j $JOBS --target $TARGET --no-run 2>&1 \
             | tee /dev/stderr | sed -n 's/.*Executable unittests src\/main.rs (\(.*\.exe\)).*/\1/p' | tail -1)
      install -m755 \"\$exe\" out/gui-tests.exe
    fi
  "
ls -l "$GUI/out"
sha256sum "$GUI/out/"*.exe
