#!/usr/bin/env bash
# Runs cargo inside the pinned builder image.
#
# There is no host toolchain; the caches live inside the bind-mounted repo so
# they are owned by the invoking user rather than by root.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec docker run --rm \
  -v "$repo:/work" -w /work \
  -e CARGO_HOME=/work/.build-cache/cargo \
  -e RUSTUP_HOME=/work/.build-cache/rustup \
  -e CARGO_TARGET_DIR=/work/.build-cache/target \
  -u "$(id -u):$(id -g)" \
  ph-reactor-builder:1.83-alpine \
  cargo "$@"
