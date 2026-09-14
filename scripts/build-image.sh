#!/usr/bin/env bash
# Build the ph-reactor container image from the same static musl binary the
# release workflow ships. Usage: scripts/build-image.sh [tag]
#
# The musl build needs a musl C toolchain, because `ring` compiles C. If the
# host has musl-gcc (CI installs musl-tools) it is used directly. Otherwise the
# cargo build runs inside rust:alpine -- Alpine is natively musl, so no
# cross-compilation is involved and no root on the host is required.
set -euo pipefail

TAG="${1:-ph-reactor:dev}"
cd "$(dirname "$0")/.."

TARGET=x86_64-unknown-linux-musl

if command -v musl-gcc >/dev/null 2>&1; then
  echo "==> building on the host (musl-gcc found)"
  rustup target add "$TARGET"
  CC_x86_64_unknown_linux_musl="${CC_x86_64_unknown_linux_musl:-musl-gcc}" \
    cargo build --release --locked --target "$TARGET"
  BIN="target/$TARGET/release/ph-reactor"
else
  echo "==> no musl-gcc on the host; building inside rust:alpine"
  # musl-dev is baked into a builder image rather than apk-added at run time,
  # because the build container runs as the invoking user (so nothing it writes
  # into the repo ends up root-owned) and an unprivileged user cannot apk add.
  docker build -q -t ph-reactor-builder:1.83-alpine - <<'BUILDER' >/dev/null
FROM rust:1.83-alpine
RUN apk add --no-cache musl-dev
BUILDER

  # Caches live INSIDE the bind-mounted repo rather than in their own volumes:
  # a separate -v mount whose host path does not exist yet is created by the
  # daemon as root, which an unprivileged build container then cannot write to.
  mkdir -p .build-cache/cargo .build-cache/rustup target/docker
  docker run --rm \
    -u "$(id -u):$(id -g)" \
    -v "$PWD:/src" -w /src \
    -e CARGO_HOME=/src/.build-cache/cargo \
    -e RUSTUP_HOME=/src/.build-cache/rustup \
    -e CARGO_TARGET_DIR=/src/target/docker \
    ph-reactor-builder:1.83-alpine \
    cargo build --release --locked
  BIN="target/docker/release/ph-reactor"
fi

mkdir -p dist
cp "$BIN" dist/ph-reactor
chmod 755 dist/ph-reactor

# A dynamically linked binary would fail at runtime on distroless/static, and
# the failure mode is an exec error with no useful message.
#
# Test the property, not one spelling of it: a musl release build reports
# "static-pie linked" rather than "statically linked", and both are static.
# What must never appear is "dynamically linked".
if command -v file >/dev/null 2>&1; then
  DESC="$(file -b dist/ph-reactor)"
  case "$DESC" in
    *"dynamically linked"*)
      echo "FAIL: dist/ph-reactor is dynamically linked; it cannot run on distroless/static" >&2
      echo "  $DESC" >&2
      exit 1 ;;
    *"static-pie linked"*|*"statically linked"*)
      echo "==> verified: static ($DESC)" ;;
    *)
      echo "FAIL: could not determine linkage of dist/ph-reactor" >&2
      echo "  $DESC" >&2
      exit 1 ;;
  esac
fi

docker build -t "$TAG" .
echo "built $TAG"
