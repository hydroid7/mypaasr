#!/bin/sh
# Builds a statically linked release binary (x86_64, musl) in a container, so
# the host needs only Docker. The result runs on any Linux server, whatever its
# glibc version: target-musl/x86_64-unknown-linux-musl/release/mypaas
set -eu

cd "$(dirname "$0")/.."
IMAGE="rust:1.96-alpine"
OUT="target-musl"
mkdir -p "$OUT/cargo-home"

docker run --rm \
  -v "$PWD:/src" -w /src \
  -e CARGO_HOME=/src/$OUT/cargo-home \
  -e CARGO_TARGET_DIR=/src/$OUT \
  "$IMAGE" sh -c '
    set -e
    apk add --no-cache musl-dev gcc g++ make cmake perl >/dev/null
    cargo build --release --locked --target x86_64-unknown-linux-musl
    chown -R '"$(id -u):$(id -g)"' /src/'"$OUT"'
  '

BIN="$OUT/x86_64-unknown-linux-musl/release/mypaas"
file "$BIN" | grep -qE "static(-pie)? linked" || { echo "error: $BIN is not statically linked" >&2; exit 1; }
ls -lh "$BIN"
