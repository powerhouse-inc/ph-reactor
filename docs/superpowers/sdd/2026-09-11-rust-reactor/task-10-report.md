# Task 10 report — Packaging (snap/brew/CI)

**Done:**
- `apps/ph-reactor/snap/snapcraft.yaml`: core24, strict, static musl
  binary installed via a nil part (CI builds it into `dist/`), session
  autostart `run --daemonize` with `PH_REACTOR_STATE_DIR=
  $SNAP_USER_DATA/ph-reactor`, plugs home/network/network-bind/dbus
  (dbus = session bus for the SNI tray).
- `.github/workflows/rust-reactor.yml`: on tag `ph-reactor-v*` —
  stable Rust + musl target, release build, `--version` smoke, sha256
  sidecar, GitHub release with assets.

**Deviations:** Homebrew formula deferred to a follow-up (the release
binary is sufficient for manual install; documented in the README).
Snap store publishing is a manual step from the release assets (the
build itself needs an LXD host; not run in CI).
